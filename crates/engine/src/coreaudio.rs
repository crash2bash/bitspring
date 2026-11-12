//! macOS output through the CoreAudio HAL, talking to the device directly.
//!
//! The path to the DAC, and what each step buys us:
//!   1. **Hog mode** — exclusive access: no other app or system sound gets mixed in, and
//!      nobody else can change the device's rate behind our back.
//!   2. **Nominal sample rate = source rate** — the device clock follows each track, so the
//!      HAL never resamples.
//!   3. **Physical format** — the widest integer format the DAC offers at that rate
//!      (e.g. 24 or 32 bit), so a 24-bit source isn't truncated to 16 on the wire.
//!   4. **A raw IOProc** — we write float32 straight into the device's buffers, with no
//!      AudioUnit graph (and its converters) in between.

use std::ffi::{CStr, c_void};
use std::mem::{MaybeUninit, size_of};
use std::ops::Deref;
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use coreaudio_sys as ca;

use crate::channels::Speaker;
use crate::decode::Track;
use crate::output::{self, OutBuf, OutFormat, PlayOptions, Playback, Shared, SignalPath, StreamFormat, Transport};
use crate::rate::RateRange;

// We call CFStringGetCString/CFRelease directly; coreaudio-sys doesn't link CoreFoundation.
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {}

const MAIN: u32 = ca::kAudioObjectPropertyElementMain as u32;
const GLOBAL: u32 = ca::kAudioObjectPropertyScopeGlobal as u32;
const OUTPUT: u32 = ca::kAudioObjectPropertyScopeOutput as u32;
const NO_HOG: i32 = -1;

// ---------------------------------------------------------------------------------------------
// Property plumbing
// ---------------------------------------------------------------------------------------------

fn addr(selector: u32, scope: u32) -> ca::AudioObjectPropertyAddress {
    ca::AudioObjectPropertyAddress { mSelector: selector, mScope: scope, mElement: MAIN }
}

/// CoreAudio errors are usually four-char codes like `'!dat'`; show them that way.
fn fourcc(status: i32) -> String {
    let b = status.to_be_bytes();
    if b.iter().all(|c| c.is_ascii_graphic()) {
        format!("'{}'", String::from_utf8_lossy(&b))
    } else {
        status.to_string()
    }
}

fn check(status: ca::OSStatus, what: &str) -> Result<()> {
    if status == 0 { Ok(()) } else { bail!("{what}: OSStatus {}", fourcc(status)) }
}

fn get<T: Copy>(obj: u32, selector: u32, scope: u32) -> Result<T> {
    let a = addr(selector, scope);
    let mut value = MaybeUninit::<T>::uninit();
    let mut size = size_of::<T>() as u32;
    // SAFETY: the buffer is exactly `size` bytes; CoreAudio writes at most that much.
    let st = unsafe {
        ca::AudioObjectGetPropertyData(obj, &a, 0, ptr::null(), &mut size, value.as_mut_ptr().cast())
    };
    check(st, "get property")?;
    if size as usize != size_of::<T>() {
        bail!("property size mismatch: got {size}, expected {}", size_of::<T>());
    }
    // SAFETY: CoreAudio filled all `size_of::<T>()` bytes (checked above).
    Ok(unsafe { value.assume_init() })
}

fn get_vec<T: Copy>(obj: u32, selector: u32, scope: u32) -> Result<Vec<T>> {
    let a = addr(selector, scope);
    let mut size = 0u32;
    // SAFETY: plain out-parameter.
    check(unsafe { ca::AudioObjectGetPropertyDataSize(obj, &a, 0, ptr::null(), &mut size) }, "get size")?;
    let mut v: Vec<T> = Vec::with_capacity(size as usize / size_of::<T>());
    // SAFETY: capacity covers `size` bytes; we only expose what CoreAudio reports as written.
    unsafe {
        check(
            ca::AudioObjectGetPropertyData(obj, &a, 0, ptr::null(), &mut size, v.as_mut_ptr().cast()),
            "get property list",
        )?;
        v.set_len(size as usize / size_of::<T>());
    }
    Ok(v)
}

fn set<T>(obj: u32, selector: u32, scope: u32, value: &T) -> Result<()> {
    let a = addr(selector, scope);
    // SAFETY: `value` is a valid T for the duration of the call.
    let st = unsafe {
        ca::AudioObjectSetPropertyData(
            obj,
            &a,
            0,
            ptr::null(),
            size_of::<T>() as u32,
            (value as *const T).cast(),
        )
    };
    check(st, "set property")
}

fn get_string(obj: u32, selector: u32) -> Result<String> {
    let cf: ca::CFStringRef = get(obj, selector, GLOBAL)?;
    if cf.is_null() {
        return Ok(String::new());
    }
    let mut buf = [0 as std::os::raw::c_char; 512];
    // SAFETY: `cf` is a CFString we own (Copy-rule getter), released right after use.
    let ok = unsafe {
        let ok = ca::CFStringGetCString(cf, buf.as_mut_ptr(), buf.len() as _, ca::kCFStringEncodingUTF8);
        ca::CFRelease(cf as ca::CFTypeRef);
        ok
    };
    if ok == 0 {
        bail!("CFString not representable in 512 bytes of UTF-8");
    }
    // SAFETY: CFStringGetCString NUL-terminates on success.
    Ok(unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------------------------


fn transport_from_raw(t: u32) -> Transport {
    use Transport::*;
    match t {
        x if x == ca::kAudioDeviceTransportTypeBuiltIn as u32 => BuiltIn,
        x if x == ca::kAudioDeviceTransportTypeUSB as u32 => Usb,
        x if x == ca::kAudioDeviceTransportTypeBluetooth as u32 => Bluetooth,
        x if x == ca::kAudioDeviceTransportTypeHDMI as u32 => Hdmi,
        x if x == ca::kAudioDeviceTransportTypeDisplayPort as u32 => DisplayPort,
        x if x == ca::kAudioDeviceTransportTypeThunderbolt as u32 => Thunderbolt,
        x if x == ca::kAudioDeviceTransportTypeAirPlay as u32 => AirPlay,
        x if x == ca::kAudioDeviceTransportTypeAggregate as u32 => Aggregate,
        x if x == ca::kAudioDeviceTransportTypeVirtual as u32 => Virtual,
        x => Other(x),
    }
}

fn format_from_asbd(d: &ca::AudioStreamBasicDescription) -> StreamFormat {
    StreamFormat {
        sample_rate: d.mSampleRate,
        bits: d.mBitsPerChannel,
        float: d.mFormatFlags & ca::kAudioFormatFlagIsFloat as u32 != 0,
        channels: d.mChannelsPerFrame,
        non_mixable: d.mFormatFlags & ca::kAudioFormatFlagIsNonMixable as u32 != 0,
    }
}

/// An AudioChannelLayout, as the 32-bit words CoreAudio hands over: the speakers in device
/// channel order, when it says.
fn parse_channel_layout(words: &[u32]) -> Option<Vec<Speaker>> {
    use Speaker::*;
    let (&tag, &bitmap, &n) = (words.first()?, words.get(1)?, words.get(2)?);
    const USE_DESCRIPTIONS: u32 = 0;
    const USE_BITMAP: u32 = 1 << 16;
    let label = |l: u32| match l {
        1 => FrontLeft,
        2 => FrontRight,
        3 => FrontCenter,
        4 => Lfe,
        // CoreAudio's "surround" pair is 5.1's surrounds and 7.1's side pair; its rear pair is 7.1's back.
        5 | 10 => SideLeft,
        6 | 11 => SideRight,
        7 => FrontLeftCenter,
        8 => FrontRightCenter,
        9 => BackCenter,
        33 => BackLeft,
        34 => BackRight,
        _ => Other,
    };
    match tag {
        USE_DESCRIPTIONS => {
            let d: Vec<Speaker> = words.get(3..)?.chunks_exact(5).take(n as usize).map(|d| label(d[0])).collect();
            (d.len() == n as usize).then_some(d)
        }
        USE_BITMAP => Some(crate::channels::from_mask(bitmap as u64)),
        _ => {
            // The few named layouts devices use (tag = kind << 16 | channel count).
            let v: &[Speaker] = match tag >> 16 {
                100 => &[FrontCenter],
                101 | 102 => &[FrontLeft, FrontRight],
                108 => &[FrontLeft, FrontRight, SideLeft, SideRight],
                121 => &[FrontLeft, FrontRight, FrontCenter, Lfe, SideLeft, SideRight],
                123 => &[FrontLeft, FrontCenter, FrontRight, SideLeft, SideRight, Lfe],
                128 => &[FrontLeft, FrontRight, FrontCenter, Lfe, SideLeft, SideRight, BackLeft, BackRight],
                _ => return None,
            };
            Some(v.to_vec())
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub device: Device,
    pub name: String,
    pub uid: String,
    pub transport: Transport,
    pub output_channels: u32,
    pub nominal_rate: f64,
    pub rates: Vec<RateRange>,
    pub is_default: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Device {
    pub id: ca::AudioObjectID,
}

impl Device {
    pub fn default_output() -> Result<Self> {
        let id = get::<u32>(
            ca::kAudioObjectSystemObject as u32,
            ca::kAudioHardwarePropertyDefaultOutputDevice as u32,
            GLOBAL,
        )?;
        Ok(Self { id })
    }

    pub fn info(&self) -> Result<DeviceInfo> {
        let default = Self::default_output().ok();
        let output_channels = self
            .output_streams()?
            .iter()
            .filter_map(|&s| get::<ca::AudioStreamBasicDescription>(s, ca::kAudioStreamPropertyVirtualFormat as u32, GLOBAL).ok())
            .map(|f| f.mChannelsPerFrame)
            .sum();
        Ok(DeviceInfo {
            device: *self,
            name: get_string(self.id, ca::kAudioObjectPropertyName as u32)?,
            uid: get_string(self.id, ca::kAudioDevicePropertyDeviceUID as u32).unwrap_or_default(),
            transport: transport_from_raw(
                get::<u32>(self.id, ca::kAudioDevicePropertyTransportType as u32, GLOBAL).unwrap_or(0),
            ),
            output_channels,
            nominal_rate: self.nominal_rate()?,
            rates: self.available_rates()?,
            is_default: default == Some(*self),
        })
    }

    pub fn nominal_rate(&self) -> Result<f64> {
        get::<f64>(self.id, ca::kAudioDevicePropertyNominalSampleRate as u32, GLOBAL)
    }

    pub fn available_rates(&self) -> Result<Vec<RateRange>> {
        Ok(get_vec::<ca::AudioValueRange>(self.id, ca::kAudioDevicePropertyAvailableNominalSampleRates as u32, GLOBAL)?
            .into_iter()
            .map(|r| RateRange { min: r.mMinimum, max: r.mMaximum })
            .collect())
    }

    /// Switch the device clock and wait until the hardware has actually followed —
    /// the property change is asynchronous, and starting IO too early plays at the old rate.
    pub fn set_nominal_rate(&self, hz: f64) -> Result<()> {
        if (self.nominal_rate()? - hz).abs() < 0.5 {
            return Ok(());
        }
        set(self.id, ca::kAudioDevicePropertyNominalSampleRate as u32, GLOBAL, &hz)?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if (self.nominal_rate()? - hz).abs() < 0.5 {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        bail!(crate::UserError::RateStuck { khz: hz / 1000.0, current: self.nominal_rate()? / 1000.0 })
    }

    fn output_streams(&self) -> Result<Vec<ca::AudioStreamID>> {
        get_vec::<ca::AudioStreamID>(self.id, ca::kAudioDevicePropertyStreams as u32, OUTPUT)
    }

    /// What each of the device's `channels` output channels feeds, from its speaker setup
    /// (Audio MIDI Setup → Configure Speakers, or the driver). A device that doesn't say is
    /// taken to be in the usual order (L R C LFE, then surrounds).
    pub fn speakers(&self, channels: usize) -> Vec<Speaker> {
        let said = get_vec::<u32>(self.id, ca::kAudioDevicePropertyPreferredChannelLayout as u32, OUTPUT)
            .ok()
            .and_then(|w| parse_channel_layout(&w));
        match said {
            Some(mut v) if v.iter().any(|&s| s != Speaker::Other) && v.len() <= channels => {
                v.resize(channels, Speaker::Other);
                v
            }
            _ => crate::channels::default_layout(channels.max(1)),
        }
    }

    /// Which device channels (1-based) carry left and right, as set in Audio MIDI Setup →
    /// Configure Speakers, or by the driver.
    pub fn preferred_stereo_channels(&self) -> Result<[u32; 2]> {
        get::<[u32; 2]>(self.id, ca::kAudioDevicePropertyPreferredChannelsForStereo as u32, OUTPUT)
    }

    /// The physical format the first output stream is running in right now.
    pub fn current_physical_format(&self) -> Result<Option<StreamFormat>> {
        let Some(&stream) = self.output_streams()?.first() else { return Ok(None) };
        let f = get::<ca::AudioStreamBasicDescription>(stream, ca::kAudioStreamPropertyPhysicalFormat as u32, GLOBAL)?;
        Ok(Some(format_from_asbd(&f)))
    }

    /// Every physical format the first output stream advertises, with the rates it covers.
    pub fn physical_formats(&self) -> Result<Vec<(StreamFormat, RateRange)>> {
        let Some(&stream) = self.output_streams()?.first() else { return Ok(vec![]) };
        Ok(get_vec::<ca::AudioStreamRangedDescription>(
            stream,
            ca::kAudioStreamPropertyAvailablePhysicalFormats as u32,
            GLOBAL,
        )?
        .iter()
        .map(|d| {
            let r = &d.mSampleRateRange;
            (format_from_asbd(&d.mFormat), RateRange { min: r.mMinimum, max: r.mMaximum })
        })
        .collect())
    }

    /// Pick and apply the widest integer PCM format each output stream offers at `hz`.
    /// Returns the format of the first stream (the one carrying channels 1/2).
    ///
    /// `exclusive`: we hold hog mode, so prefer the *non-mixable* variant — that's what makes
    /// the HAL hand us the integer format directly. Without hog mode, non-mixable would cut
    /// every other app off the device, so prefer mixable.
    pub fn apply_best_physical_format(&self, hz: f64, exclusive: bool) -> Result<Option<StreamFormat>> {
        let mut first = None;
        for stream in self.output_streams()? {
            let available = get_vec::<ca::AudioStreamRangedDescription>(
                stream,
                ca::kAudioStreamPropertyAvailablePhysicalFormats as u32,
                GLOBAL,
            )?;
            let best = available
                .iter()
                .filter(|d| d.mFormat.mFormatID == ca::kAudioFormatLinearPCM as u32)
                .filter(|d| {
                    let r = &d.mSampleRateRange;
                    RateRange { min: r.mMinimum, max: r.mMaximum }.contains(hz)
                })
                // Integer before float (that's what DAC chips take), then the most bits,
                // then the mixability that matches our access mode. Fully ordered, so the
                // pick never depends on the order the driver lists formats in.
                .max_by_key(|d| {
                    let f = format_from_asbd(&d.mFormat);
                    (!f.float, f.bits, f.non_mixable == exclusive)
                });
            let Some(best) = best else { continue };
            let mut asbd = best.mFormat;
            asbd.mSampleRate = hz;
            set(stream, ca::kAudioStreamPropertyPhysicalFormat as u32, GLOBAL, &asbd)?;
            let wanted = format_from_asbd(&asbd);
            first.get_or_insert(wait_for_physical_format(stream, wanted)?);
        }
        Ok(first)
    }

    /// Can this device carry DoP for a `dsd_rate` stream? Its clock must run at `dsd_rate / 16`
    /// and offer an integer format of at least 24 bits there. (Whether the DAC actually
    /// *decodes* DoP can't be queried — that's the user's per-device switch.)
    pub fn supports_dop(&self, dsd_rate: u32) -> bool {
        let hz = dsd_rate as f64 / 16.0;
        self.available_rates().is_ok_and(|r| r.iter().any(|r| r.contains(hz)))
            && self
                .physical_formats()
                .is_ok_and(|f| f.iter().any(|(f, r)| !f.float && f.bits >= 24 && r.contains(hz)))
    }

    /// The formats our IOProc will actually be handed (per output stream).
    fn virtual_formats(&self) -> Result<Vec<ca::AudioStreamBasicDescription>> {
        self.output_streams()?
            .into_iter()
            .map(|s| get(s, ca::kAudioStreamPropertyVirtualFormat as u32, GLOBAL))
            .collect()
    }

    /// Take exclusive ("hog") access. Released when the guard drops — or by the OS if the
    /// process dies, so a crash can't leave the DAC locked.
    pub fn hog(&self) -> Result<HogGuard> {
        let me = std::process::id() as i32;
        let owner: i32 = get(self.id, ca::kAudioDevicePropertyHogMode as u32, GLOBAL)?;
        if owner == me {
            return Ok(HogGuard { device: *self, owned: false });
        }
        if owner != NO_HOG {
            log::warn!("device is held exclusively by another process (pid {owner})");
            bail!(crate::UserError::DeviceBusy);
        }
        set(self.id, ca::kAudioDevicePropertyHogMode as u32, GLOBAL, &me)?;
        let owner: i32 = get(self.id, ca::kAudioDevicePropertyHogMode as u32, GLOBAL)?;
        if owner != me {
            log::warn!("hog mode request was not honoured (owner is now {owner})");
            bail!(crate::UserError::DeviceBusy);
        }
        Ok(HogGuard { device: *self, owned: true })
    }
}

pub struct HogGuard {
    device: Device,
    owned: bool,
}

impl Drop for HogGuard {
    fn drop(&mut self) {
        if self.owned {
            let _ = set(self.device.id, ca::kAudioDevicePropertyHogMode as u32, GLOBAL, &NO_HOG);
        }
    }
}

/// Physical-format changes are asynchronous: right after the set, the stream can still report
/// the old format (seen on a freshly plugged FiiO Q7: asked for 32-bit, read back 16-bit).
/// Poll until the hardware has really switched, or report what it settled on.
fn wait_for_physical_format(stream: ca::AudioStreamID, wanted: StreamFormat) -> Result<StreamFormat> {
    let read = || -> Result<StreamFormat> {
        Ok(format_from_asbd(&get::<ca::AudioStreamBasicDescription>(
            stream,
            ca::kAudioStreamPropertyPhysicalFormat as u32,
            GLOBAL,
        )?))
    };
    let matches = |f: &StreamFormat| {
        f.bits == wanted.bits
            && f.float == wanted.float
            && f.non_mixable == wanted.non_mixable
            && (f.sample_rate - wanted.sample_rate).abs() < 0.5
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let now = read()?;
        if matches(&now) {
            return Ok(now);
        }
        if Instant::now() >= deadline {
            log::warn!("physical format settled on {now:?}, wanted {wanted:?}");
            return Ok(now);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn output_devices() -> Result<Vec<DeviceInfo>> {
    let ids = get_vec::<u32>(ca::kAudioObjectSystemObject as u32, ca::kAudioHardwarePropertyDevices as u32, GLOBAL)?;
    Ok(ids
        .into_iter()
        .map(|id| Device { id })
        .filter(|d| d.output_streams().map(|s| !s.is_empty()).unwrap_or(false))
        .filter_map(|d| d.info().ok())
        .collect())
}

// Playback
// ---------------------------------------------------------------------------------------------

/// Sample layout of one output buffer, as the device's *virtual* format dictates: usually
/// float32, but a hogged USB DAC may expose its integer format directly ("integer mode").
fn out_format_from_virtual(d: &ca::AudioStreamBasicDescription) -> Result<OutFormat> {
    let f = d.mFormatFlags;
    let has = |flag: u32| f & flag != 0;
    if d.mFormatID != ca::kAudioFormatLinearPCM as u32
        || has(ca::kAudioFormatFlagIsBigEndian as u32)
        || has(ca::kAudioFormatFlagIsNonInterleaved as u32)
        || d.mChannelsPerFrame == 0
    {
        bail!("unsupported device virtual format: {:?} flags={f:#x}", format_from_asbd(d));
    }
    let bytes = d.mBytesPerFrame / d.mChannelsPerFrame;
    Ok(match (has(ca::kAudioFormatFlagIsFloat as u32), bytes, d.mBitsPerChannel) {
        (true, 4, 32) => OutFormat::F32,
        (false, 2, 16) => OutFormat::I16,
        (false, 3, 24) => OutFormat::I24Packed,
        (false, 4, b) if b == 32 || has(ca::kAudioFormatFlagIsAlignedHigh as u32) => OutFormat::I32 { bits: 32 },
        (false, 4, b) => OutFormat::I32 { bits: b },
        _ => bail!("unsupported device virtual format: {:?} flags={f:#x}", format_from_asbd(d)),
    })
}

/// Puts the device back the way the user had it before we touched it — physical format and
/// rate — on success *and* on any early error return from `Player::start`.
struct DeviceRestore {
    device: Device,
    hz: f64,
    formats: Vec<(ca::AudioStreamID, ca::AudioStreamBasicDescription)>,
}

impl DeviceRestore {
    fn capture(device: Device) -> Result<Self> {
        let formats = device
            .output_streams()?
            .into_iter()
            .filter_map(|s| {
                get::<ca::AudioStreamBasicDescription>(s, ca::kAudioStreamPropertyPhysicalFormat as u32, GLOBAL)
                    .ok()
                    .map(|f| (s, f))
            })
            .collect();
        Ok(Self { device, hz: device.nominal_rate()?, formats })
    }
}

impl Drop for DeviceRestore {
    fn drop(&mut self) {
        for (stream, mut asbd) in self.formats.drain(..) {
            // Never hand a non-mixable format back to the system — other apps couldn't play
            // through the device. (This also heals a device left that way by a crash.)
            asbd.mFormatFlags &= !(ca::kAudioFormatFlagIsNonMixable as u32);
            if set(stream, ca::kAudioStreamPropertyPhysicalFormat as u32, GLOBAL, &asbd).is_ok() {
                let _ = wait_for_physical_format(stream, format_from_asbd(&asbd));
            }
        }
        if (self.device.nominal_rate().unwrap_or(self.hz) - self.hz).abs() >= 0.5 {
            let _ = self.device.set_nominal_rate(self.hz);
        }
    }
}


/// What the IOProc gets as its user data.
struct IoContext {
    playback: Playback,
    /// One entry per output stream / buffer, fixed before IO starts.
    formats: Vec<OutFormat>,
}

pub struct Player {
    device: Device,
    proc_id: ca::AudioDeviceIOProcID,
    /// Boxed: the IOProc holds its address.
    io: Box<IoContext>,
    // Field order = drop order after Player::drop: rate restored first, then hog released.
    _restore: DeviceRestore,
    _hog: Option<HogGuard>,
    pub path: SignalPath,
}

impl Deref for Player {
    type Target = Playback;
    fn deref(&self) -> &Playback {
        &self.io.playback
    }
}

impl Player {
    pub fn start(device: Device, track: Arc<Track>, opts: &PlayOptions) -> Result<Self> {
        let info = device.info()?;
        let hog = if opts.exclusive { Some(device.hog()?) } else { None };
        let restore = DeviceRestore::capture(device)?;

        let dop_ok = track.dsd.is_some_and(|d| device.supports_dop(d.rate));
        let speakers = device.speakers(info.output_channels as usize);
        let plan = output::plan(&track, opts, &info.rates, &info.name, dop_ok, &speakers)?;

        // Physical format first: setting it can itself move the nominal rate.
        let physical = match device.apply_best_physical_format(plan.target, hog.is_some()) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("could not set physical format: {e}");
                None
            }
        };
        device.set_nominal_rate(plan.target)?;
        let device_rate = device.nominal_rate()?;

        let formats = device
            .virtual_formats()?
            .iter()
            .map(out_format_from_virtual)
            .collect::<Result<Vec<_>>>()?;
        let first = formats.first().copied().unwrap_or(OutFormat::F32);
        if plan.dop
            && ((device_rate - track.sample_rate as f64).abs() >= 0.5
                || !formats.iter().all(|f| f.carries_dop())
                || !physical.is_some_and(|p| !p.float && p.bits >= 24))
        {
            return Err(output::dop_error(&info.name, &track));
        }
        let path = output::signal_path(&track, opts, &plan, device_rate, physical, first, hog.is_some(), info.transport);
        let playback = Playback::new(track, opts, &plan, device_rate)?;
        let io = Box::new(IoContext { playback, formats });

        let mut proc_id: ca::AudioDeviceIOProcID = None;
        let client = &*io as *const IoContext as *mut c_void;
        // SAFETY: `io` is boxed (stable address) and outlives the IOProc: Drop stops and
        // destroys the proc before the box is freed.
        unsafe {
            check(ca::AudioDeviceCreateIOProcID(device.id, Some(io_proc), client, &mut proc_id), "create IOProc")?;
            if let Err(e) = check(ca::AudioDeviceStart(device.id, proc_id), "start device") {
                ca::AudioDeviceDestroyIOProcID(device.id, proc_id);
                return Err(e);
            }
        }

        Ok(Self { device, proc_id, io, _restore: restore, _hog: hog, path })
    }

    pub fn device(&self) -> Device {
        self.device
    }

    /// The device went away mid-stream. (CoreAudio keeps the IOProc alive; nothing to report.)
    pub fn output_error(&self) -> Option<crate::UserError> {
        None
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // SAFETY: proc_id came from AudioDeviceCreateIOProcID on this device.
        unsafe {
            ca::AudioDeviceStop(self.device.id, self.proc_id);
            ca::AudioDeviceDestroyIOProcID(self.device.id, self.proc_id);
        }
        // Then `io` (joins the render thread), `_restore`, `_hog`, in field order.
    }
}

/// Most output streams we serve on one device (more are left alone).
const MAX_BUFFERS: usize = 16;

/// The realtime callback. Runs on CoreAudio's IO thread at elevated priority: it must not
/// allocate, lock, log, or do anything with unbounded duration.
unsafe extern "C" fn io_proc(
    _device: ca::AudioObjectID,
    _now: *const ca::AudioTimeStamp,
    _input: *const ca::AudioBufferList,
    _input_time: *const ca::AudioTimeStamp,
    output: *mut ca::AudioBufferList,
    _output_time: *const ca::AudioTimeStamp,
    client: *mut c_void,
) -> ca::OSStatus {
    // SAFETY: `client` is the `IoContext` registered in Player::start, alive until the proc is
    // destroyed; `output` is a valid buffer list for the duration of this call, and each
    // buffer holds mDataByteSize writable bytes in the stream's virtual format.
    let (io, buffers) = unsafe {
        let list = &mut *output;
        (
            &*(client as *const IoContext),
            std::slice::from_raw_parts_mut(list.mBuffers.as_mut_ptr(), list.mNumberBuffers as usize),
        )
    };
    let mut it = buffers.iter_mut().zip(&io.formats);
    let mut bufs: [OutBuf<'_>; MAX_BUFFERS] = std::array::from_fn(|_| match it.next() {
        Some((b, &format)) if !b.mData.is_null() => OutBuf {
            // SAFETY: see above.
            data: unsafe { std::slice::from_raw_parts_mut(b.mData as *mut u8, b.mDataByteSize as usize) },
            channels: b.mNumberChannels as usize,
            format,
        },
        _ => OutBuf { data: &mut [], channels: 0, format: OutFormat::F32 },
    });
    let shared: &Shared = io.playback.shared();
    shared.render(&mut bufs);
    0
}

pub use crate::output::format_rates;

#[cfg(test)]
mod layout_tests {
    use super::*;
    use Speaker::*;

    #[test]
    fn speaker_setups_read_as_coreaudio_gives_them() {
        // Configure Speakers → 5.1: descriptions L R C LFE Ls Rs.
        let mut w = vec![0, 0, 6];
        for l in [1, 2, 3, 4, 5, 6] {
            w.extend([l, 0, 0, 0, 0]);
        }
        assert_eq!(parse_channel_layout(&w).unwrap(), [FrontLeft, FrontRight, FrontCenter, Lfe, SideLeft, SideRight]);
        // A bitmap (WAVE's bits), and a named layout (MPEG 5.1 C: L C R Ls Rs LFE).
        assert_eq!(parse_channel_layout(&[1 << 16, 0x3F, 0]).unwrap(), crate::channels::default_layout(6));
        assert_eq!(parse_channel_layout(&[(123 << 16) | 6, 0, 0]).unwrap()[1], FrontCenter);
        assert_eq!(parse_channel_layout(&[(999 << 16) | 6, 0, 0]), None);
        assert_eq!(parse_channel_layout(&[0, 0, 2, 1, 0, 0, 0, 0]), None, "short");
    }
}
