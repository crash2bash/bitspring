//! Linux output through ALSA.
//!
//! - **A sound card's own device** (`hw:CARD=…,DEV=…`) is Linux' bit-perfect path: we open
//!   the card directly, past PipeWire/PulseAudio, at the track's rate in the card's own
//!   integer format, so our samples reach the DAC unchanged. This is also what carries DoP.
//!   Only one app can hold such a device; PipeWire lets go of a card a few seconds after it
//!   last played something, so opening it is retried for a while before calling it busy.
//! - **System sound** (ALSA's `default`, i.e. PipeWire) mixes with every other app and plays
//!   wherever the system sends sound, Bluetooth included. It runs at the mixer's rate, so we
//!   resample to it ourselves.

use std::collections::HashMap;
use std::ffi::CString;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use anyhow::{Context, Result, anyhow, bail};

use crate::UserError;
use crate::channels::Speaker;
use crate::decode::Track;
use crate::output::{self, OutBuf, OutFormat, PlayOptions, Playback, SignalPath, StreamFormat, Transport};
use crate::rate::RateRange;

/// Rates worth probing: every common rate of both families.
const RATES: [u32; 10] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000];

/// Card formats in order of preference: the widest integer first (what DAC chips take).
/// S24_LE is 24 bits in the low end of a 32-bit word, S32_LE the full word.
const HW_FORMATS: [(Format, OutFormat); 5] = [
    (Format::S32LE, OutFormat::I32 { bits: 32 }),
    (Format::S243LE, OutFormat::I24Packed),
    (Format::S24LE, OutFormat::I32 { bits: 24 }),
    (Format::S16LE, OutFormat::I16),
    (Format::FloatLE, OutFormat::F32),
];

/// The rate PipeWire mixes at unless told otherwise.
const SYSTEM_RATE: u32 = 48_000;

/// How long to keep trying a card another app (PipeWire) still holds. PipeWire closes a card
/// 5 s after its last sound by default.
const BUSY_WAIT: Duration = Duration::from_secs(7);

const SYSTEM_UID: &str = "alsa:default";

/// An output: a card's PCM device, or the system mixer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Device {
    /// ALSA's `default` (PipeWire or PulseAudio, mixing with other apps).
    System,
    /// A card's own device, by the card's id string (stable across replugs, unlike its index)
    /// and the device number on the card.
    Hw { card: String, dev: u32 },
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub device: Device,
    pub name: String,
    pub uid: String,
    pub transport: Transport,
    pub output_channels: u32,
    /// The rate the device runs at when we don't choose (the mixer's, or the card's highest).
    pub nominal_rate: f64,
    /// Rates the device takes untouched.
    pub rates: Vec<RateRange>,
    pub is_default: bool,
}

/// What a card's device can do, found by opening it once. Kept, because the device can't be
/// opened again while we (or PipeWire) play on it.
#[derive(Debug, Clone)]
struct Caps {
    rates: Vec<u32>,
    max_channels: u32,
    /// An integer format of 24 bits or more exists (needed for DoP).
    wide_int: bool,
}

fn caps_cache() -> &'static Mutex<HashMap<String, Caps>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Caps>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn alsa_err(e: alsa::Error) -> anyhow::Error {
    match e.errno() {
        libc::EBUSY => UserError::DeviceBusy.into(),
        libc::ENOENT | libc::ENODEV => UserError::DeviceNotFound.into(),
        _ => anyhow!("ALSA: {e}"),
    }
}

impl Device {
    pub fn default_output() -> Result<Self> {
        Ok(Self::System)
    }

    fn uid(&self) -> String {
        match self {
            Self::System => SYSTEM_UID.into(),
            Self::Hw { card, dev } => format!("alsa:{card}:{dev}"),
        }
    }

    /// The ALSA name to open.
    fn pcm_name(&self) -> String {
        match self {
            Self::System => "default".into(),
            Self::Hw { card, dev } => format!("hw:CARD={card},DEV={dev}"),
        }
    }

    pub fn info(&self) -> Result<DeviceInfo> {
        let uid = self.uid();
        output_devices()?.into_iter().find(|d| d.uid == uid).ok_or_else(|| UserError::DeviceNotFound.into())
    }

    /// Can this device carry DoP for a `dsd_rate` stream: a card's own device at
    /// `dsd_rate / 16` with an integer format of at least 24 bits. (Whether the DAC decodes
    /// DoP can't be queried — that's the user's per-device switch.)
    pub fn supports_dop(&self, dsd_rate: u32) -> bool {
        let Self::Hw { .. } = self else { return false };
        // In its own statement, so the cache's lock is let go before `probe` takes it again.
        let cached = caps_cache().lock().unwrap().get(&self.uid()).cloned();
        let Some(caps) = cached.or_else(|| probe(self)) else { return false };
        caps.wide_int && caps.rates.contains(&(dsd_rate / 16))
    }
}

/// Open the device just to see what it can do (without blocking if another app has it).
fn probe(device: &Device) -> Option<Caps> {
    let pcm = PCM::new(&device.pcm_name(), Direction::Playback, true).ok()?;
    let hw = HwParams::any(&pcm).ok()?;
    hw.set_access(Access::RWInterleaved).ok()?;
    let rates: Vec<u32> = RATES.iter().copied().filter(|&r| hw.test_rate(r).is_ok()).collect();
    let wide_int = [Format::S32LE, Format::S243LE, Format::S24LE].iter().any(|&f| hw.test_format(f).is_ok());
    let max_channels = hw.get_channels_max().unwrap_or(2).clamp(1, 32);
    if rates.is_empty() {
        return None;
    }
    let caps = Caps { rates, max_channels, wide_int };
    caps_cache().lock().unwrap().insert(device.uid(), caps.clone());
    Some(caps)
}

/// A USB card's rates as the kernel lists them (`/proc/asound/cardN/stream0`): readable even
/// while another app has the card open.
fn usb_stream_caps(card_index: i32) -> Option<Caps> {
    let text = std::fs::read_to_string(format!("/proc/asound/card{card_index}/stream0")).ok()?;
    // Only the "Playback:" part: capture interfaces come after it.
    let playback = text.split("Capture:").next()?;
    let mut rates = Vec::new();
    let mut max_channels = 2;
    let mut wide_int = false;
    for line in playback.lines().map(str::trim) {
        if let Some(list) = line.strip_prefix("Rates:") {
            for r in list.split(',').filter_map(|r| r.trim().parse::<u32>().ok()) {
                if RATES.contains(&r) && !rates.contains(&r) {
                    rates.push(r);
                }
            }
        } else if let Some(c) = line.strip_prefix("Channels:") {
            max_channels = max_channels.max(c.trim().parse().unwrap_or(2));
        } else if let Some(f) = line.strip_prefix("Format:") {
            wide_int |= f.contains("S32_LE") || f.contains("S24_3LE") || f.contains("S24_LE");
        }
    }
    rates.sort_unstable();
    (!rates.is_empty()).then_some(Caps { rates, max_channels, wide_int })
}

fn exact(hz: u32) -> RateRange {
    RateRange { min: hz as f64, max: hz as f64 }
}

fn system_info(is_default: bool) -> DeviceInfo {
    DeviceInfo {
        device: Device::System,
        name: "System sound".into(),
        uid: SYSTEM_UID.into(),
        // PipeWire mixes: never bit-perfect, whatever it plays on.
        transport: Transport::Virtual,
        output_channels: 2,
        nominal_rate: SYSTEM_RATE as f64,
        rates: vec![exact(SYSTEM_RATE)],
        is_default,
    }
}

/// Is a sound server (PipeWire/PulseAudio) running? Then `default` goes through it.
fn sound_server() -> bool {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
    dir.is_some_and(|d| d.join("pipewire-0").exists() || d.join("pulse/native").exists())
}

pub fn output_devices() -> Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    let mut usb_seen = false;
    for card in alsa::card::Iter::new().filter_map(Result::ok) {
        let index = card.get_index();
        let Ok(ctl) = alsa::ctl::Ctl::from_card(&card, false) else { continue };
        let Ok(card_info) = ctl.card_info() else { continue };
        let card_id = card_info.get_id().unwrap_or_default().to_string();
        let card_name = card_info.get_name().unwrap_or_default().trim().to_string();
        let driver = card_info.get_driver().unwrap_or_default().to_string();
        let usb = driver == "USB-Audio" || std::path::Path::new(&format!("/proc/asound/card{index}/usbid")).exists();
        let devs: Vec<(u32, String)> = alsa::ctl::DeviceIter::new(&ctl)
            .filter_map(|dev| {
                let info = ctl.pcm_info(dev as u32, 0, Direction::Playback).ok()?;
                Some((dev as u32, info.get_name().unwrap_or_default().trim().to_string()))
            })
            .collect();
        let several = devs.len() > 1;
        for (dev, pcm_name) in devs {
            let device = Device::Hw { card: card_id.clone(), dev };
            let uid = device.uid();
            let lname = format!("{card_name} {pcm_name}").to_lowercase();
            let hdmi = lname.contains("hdmi") || lname.contains("displayport") || lname.contains(" dp");
            let transport = if usb {
                Transport::Usb
            } else if hdmi {
                Transport::Hdmi
            } else {
                Transport::BuiltIn
            };
            let cached = caps_cache().lock().unwrap().get(&uid).cloned();
            let caps = cached.or_else(|| probe(&device)).or_else(|| if usb { usb_stream_caps(index) } else { None });
            // An HDMI port with nothing plugged in (or a device that won't open) has no rates.
            let Some(caps) = caps else { continue };
            let name = match transport {
                Transport::Usb => card_name.clone(),
                Transport::BuiltIn => "Built-in speakers / headphones".into(),
                _ if several => format!("{card_name} — {pcm_name}"),
                _ => card_name.clone(),
            };
            usb_seen |= usb;
            out.push(DeviceInfo {
                device,
                uid,
                name: if name.is_empty() { "Audio device".into() } else { name },
                transport,
                output_channels: caps.max_channels.max(2),
                nominal_rate: *caps.rates.last().unwrap() as f64,
                rates: caps.rates.iter().copied().map(exact).collect(),
                is_default: false,
            });
        }
    }
    // Built-in "speakers / headphones" twice (the Deck's card has several devices) reads as
    // a duplicate: number them.
    let builtin = out.iter().filter(|d| d.transport == Transport::BuiltIn).count();
    if builtin > 1 {
        for (n, d) in out.iter_mut().filter(|d| d.transport == Transport::BuiltIn).enumerate() {
            d.name = format!("{} {}", d.name, n + 1);
        }
    }
    // Default: a USB DAC when one is plugged in (that's why it's plugged in), else the system.
    if sound_server() || out.is_empty() {
        out.insert(0, system_info(!usb_seen));
    }
    if let Some(d) = out.iter_mut().find(|d| d.transport == Transport::Usb) {
        d.is_default = true;
    } else if !out.iter().any(|d| d.is_default) {
        if let Some(d) = out.first_mut() {
            d.is_default = true;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------------------------

/// What the stream thread got the device to run in.
struct Negotiated {
    rate: f64,
    format: OutFormat,
    physical: StreamFormat,
    channels: usize,
    speakers: Option<Vec<Speaker>>,
}

pub struct Player {
    device: Device,
    playback: Arc<Playback>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Why the stream thread stopped early (device unplugged), if it did.
    failure: Arc<Mutex<Option<UserError>>>,
    pub path: SignalPath,
}

impl Deref for Player {
    type Target = Playback;
    fn deref(&self) -> &Playback {
        &self.playback
    }
}

impl Player {
    pub fn start(device: Device, track: Arc<Track>, opts: &PlayOptions) -> Result<Self> {
        let info = device.info()?;
        // A card's own device is always ours alone; the system mixer never is.
        let exclusive = matches!(device, Device::Hw { .. });
        let opts = &PlayOptions { exclusive, ..opts.clone() };
        let dop_ok = output::wire_dsd_rate(&track, opts).is_some_and(|r| device.supports_dop(r));
        let wide = exclusive && track.channels > 2 && info.output_channels > 2;
        let open_channels = if wide { info.output_channels as usize } else { 2 };
        let speakers = crate::channels::default_layout(open_channels);
        let mut plan = output::plan(&track, opts, &info.rates, &info.name, output::DsdSupport { dop: dop_ok, native: false }, &speakers)?;

        let (fmt_tx, fmt_rx) = mpsc::sync_channel::<Result<Negotiated>>(1);
        let (pb_tx, pb_rx) = mpsc::sync_channel::<Option<Arc<Playback>>>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let thread = {
            let (dev, target, dop) = (device.clone(), plan.target as u32, plan.dop);
            let (stop, failure) = (stop.clone(), failure.clone());
            std::thread::Builder::new().name("hifi-alsa".into()).spawn(move || {
                if let Err(e) = stream_thread(&dev, target, dop, open_channels, &fmt_tx, &pb_rx, &stop) {
                    // Before negotiation the error goes back to `start`; after, it's a failure.
                    let msg = UserError::of(&e);
                    if fmt_tx.try_send(Err(e)).is_err() {
                        *failure.lock().unwrap() = Some(msg);
                    }
                }
            })?
        };
        let neg = match fmt_rx.recv() {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                let _ = thread.join();
                bail!("audio thread ended unexpectedly");
            }
        };
        let abort = |pb_tx: &mpsc::SyncSender<Option<Arc<Playback>>>, thread: JoinHandle<()>| {
            let _ = pb_tx.send(None);
            let _ = thread.join();
        };
        if plan.dop && (!neg.format.carries_dop() || neg.format == OutFormat::F32 || (neg.rate - plan.target).abs() >= 0.5) {
            abort(&pb_tx, thread);
            return Err(output::dop_error(&info.name, (plan.target * 16.0).round() as u32));
        }
        // The card said where its channels go, or took fewer than asked: plan for what it has.
        let actual = neg.speakers.clone().unwrap_or_else(|| crate::channels::default_layout(neg.channels));
        if actual != speakers {
            match output::plan(&track, opts, &info.rates, &info.name, output::DsdSupport { dop: dop_ok, native: false }, &actual) {
                Ok(p) => plan = p,
                Err(e) => {
                    abort(&pb_tx, thread);
                    return Err(e);
                }
            }
        }
        let path = output::signal_path(&track, opts, &plan, neg.rate, Some(neg.physical), neg.format, exclusive, info.transport);
        let playback = match Playback::new(track, opts, &plan, neg.rate) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                abort(&pb_tx, thread);
                return Err(e);
            }
        };
        let _ = pb_tx.send(Some(playback.clone()));
        Ok(Self { device, playback, stop, thread: Some(thread), failure, path })
    }

    pub fn device(&self) -> Device {
        self.device.clone()
    }

    /// The device went away mid-stream (unplugged).
    pub fn output_error(&self) -> Option<UserError> {
        self.failure.lock().unwrap().clone()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // `playback` drops after this, joining the render thread.
    }
}

/// Open the PCM, waiting a while if PipeWire still holds the card.
fn open(device: &Device, stop: &AtomicBool) -> Result<PCM> {
    let name = CString::new(device.pcm_name())?;
    let deadline = Instant::now() + BUSY_WAIT;
    loop {
        match PCM::open(&name, Direction::Playback, false) {
            Ok(p) => return Ok(p),
            Err(e) if e.errno() == libc::EBUSY && Instant::now() < deadline && !stop.load(Ordering::Acquire) => {
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => return Err(alsa_err(e)),
        }
    }
}

fn speaker_of(p: alsa::pcm::ChmapPosition) -> Speaker {
    use alsa::pcm::ChmapPosition as P;
    match p {
        P::FL | P::Mono => Speaker::FrontLeft,
        P::FR => Speaker::FrontRight,
        P::FC => Speaker::FrontCenter,
        P::LFE => Speaker::Lfe,
        P::RL => Speaker::BackLeft,
        P::RR => Speaker::BackRight,
        P::FLC => Speaker::FrontLeftCenter,
        P::FRC => Speaker::FrontRightCenter,
        P::RC => Speaker::BackCenter,
        P::SL => Speaker::SideLeft,
        P::SR => Speaker::SideRight,
        _ => Speaker::Other,
    }
}

/// Set the hardware up: `target` Hz, the best format, `channels` (or what the card has).
fn configure(pcm: &PCM, exclusive: bool, target: u32, dop: bool, channels: usize) -> Result<(OutFormat, u32, usize)> {
    let hw = HwParams::any(pcm).map_err(alsa_err)?;
    hw.set_access(Access::RWInterleaved).map_err(alsa_err)?;
    // A card's device must not convert anything behind our back.
    if exclusive {
        let _ = hw.set_rate_resample(false);
    }
    let format = if exclusive {
        HW_FORMATS
            .iter()
            .filter(|(_, out)| !dop || (out.carries_dop() && *out != OutFormat::F32))
            .find(|(f, _)| hw.test_format(*f).is_ok())
            .copied()
            .ok_or(UserError::FormatRefused)?
    } else {
        (Format::FloatLE, OutFormat::F32)
    };
    hw.set_format(format.0).map_err(alsa_err)?;
    let channels = hw.set_channels_near(channels as u32).map_err(alsa_err)? as usize;
    if exclusive {
        hw.set_rate(target, ValueOr::Nearest).map_err(|_| UserError::RateRefused { khz: target as f64 / 1000.0 })?;
    } else {
        hw.set_rate_near(target, ValueOr::Nearest).map_err(alsa_err)?;
    }
    // ~200 ms of buffer in ~20 ms periods: safe on a busy handheld, quick enough for seeking.
    let _ = hw.set_buffer_time_near(200_000, ValueOr::Nearest);
    let _ = hw.set_period_time_near(20_000, ValueOr::Nearest);
    pcm.hw_params(&hw).map_err(alsa_err)?;
    let rate = pcm.hw_params_current().and_then(|h| h.get_rate()).map_err(alsa_err)?;
    Ok((format.1, rate, channels))
}

/// Ask the kernel for realtime scheduling (works when the user may: rtkit limits, audio
/// group); playback works without it, just with less margin under load.
fn realtime() {
    // SAFETY: plain syscall on the calling thread with a valid param struct.
    unsafe {
        let param = libc::sched_param { sched_priority: 20 };
        if libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) != 0 {
            let _ = libc::setpriority(libc::PRIO_PROCESS, 0, -11);
        }
    }
}

/// Open the device, negotiate the format, report it, then feed the device until `stop`.
fn stream_thread(
    device: &Device,
    target: u32,
    dop: bool,
    channels: usize,
    fmt_tx: &mpsc::SyncSender<Result<Negotiated>>,
    pb_rx: &mpsc::Receiver<Option<Arc<Playback>>>,
    stop: &AtomicBool,
) -> Result<()> {
    let exclusive = matches!(device, Device::Hw { .. });
    let pcm = open(device, stop)?;
    let (format, rate, channels) = configure(&pcm, exclusive, target, dop, channels)?;
    if exclusive && rate != target {
        bail!(UserError::RateRefused { khz: target as f64 / 1000.0 });
    }
    let speakers = pcm
        .get_chmap()
        .ok()
        .map(|m| Vec::<alsa::pcm::ChmapPosition>::from(&m).into_iter().map(speaker_of).collect::<Vec<_>>())
        .filter(|s| s.len() == channels && channels > 2);
    let physical = StreamFormat {
        sample_rate: rate as f64,
        bits: if format == OutFormat::F32 { 32 } else { format.bits() },
        float: format == OutFormat::F32,
        channels: channels as u32,
        non_mixable: exclusive,
    };
    if fmt_tx.send(Ok(Negotiated { rate: rate as f64, format, physical, channels, speakers })).is_err() {
        return Ok(());
    }
    let Ok(Some(playback)) = pb_rx.recv() else { return Ok(()) };
    realtime();

    let (_, period) = pcm.get_params().unwrap_or((0, 1024));
    let period = (period as usize).clamp(64, 16_384);
    let frame_bytes = channels * format.bytes();
    let mut scratch = vec![0u8; period * frame_bytes];
    let io = pcm.io_bytes();
    let mut stalls = 0;
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        playback.shared().render(&mut [OutBuf { data: &mut scratch, channels, format }]);
        let mut done = 0;
        while done < period {
            if stop.load(Ordering::Acquire) {
                break;
            }
            match io.writei(&scratch[done * frame_bytes..]) {
                Ok(n) => {
                    done += n;
                    stalls = 0;
                }
                // EPIPE (underrun) and ESTRPIPE (suspend): prepare and carry on.
                Err(e) if matches!(e.errno(), libc::EPIPE | libc::ESTRPIPE | libc::EINTR) => {
                    stalls += 1;
                    if stalls > 50 {
                        return Err(UserError::DeviceStalled.into());
                    }
                    pcm.try_recover(e, true).map_err(alsa_err).context(UserError::DeviceUnplugged)?;
                }
                Err(e) => return Err(alsa_err(e)).context(UserError::DeviceUnplugged),
            }
        }
    }
    let _ = pcm.drop();
    Ok(())
}
