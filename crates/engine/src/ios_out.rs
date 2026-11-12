//! iPhone / iPad output: AVAudioSession picks the hardware rate, a RemoteIO audio unit pulls
//! the samples.
//!
//! iOS has one output route at a time (speaker, headphones, a USB DAC, Bluetooth…) and no
//! hog mode. What it does have:
//! - **The playback category without mixing**: other apps go quiet while we play, so nothing
//!   else is summed into our stream.
//! - **A preferred sample rate**: on a USB DAC iOS runs the DAC at the rate we ask for (when
//!   the DAC has it). We ask for the track's rate, read back what we got, and resample
//!   ourselves only if the answer differs.
//!
//! iOS's own pipeline is 32-bit float, which holds any 24-bit sample exactly. The one thing
//! that can still touch the samples is the system volume: below maximum, iOS may scale them
//! digitally (unless the DAC takes volume commands). [`system_volume`] lets the UI say so, and
//! DoP — where scaling turns DSD into loud noise — runs only at full volume, with a watchdog
//! that mutes within milliseconds if the volume moves or the route changes.

use std::collections::HashMap;
use std::ffi::c_void;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use coreaudio_sys as ca;
use objc2::rc::Retained;
use objc2_avf_audio::{
    AVAudioSession, AVAudioSessionCategoryPlayback, AVAudioSessionPortAirPlay, AVAudioSessionPortBluetoothA2DP,
    AVAudioSessionPortBluetoothHFP, AVAudioSessionPortBluetoothLE, AVAudioSessionPortBuiltInSpeaker,
    AVAudioSessionPortCarAudio, AVAudioSessionPortHDMI, AVAudioSessionPortHeadphones, AVAudioSessionPortLineOut,
    AVAudioSessionPortUSBAudio,
};
use objc2_foundation::NSString;

use crate::UserError;
use crate::decode::Track;
use crate::output::{self, OutBuf, OutFormat, PlayOptions, Playback, SignalPath, StreamFormat, Transport};
use crate::rate::RateRange;

/// Every common rate of both families, up to what iOS accepts from a USB DAC.
const USB_RATES: [f64; 8] = [44_100.0, 48_000.0, 88_200.0, 96_000.0, 176_400.0, 192_000.0, 352_800.0, 384_000.0];
/// Highest device rate we try DoP at (DSD256 → 352.8 kHz).
const MAX_DOP_RATE: u32 = 384_000;
/// Volume counts as "full" from here (the slider's float is not always exactly 1).
const FULL_VOLUME: f32 = 0.999;

fn session() -> Retained<AVAudioSession> {
    // SAFETY: the shared session is a process-wide singleton, safe from any thread.
    unsafe { AVAudioSession::sharedInstance() }
}

/// Playback category, no mixing with other apps. Set once; repeating it is harmless.
fn configure() -> Result<()> {
    let s = session();
    // SAFETY: plain property calls on the shared session; the category is a framework static.
    unsafe {
        let cat = AVAudioSessionCategoryPlayback.ok_or_else(|| anyhow!("AVAudioSession unavailable"))?;
        s.setCategory_error(cat).map_err(|e| UserError::AudioSetup { detail: e.to_string() })?;
    }
    Ok(())
}

/// The system volume (0…1) while a route is up. Below 1 iOS may scale the samples.
pub fn system_volume() -> Option<f32> {
    // SAFETY: plain property read.
    Some(unsafe { session().outputVolume() })
}

/// Why DSD can't go out as DoP right now, if something system-wide prevents it.
pub fn dop_blocker() -> Option<UserError> {
    let v = system_volume()?;
    (v < FULL_VOLUME).then_some(UserError::DopVolume)
}

struct Route {
    uid: String,
    name: String,
    transport: Transport,
    channels: u32,
}

fn is_port(port: &NSString, known: Option<&NSString>) -> bool {
    known.is_some_and(|k| k == port)
}

fn current_route() -> Option<Route> {
    let s = session();
    // SAFETY: plain property reads; the port-type constants are framework statics.
    unsafe {
        let route = s.currentRoute();
        let outputs = route.outputs();
        let port = outputs.firstObject()?;
        let kind = port.portType();
        let transport = if is_port(&kind, AVAudioSessionPortUSBAudio) {
            Transport::Usb
        } else if is_port(&kind, AVAudioSessionPortBluetoothA2DP)
            || is_port(&kind, AVAudioSessionPortBluetoothLE)
            || is_port(&kind, AVAudioSessionPortBluetoothHFP)
        {
            Transport::Bluetooth
        } else if is_port(&kind, AVAudioSessionPortAirPlay) {
            Transport::AirPlay
        } else if is_port(&kind, AVAudioSessionPortHDMI) {
            Transport::Hdmi
        } else if is_port(&kind, AVAudioSessionPortBuiltInSpeaker) || is_port(&kind, AVAudioSessionPortHeadphones) {
            Transport::BuiltIn
        } else if is_port(&kind, AVAudioSessionPortLineOut) || is_port(&kind, AVAudioSessionPortCarAudio) {
            Transport::Other(0)
        } else {
            Transport::Other(0)
        };
        let builtin_name = if is_port(&kind, AVAudioSessionPortBuiltInSpeaker) {
            Some("Speaker")
        } else if is_port(&kind, AVAudioSessionPortHeadphones) {
            Some("Headphones")
        } else {
            None
        };
        Some(Route {
            uid: port.UID().to_string(),
            name: builtin_name.map_or_else(|| port.portName().to_string(), str::to_string),
            transport,
            channels: (s.outputNumberOfChannels().max(1) as u32).max(port.channels().map_or(0, |c| c.count() as u32)),
        })
    }
}

/// Rates we have seen a route actually run at, by route UID. iOS can't list a DAC's rates
/// without switching to each of them, so we learn them as we play.
fn seen_rates() -> &'static Mutex<HashMap<String, Vec<f64>>> {
    static SEEN: OnceLock<Mutex<HashMap<String, Vec<f64>>>> = OnceLock::new();
    SEEN.get_or_init(Default::default)
}

/// DoP rates a route turned out not to take (asked for, got something else).
fn failed_dop() -> &'static Mutex<HashMap<String, Vec<u32>>> {
    static FAILED: OnceLock<Mutex<HashMap<String, Vec<u32>>>> = OnceLock::new();
    FAILED.get_or_init(Default::default)
}

fn remember_rate(uid: &str, hz: f64) {
    let mut g = seen_rates().lock().unwrap();
    let v = g.entry(uid.to_string()).or_default();
    if !v.iter().any(|r| (r - hz).abs() < 0.5) {
        v.push(hz);
        v.sort_by(f64::total_cmp);
    }
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub device: Device,
    pub name: String,
    pub uid: String,
    pub transport: Transport,
    pub output_channels: u32,
    /// The rate the route runs at now.
    pub nominal_rate: f64,
    /// Rates the route has run at (plus the current one).
    pub rates: Vec<RateRange>,
    pub is_default: bool,
}

/// The current output route, by its port UID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub uid: String,
}

impl Device {
    pub fn default_output() -> Result<Self> {
        let r = current_route().ok_or(UserError::NoDevice)?;
        Ok(Self { uid: r.uid })
    }

    pub fn info(&self) -> Result<DeviceInfo> {
        let r = current_route().filter(|r| r.uid == self.uid).ok_or(UserError::DeviceNotFound)?;
        // SAFETY: plain property read.
        let now = unsafe { session().sampleRate() };
        remember_rate(&r.uid, now);
        let rates = seen_rates().lock().unwrap().get(&r.uid).cloned().unwrap_or_default();
        Ok(DeviceInfo {
            device: self.clone(),
            uid: r.uid,
            name: r.name,
            transport: r.transport,
            output_channels: r.channels,
            nominal_rate: now,
            rates: rates.into_iter().map(RateRange::exact).collect(),
            is_default: true,
        })
    }

    /// DoP needs a USB DAC that runs at `dsd_rate / 16`. iOS can't be asked in advance, so a
    /// USB route is assumed able until a start proves otherwise.
    pub fn supports_dop(&self, dsd_rate: u32) -> bool {
        let dev_rate = dsd_rate / 16;
        current_route().is_some_and(|r| r.uid == self.uid && r.transport == Transport::Usb)
            && dev_rate <= MAX_DOP_RATE
            && !failed_dop().lock().unwrap().get(&self.uid).is_some_and(|v| v.contains(&dev_rate))
    }
}

pub fn output_devices() -> Result<Vec<DeviceInfo>> {
    let _ = configure();
    Ok(Device::default_output().and_then(|d| d.info()).into_iter().collect())
}

// ---------------------------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------------------------

/// What the render callback gets as its user data.
struct IoContext {
    playback: Playback,
    /// Write silence instead of audio (route changed, or volume moved during DoP).
    mute: AtomicBool,
}

pub struct Player {
    device: Device,
    unit: ca::AudioUnit,
    /// Boxed: the callback holds its address.
    io: Box<IoContext>,
    stop: Arc<AtomicBool>,
    watchdog: Option<JoinHandle<()>>,
    failure: Arc<Mutex<Option<UserError>>>,
    pub path: SignalPath,
}

// SAFETY: the AudioUnit handle is only used from `start` and `drop`; the callback reaches
// `io` through its own pointer.
unsafe impl Send for Player {}
unsafe impl Sync for Player {}

impl Deref for Player {
    type Target = Playback;
    fn deref(&self) -> &Playback {
        &self.io.playback
    }
}

fn check(status: ca::OSStatus, what: &str) -> Result<()> {
    if status == 0 { Ok(()) } else { bail!("{what}: OSStatus {status}") }
}

/// Ask iOS for `hz`, bring the session up, and return the rate the hardware actually runs at.
fn activate_at(hz: f64) -> Result<f64> {
    configure()?;
    let s = session();
    // SAFETY: plain property calls on the shared session.
    unsafe {
        if let Err(e) = s.setPreferredSampleRate_error(hz) {
            log::warn!("preferred rate {hz}: {e}");
        }
        // ~20 ms buffers: low enough for snappy seeks, long enough to survive a busy UI.
        let _ = s.setPreferredIOBufferDuration_error(0.02);
        s.setActive_error(true).map_err(|e| {
            log::warn!("setActive: {e}");
            UserError::AudioBusy
        })?;
        Ok(s.sampleRate())
    }
}

impl Player {
    pub fn start(device: Device, track: Arc<Track>, opts: &PlayOptions) -> Result<Self> {
        let info = device.info()?;
        let dop_ok = track.dsd.is_some_and(|d| device.supports_dop(d.rate)) && dop_blocker().is_none();
        // Plan against every rate a USB DAC might have; the session tells us what it took.
        let wide: Vec<RateRange> = if info.transport == Transport::Usb {
            USB_RATES.iter().copied().map(RateRange::exact).collect()
        } else {
            info.rates.clone()
        };
        let mut plan = output::plan(&track, opts, &wide, &info.name, dop_ok, &crate::channels::default_layout(2))?;
        let device_rate = activate_at(plan.target)?;
        remember_rate(&info.uid, device_rate);
        if (device_rate - plan.target).abs() >= 0.5 {
            if plan.dop {
                failed_dop().lock().unwrap().entry(info.uid.clone()).or_default().push(plan.target as u32);
                return Err(output::dop_error(&info.name, &track));
            }
            plan = output::plan(&track, opts, &[RateRange::exact(device_rate)], &info.name, false, &crate::channels::default_layout(2))?;
        }

        let channels = 2u32;
        let physical = StreamFormat { sample_rate: device_rate, bits: 32, float: true, channels, non_mixable: false };
        let path = output::signal_path(&track, opts, &plan, device_rate, Some(physical), OutFormat::F32, opts.exclusive, info.transport);
        let playback = Playback::new(track, opts, &plan, device_rate)?;
        let io = Box::new(IoContext { playback, mute: AtomicBool::new(false) });
        let unit = open_unit(&io, device_rate, channels)?;

        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let watchdog = {
            let (stop, failure, uid, dop) = (stop.clone(), failure.clone(), info.uid.clone(), plan.dop);
            let io_addr = &*io as *const IoContext as usize;
            std::thread::Builder::new()
                .name("hifi-route".into())
                .spawn(move || {
                    // SAFETY: `io` is boxed and outlives this thread: Player::drop joins it
                    // before the box is freed.
                    let io = unsafe { &*(io_addr as *const IoContext) };
                    watch(io, &uid, dop, &stop, &failure);
                })
                .ok()
        };
        Ok(Self { device, unit, io, stop, watchdog, failure, path })
    }

    pub fn device(&self) -> Device {
        self.device.clone()
    }

    /// The route changed under us (DAC unplugged, headphones pulled) — playback is paused —
    /// or, during DoP, the volume is below maximum (muted until it is back).
    pub fn output_error(&self) -> Option<UserError> {
        self.failure.lock().unwrap().clone()
    }
}

/// Watch the output while we play:
/// - The route changed (DAC unplugged, headphones pulled): pause — Apple's rule, and the UI
///   restarts the track on the new route, paused. DoP is also muted at once, so the new route
///   never gets a burst of DoP-as-PCM noise.
/// - DoP and the volume left maximum: iOS would scale the DoP words into noise, so mute until
///   it is back at maximum.
///
/// Polls: fast enough during DoP that a scaled stream never reaches the ear for more than a few ms.
fn watch(io: &IoContext, uid: &str, dop: bool, stop: &AtomicBool, failure: &Mutex<Option<UserError>>) {
    let every = Duration::from_millis(if dop { 5 } else { 40 });
    while !stop.load(Ordering::Acquire) {
        if current_route().is_none_or(|r| r.uid != uid) {
            if dop {
                io.mute.store(true, Ordering::Release);
            }
            io.playback.set_paused(true);
            *failure.lock().unwrap() = Some(UserError::DeviceUnplugged);
            return;
        }
        if dop {
            let low = system_volume().is_some_and(|v| v < FULL_VOLUME);
            if low != io.mute.load(Ordering::Acquire) {
                io.mute.store(low, Ordering::Release);
                *failure.lock().unwrap() = low.then_some(UserError::DopVolume);
            }
        }
        std::thread::sleep(every);
    }
}

fn open_unit(io: &IoContext, rate: f64, channels: u32) -> Result<ca::AudioUnit> {
    let desc = ca::AudioComponentDescription {
        componentType: ca::kAudioUnitType_Output,
        componentSubType: ca::kAudioUnitSubType_RemoteIO,
        componentManufacturer: ca::kAudioUnitManufacturer_Apple,
        componentFlags: 0,
        componentFlagsMask: 0,
    };
    let bytes = 4 * channels;
    let asbd = ca::AudioStreamBasicDescription {
        mSampleRate: rate,
        mFormatID: ca::kAudioFormatLinearPCM,
        mFormatFlags: ca::kAudioFormatFlagIsFloat | ca::kAudioFormatFlagIsPacked,
        mBytesPerPacket: bytes,
        mFramesPerPacket: 1,
        mBytesPerFrame: bytes,
        mChannelsPerFrame: channels,
        mBitsPerChannel: 32,
        mReserved: 0,
    };
    // Apple's figure for playback with the screen locked, when iOS stretches the buffers.
    let max_frames: u32 = 4096;
    let callback = ca::AURenderCallbackStruct {
        inputProc: Some(render_cb),
        inputProcRefCon: io as *const IoContext as *mut c_void,
    };
    // SAFETY: standard RemoteIO setup. The refcon points at the boxed `IoContext`, which
    // outlives the unit (Player::drop disposes of the unit first).
    unsafe {
        let comp = ca::AudioComponentFindNext(std::ptr::null_mut(), &desc);
        if comp.is_null() {
            bail!("RemoteIO unavailable");
        }
        let mut unit: ca::AudioUnit = std::ptr::null_mut();
        check(ca::AudioComponentInstanceNew(comp, &mut unit), "open RemoteIO")?;
        let set = |prop: u32, scope: u32, data: *const c_void, size: usize| {
            check(ca::AudioUnitSetProperty(unit, prop, scope, 0, data, size as u32), "configure RemoteIO")
        };
        let result = (|| {
            set(
                ca::kAudioUnitProperty_StreamFormat,
                ca::kAudioUnitScope_Input,
                &asbd as *const _ as *const c_void,
                size_of_val(&asbd),
            )?;
            set(
                ca::kAudioUnitProperty_MaximumFramesPerSlice,
                ca::kAudioUnitScope_Global,
                &max_frames as *const _ as *const c_void,
                size_of_val(&max_frames),
            )?;
            set(
                ca::kAudioUnitProperty_SetRenderCallback,
                ca::kAudioUnitScope_Input,
                &callback as *const _ as *const c_void,
                size_of_val(&callback),
            )?;
            check(ca::AudioUnitInitialize(unit), "initialize RemoteIO")?;
            check(ca::AudioOutputUnitStart(unit), "start RemoteIO")
        })();
        if let Err(e) = result {
            ca::AudioComponentInstanceDispose(unit);
            return Err(e);
        }
        Ok(unit)
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // SAFETY: the unit came from `open_unit`; after Stop the callback no longer runs.
        unsafe {
            ca::AudioOutputUnitStop(self.unit);
            ca::AudioUnitUninitialize(self.unit);
            ca::AudioComponentInstanceDispose(self.unit);
        }
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.watchdog.take() {
            let _ = t.join();
        }
        // Then `io` drops, joining the render thread. The session stays active, so the next
        // track starts without iOS re-routing.
    }
}

/// The realtime callback: no locks, no allocation, nothing of unbounded duration.
unsafe extern "C" fn render_cb(
    refcon: *mut c_void,
    _flags: *mut ca::AudioUnitRenderActionFlags,
    _time: *const ca::AudioTimeStamp,
    _bus: u32,
    _frames: u32,
    data: *mut ca::AudioBufferList,
) -> ca::OSStatus {
    // SAFETY: `refcon` is the `IoContext` set in `open_unit`, alive while the unit runs;
    // `data` is a valid interleaved buffer list for this call.
    let (io, buffers) = unsafe {
        let list = &mut *data;
        (&*(refcon as *const IoContext), std::slice::from_raw_parts_mut(list.mBuffers.as_mut_ptr(), list.mNumberBuffers as usize))
    };
    let Some(b) = buffers.first_mut() else { return 0 };
    if b.mData.is_null() {
        return 0;
    }
    // SAFETY: see above.
    let bytes = unsafe { std::slice::from_raw_parts_mut(b.mData as *mut u8, b.mDataByteSize as usize) };
    if io.mute.load(Ordering::Acquire) {
        bytes.fill(0);
        return 0;
    }
    io.playback.shared().render(&mut [OutBuf { data: bytes, channels: b.mNumberChannels as usize, format: OutFormat::F32 }]);
    0
}
