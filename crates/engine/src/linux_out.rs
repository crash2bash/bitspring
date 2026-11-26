//! Linux output through ALSA.
//!
//! - **A sound card's own device** (`hw:CARD=…,DEV=…`) is Linux' bit-perfect path: we open
//!   the card directly, past PipeWire/PulseAudio, at the track's rate in the card's own
//!   integer format, so our samples reach the DAC unchanged. This is also what carries DoP,
//!   and native DSD where the kernel gives the card a DSD format (`DSD_U32_BE` and the like:
//!   see [`crate::alsa_dsd`]) — preferred over DoP, which stays the fallback.
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
use crate::alsa_dsd::{self, DsdFormat, NativeDsd};
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
    /// The native DSD formats it takes, each at its DSD rates (empty for most cards).
    native: NativeDsd,
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

    /// What a card's own device can do: as found when it was first opened, else by opening
    /// it now (without blocking), else — for a USB card another app holds — as the kernel
    /// lists it. None for the system mixer.
    fn caps(&self) -> Option<Caps> {
        let Self::Hw { card, dev } = self else { return None };
        // In its own statement, so the cache's lock is let go before `probe` takes it again.
        let cached = caps_cache().lock().unwrap().get(&self.uid()).cloned();
        cached.or_else(|| probe(self)).or_else(|| usb_stream_caps(card, *dev))
    }

    /// Can this device carry DoP for a `dsd_rate` stream: a card's own device at
    /// `dsd_rate / 16` with an integer format of at least 24 bits. (Whether the DAC decodes
    /// DoP can't be queried — that's the user's per-device switch.)
    pub fn supports_dop(&self, dsd_rate: u32) -> bool {
        self.caps().is_some_and(|caps| caps.wide_int && caps.rates.contains(&(dsd_rate / 16)))
    }

    /// Does this device take a `dsd_rate` stream (2 822 400 for DSD64) as native DSD: a card's
    /// own device that the kernel gives a DSD format (DSD_U32_BE…) at the matching rate. USB
    /// DACs get one only through the kernel's quirk table, so most cards don't.
    pub fn supports_native_dsd(&self, dsd_rate: u32) -> bool {
        self.native_dsd_format(dsd_rate).is_some()
    }

    /// The native DSD format to send `dsd_rate` in, when the device has one: the first of
    /// [`DsdFormat::PREFERENCE`] it runs at that rate in.
    fn native_dsd_format(&self, dsd_rate: u32) -> Option<DsdFormat> {
        self.caps()?.native.format_for(dsd_rate)
    }
}

fn alsa_format(format: DsdFormat) -> Format {
    match format {
        DsdFormat::U32Be => Format::DSDU32BE,
        DsdFormat::U32Le => Format::DSDU32LE,
        DsdFormat::U16Be => Format::DSDU16BE,
        DsdFormat::U16Le => Format::DSDU16LE,
        DsdFormat::U8 => Format::DSDU8,
    }
}

/// Open the device just to see what it can do (without blocking if another app has it).
fn probe(device: &Device) -> Option<Caps> {
    let pcm = PCM::new(&device.pcm_name(), Direction::Playback, true).ok()?;
    let hw = HwParams::any(&pcm).ok()?;
    hw.set_access(Access::RWInterleaved).ok()?;
    // Rates format by format: with every format open a rate passes when *any* format runs at
    // it, and a card's DSD format runs at card rates (DSD512 in DSD_U32: 705.6 kHz) that may be
    // no PCM rate of it.
    let with = |format: Format| {
        let one = hw.clone();
        one.test_format(format).ok()?;
        one.set_format(format).ok()?;
        Some(one)
    };
    let mut rates: Vec<u32> = Vec::new();
    for one in HW_FORMATS.iter().filter_map(|&(f, _)| with(f)) {
        for r in RATES {
            if !rates.contains(&r) && one.test_rate(r).is_ok() {
                rates.push(r);
            }
        }
    }
    rates.sort_unstable();
    let mut native = NativeDsd::default();
    for format in DsdFormat::PREFERENCE {
        let Some(one) = with(alsa_format(format)) else { continue };
        for r in format.device_rates().filter(|&r| one.test_rate(r).is_ok()) {
            native.add(format, format.dsd_rate(r));
        }
    }
    let wide_int = [Format::S32LE, Format::S243LE, Format::S24LE].iter().any(|&f| hw.test_format(f).is_ok());
    let max_channels = hw.get_channels_max().unwrap_or(2).clamp(1, 32);
    if rates.is_empty() {
        return None;
    }
    let caps = Caps { rates, max_channels, wide_int, native };
    caps_cache().lock().unwrap().insert(device.uid(), caps.clone());
    Some(caps)
}

/// A USB card's formats and rates as the kernel lists them (`/proc/asound/<card id>/stream<dev>`,
/// see [`alsa_dsd::parse_stream`]): readable even while another app has the card open. Other
/// cards have no such file.
fn usb_stream_caps(card: &str, dev: u32) -> Option<Caps> {
    let text = std::fs::read_to_string(format!("/proc/asound/{card}/stream{dev}")).ok()?;
    let s = alsa_dsd::parse_stream(&text, &RATES)?;
    Some(Caps { rates: s.pcm_rates, max_channels: s.max_channels, wide_int: s.wide_int, native: s.native })
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
            let caps = device.caps();
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
        let dsd_rate = output::wire_dsd_rate(&track, opts);
        let mut dsd = output::DsdSupport {
            dop: dsd_rate.is_some_and(|r| device.supports_dop(r)),
            native: dsd_rate.is_some_and(|r| device.supports_native_dsd(r)),
        };
        let wide = exclusive && track.channels > 2 && info.output_channels > 2;
        let open_channels = if wide { info.output_channels as usize } else { 2 };
        let speakers = crate::channels::default_layout(open_channels);
        let mut plan = output::plan(&track, opts, &info.rates, &info.name, dsd, &speakers)?;
        let stream = match Stream::open(&device, &plan, open_channels, &info.name) {
            // The card wouldn't run its native DSD format at that rate after all: once more as if
            // it had none (DoP where it carries that, else as `plan` decides). Not when it is busy
            // or gone: that is no better the other way.
            Err(e) if plan.native && !matches!(UserError::of(&e), UserError::DeviceBusy | UserError::DeviceNotFound) => {
                log::warn!("{}: native DSD didn't open ({e:#}); trying without", info.name.trim());
                dsd.native = false;
                plan = output::plan(&track, opts, &info.rates, &info.name, dsd, &speakers)?;
                Stream::open(&device, &plan, open_channels, &info.name)?
            }
            r => r?,
        };
        let Stream { thread, neg, pb_tx, stop, failure } = stream;
        let abort = |pb_tx: &mpsc::SyncSender<Option<Arc<Playback>>>, thread: JoinHandle<()>| {
            let _ = pb_tx.send(None);
            let _ = thread.join();
        };
        // The card said where its channels go, or took fewer than asked: plan for what it has.
        let actual = neg.speakers.clone().unwrap_or_else(|| crate::channels::default_layout(neg.channels));
        if actual != speakers {
            match output::plan(&track, opts, &info.rates, &info.name, dsd, &actual) {
                Ok(p) => plan = p,
                Err(e) => {
                    abort(&pb_tx, thread);
                    return Err(e);
                }
            }
        }
        // Native DSD runs the card at its own rate for the format (DSD64 in DSD_U32: 88.2 kHz),
        // but the playback and the signal path count our 16-bit words, as for DoP: `plan.target`
        // (dsd_rate / 16). The card's real rate is in `neg.physical`.
        let rate = if plan.native { plan.target } else { neg.rate };
        let path = output::signal_path(&track, opts, &plan, rate, Some(neg.physical), neg.format, exclusive, info.transport);
        let playback = match Playback::new(track, opts, &plan, rate) {
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

/// A stream thread with the card open and set up, waiting for its playback.
struct Stream {
    thread: JoinHandle<()>,
    neg: Negotiated,
    pb_tx: mpsc::SyncSender<Option<Arc<Playback>>>,
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<UserError>>>,
}

impl Stream {
    /// Open the card for `plan` on a new stream thread, with `channels`, and check it runs
    /// exactly as DSD needs: for native DSD the format and the card rate for it, for DoP a
    /// ≥24-bit integer format at `plan.target`.
    fn open(device: &Device, plan: &output::Plan, channels: usize, name: &str) -> Result<Self> {
        let (rate, wire) = if plan.native {
            let (format, rate) = device.caps().and_then(|c| alsa_dsd::native_open(&c.native, plan.target)).ok_or(UserError::FormatRefused)?;
            (rate, Wire::Native(format))
        } else {
            (plan.target as u32, Wire::Pcm { dop: plan.dop })
        };
        let (fmt_tx, fmt_rx) = mpsc::sync_channel::<Result<Negotiated>>(1);
        let (pb_tx, pb_rx) = mpsc::sync_channel::<Option<Arc<Playback>>>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let thread = {
            let dev = device.clone();
            let (stop, failure) = (stop.clone(), failure.clone());
            std::thread::Builder::new().name("hifi-alsa".into()).spawn(move || {
                if let Err(e) = stream_thread(&dev, rate, wire, channels, &fmt_tx, &pb_rx, &stop) {
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
        let wrong = match wire {
            Wire::Native(format) => neg.format != format.out_format() || neg.rate != rate as f64,
            Wire::Pcm { dop: true } => !neg.format.carries_dop() || neg.format == OutFormat::F32 || (neg.rate - plan.target).abs() >= 0.5,
            Wire::Pcm { dop: false } => false,
        };
        if wrong {
            let _ = pb_tx.send(None);
            let _ = thread.join();
            return Err(output::dop_error(name, output::dsd_rate_of_words(plan.target)));
        }
        Ok(Self { thread, neg, pb_tx, stop, failure })
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

/// What the card is opened in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire {
    /// PCM in the best format it has; `dop`: one that carries DoP untouched (≥24-bit integer).
    Pcm { dop: bool },
    /// Native DSD, in exactly this format (a card's own device only).
    Native(DsdFormat),
}

/// Set the hardware up: `target` Hz, the format for `wire`, `channels` (or what the card has).
fn configure(pcm: &PCM, exclusive: bool, target: u32, wire: Wire, channels: usize) -> Result<(OutFormat, u32, usize)> {
    let hw = HwParams::any(pcm).map_err(alsa_err)?;
    hw.set_access(Access::RWInterleaved).map_err(alsa_err)?;
    // A card's device must not convert anything behind our back.
    if exclusive {
        let _ = hw.set_rate_resample(false);
    }
    let format = match wire {
        Wire::Native(format) => {
            hw.test_format(alsa_format(format)).map_err(|_| UserError::FormatRefused)?;
            (alsa_format(format), format.out_format())
        }
        Wire::Pcm { dop } if exclusive => HW_FORMATS
            .iter()
            .filter(|(_, out)| !dop || (out.carries_dop() && *out != OutFormat::F32))
            .find(|(f, _)| hw.test_format(*f).is_ok())
            .copied()
            .ok_or(UserError::FormatRefused)?,
        Wire::Pcm { .. } => (Format::FloatLE, OutFormat::F32),
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
    if wire == Wire::Native(DsdFormat::U8) {
        even_period(&hw, target);
    } else {
        let _ = hw.set_period_time_near(20_000, ValueOr::Nearest);
    }
    pcm.hw_params(&hw).map_err(alsa_err)?;
    let rate = pcm.hw_params_current().and_then(|h| h.get_rate()).map_err(alsa_err)?;
    Ok((format.1, rate, channels))
}

/// ~20 ms periods of an even number of frames, for DSD_U8: one of our 16-bit words spans two
/// of its frames, so an odd period would end every write in half a word sent idle (see
/// `fill_native`), a dropout of 8 DSD bits per period. (Odd only if the card takes nothing
/// else.)
fn even_period(hw: &HwParams, rate: u32) {
    let want = (rate / 50) as alsa::pcm::Frames & !1;
    if hw.set_period_size(want, ValueOr::Nearest).is_ok() {
        return;
    }
    // Not that size: an even one next to the nearest it takes.
    if let Ok(near) = hw.clone().set_period_size_near(want, ValueOr::Nearest) {
        for p in [near, near + 1, near - 1] {
            if p > 0 && p % 2 == 0 && hw.set_period_size(p, ValueOr::Nearest).is_ok() {
                return;
            }
        }
    }
    let _ = hw.set_period_time_near(20_000, ValueOr::Nearest);
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
/// `target`: the card's rate (for native DSD the one for its format, not the word rate).
fn stream_thread(
    device: &Device,
    target: u32,
    wire: Wire,
    channels: usize,
    fmt_tx: &mpsc::SyncSender<Result<Negotiated>>,
    pb_rx: &mpsc::Receiver<Option<Arc<Playback>>>,
    stop: &AtomicBool,
) -> Result<()> {
    let exclusive = matches!(device, Device::Hw { .. });
    let pcm = open(device, stop)?;
    let (format, rate, channels) = configure(&pcm, exclusive, target, wire, channels)?;
    if exclusive && rate != target {
        bail!(UserError::RateRefused { khz: target as f64 / 1000.0 });
    }
    let speakers = pcm
        .get_chmap()
        .ok()
        .map(|m| Vec::<alsa::pcm::ChmapPosition>::from(&m).into_iter().map(speaker_of).collect::<Vec<_>>())
        .filter(|s| s.len() == channels && channels > 2);
    // (Native DSD: the card's own rate, and 1 bit — the bitstream's width, whatever the
    // container.)
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
    // (The bounds are even, so an even period stays even: DSD_U8 keeps whole words, see
    // `even_period`.)
    let period = (period as usize).clamp(64, 16_384);
    // In device frames, as `writei` counts (for native DSD the container: a DSD_U32 frame
    // holds two of the frames `render` counts).
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
