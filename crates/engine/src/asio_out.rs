//! Windows output through ASIO: a device's own driver, past the Windows audio stack, for
//! bit-perfect PCM and native DSD (DoP where the driver has no DSD mode).
//!
//! The driver side uses the `bitspring-asio` crate, which is GPL-3.0-only (it is built on the
//! Steinberg ASIO SDK under the SDK's GPLv3 option), so the Windows builds that link it are
//! distributed under GPLv3.
//!
//! - **One driver per process.** ASIO allows only one loaded driver, so there is one slot:
//!   a player for driver X reuses it when X is loaded, else closes what is loaded and opens X.
//!   A stream an older player still runs there is stopped first; every player has a
//!   generation number and only acts on the slot (stopping, reporting a failure) while it is
//!   still the slot's owner. The driver stays loaded after a player stops: starting again is
//!   cheap and its control panel stays usable. It is closed when another driver is needed, or
//!   by [`release`].
//! - **Probing** is done once per driver and kept for the process, only while nothing of ours
//!   plays exclusively: a probe switches the driver's rate (and mode), and the driver (or a
//!   wrapper like ASIO4ALL) may drive the very hardware a stream plays on. The device list
//!   probes the PCM side only (rates, channels, sample types, and whether the driver says it
//!   has a DSD mode, which switches nothing). The DSD mode itself is probed once per session,
//!   when the driver is about to be used: a stream on it starts, or the app asks whether it
//!   takes a DSD rate natively. A driver that fails to open is tried again only when the user
//!   refreshes the list. One whose settings changed (its control panel, a reset request) is
//!   probed afresh at its next use. Looking a driver up by uid touches no other driver.
//! - **The stream** renders straight into the driver's buffers from its realtime callback,
//!   one non-interleaved buffer per channel. Driver events (a reset request after its control
//!   panel changed the buffer size, a rate change) restart the stream in place; when that
//!   doesn't work the player reports [`crate::UserError::DeviceReset`]. A driver that stops
//!   calling back for two seconds is reported as [`crate::UserError::DeviceStalled`].
//! - **The control panel**: while a driver's settings window is open (many hold the host
//!   thread until it closes) no driver call is made: a start is refused at once
//!   ([`crate::UserError::DriverPanelOpen`]), a player dropped meanwhile goes silent and is
//!   stopped once the window closes, and the app can quit.
//!
//! The rules that need no driver are platform-independent, and tested everywhere.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::output::OutFormat;

/// The most channels one stream opens (`bitspring_asio::MAX_CHANNELS`, checked on Windows).
const MAX_OPEN: usize = 64;

/// Software ASIO drivers that sit on top of Windows audio (and resample or mix): never
/// bit-perfect, whatever they play on.
const WRAPPERS: [&str; 5] = ["asio4all", "flexasio", "asio2wasapi", "realtek asio", "generic low latency asio"];

/// The name a driver is listed under: its registry description, with " (ASIO)" so it isn't
/// mistaken for the same device's WASAPI entry (unless the name says ASIO already).
fn display_name(description: &str) -> String {
    let name = description.trim();
    if name.to_lowercase().contains("asio") { name.to_string() } else { format!("{name} (ASIO)") }
}

/// The driver is a software wrapper over Windows audio (see [`WRAPPERS`]).
fn is_wrapper(name: &str) -> bool {
    let name = name.to_lowercase();
    WRAPPERS.iter().any(|w| name.contains(w))
}

/// The uid (and per-device settings key) of the driver with this CLSID.
fn uid_of(clsid: &str) -> String {
    format!("asio:{clsid}")
}

// ASIOSampleType values (the SDK's numbers), for the formats we write.
const INT16_LSB: i32 = 16;
const INT24_LSB: i32 = 17;
const INT32_LSB: i32 = 18;
const FLOAT32_LSB: i32 = 19;
const INT32_LSB16: i32 = 24;
const INT32_LSB18: i32 = 25;
const INT32_LSB20: i32 = 26;
const INT32_LSB24: i32 = 27;
const DSD_INT8_LSB1: i32 = 32;
const DSD_INT8_MSB1: i32 = 33;

/// How we write a PCM channel buffer of this ASIO sample type (its SDK number). The
/// `Int32LSB16/18/20/24` types hold the value in the low bits of a 32-bit word. None for the
/// big-endian types, 64-bit float and anything else: the driver is refused for PCM.
fn pcm_format(raw: i32) -> Option<OutFormat> {
    Some(match raw {
        INT32_LSB => OutFormat::I32 { bits: 32 },
        INT32_LSB24 => OutFormat::I32 { bits: 24 },
        INT32_LSB20 => OutFormat::I32 { bits: 20 },
        INT32_LSB18 => OutFormat::I32 { bits: 18 },
        INT32_LSB16 => OutFormat::I32 { bits: 16 },
        INT24_LSB => OutFormat::I24Packed,
        INT16_LSB => OutFormat::I16,
        FLOAT32_LSB => OutFormat::F32,
        _ => return None,
    })
}

/// How we write a native DSD channel buffer of this ASIO sample type: one DSD byte at a time,
/// oldest bit in the MSB (DSDInt8MSB1) or the LSB (DSDInt8LSB1).
fn dsd_format(raw: i32) -> Option<OutFormat> {
    match raw {
        DSD_INT8_MSB1 => Some(OutFormat::Dsd { bytes: 1, big_endian: true, lsb_first: false }),
        DSD_INT8_LSB1 => Some(OutFormat::Dsd { bytes: 1, big_endian: true, lsb_first: true }),
        _ => None,
    }
}

/// The format takes DoP words untouched: a ≥24-bit integer (not float, which the driver
/// would convert on its way to the DAC).
fn carries_dop(format: OutFormat) -> bool {
    format.carries_dop() && format != OutFormat::F32
}

/// The driver can carry a `dsd_rate` stream as DoP: PCM at `dsd_rate / 16` in a format that
/// takes the words untouched (`first_type`: the first output's PCM sample type).
fn dop_capable(pcm_rates: &[u32], first_type: Option<i32>, dsd_rate: u32) -> bool {
    pcm_rates.contains(&(dsd_rate / 16)) && first_type.and_then(pcm_format).is_some_and(carries_dop)
}

/// The driver takes `dsd_rate` natively: its DSD mode runs at that rate, in a 1-bit type.
fn native_capable(dsd_rates: &[u32], first_type: Option<i32>, dsd_rate: u32) -> bool {
    dsd_rates.contains(&dsd_rate) && first_type.and_then(dsd_format).is_some()
}

/// How many outputs to open (the first ones): the track's channels, at least a stereo pair,
/// at most what the driver has.
fn open_channels(track_channels: usize, outputs: usize) -> usize {
    track_channels.max(2).min(outputs).min(MAX_OPEN)
}

/// What the driver is set to: in DSD mode its "sample rate" is the DSD bit rate (2 822 400
/// for DSD64), while the plan counts our 16-bit words; PCM and DoP go at the plan's rate.
fn stream_rate(target: f64, native: bool) -> f64 {
    if native { crate::output::dsd_rate_of_words(target) as f64 } else { target }
}

/// The rate to call the driver's own: the one it runs at, when it is one of its PCM rates
/// (a DSD rate left over from another app isn't), else 44.1 kHz, else its first.
fn nominal_rate(current: Option<f64>, pcm_rates: &[u32]) -> f64 {
    current
        .filter(|&r| pcm_rates.iter().any(|&p| (p as f64 - r).abs() < 0.5))
        .or_else(|| pcm_rates.contains(&44_100).then_some(44_100.0))
        .or_else(|| pcm_rates.first().map(|&r| r as f64))
        .unwrap_or(0.0)
}

/// Why a started stream isn't the one planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// The driver runs at another rate.
    Rate,
    /// Its sample type is one we don't write (or not DSD in DSD mode).
    Format,
    /// PCM in a format that can't carry DoP untouched.
    Dop,
}

/// The format to render a started stream in: by its sample type (`raw`), checked against what
/// was planned (`native` DSD, or `dop` words in PCM) and the rate asked for.
fn stream_format(raw: i32, got: f64, asked: f64, native: bool, dop: bool) -> Result<OutFormat, Refusal> {
    if (got - asked).abs() >= 0.5 {
        return Err(Refusal::Rate);
    }
    let format = if native { dsd_format(raw) } else { pcm_format(raw) }.ok_or(Refusal::Format)?;
    if dop && !native && !carries_dop(format) {
        return Err(Refusal::Dop);
    }
    Ok(format)
}

/// Every format a stream renders in, so the realtime callback can read the one in use from
/// an atomic byte (see [`format_code`]).
const FORMATS: [OutFormat; 10] = [
    OutFormat::I32 { bits: 32 },
    OutFormat::I32 { bits: 24 },
    OutFormat::I32 { bits: 20 },
    OutFormat::I32 { bits: 18 },
    OutFormat::I32 { bits: 16 },
    OutFormat::I24Packed,
    OutFormat::I16,
    OutFormat::F32,
    OutFormat::Dsd { bytes: 1, big_endian: true, lsb_first: false },
    OutFormat::Dsd { bytes: 1, big_endian: true, lsb_first: true },
];

/// `format` as a byte: its place in [`FORMATS`] plus one (0 = not known yet).
fn format_code(format: OutFormat) -> u8 {
    FORMATS.iter().position(|&f| f == format).map_or(0, |i| i as u8 + 1)
}

/// The format a [`format_code`] stands for; None for 0 (not known yet: render nothing).
#[inline(always)]
fn format_of_code(code: u8) -> Option<OutFormat> {
    FORMATS.get((code as usize).checked_sub(1)?).copied()
}

/// What the process' one ASIO driver slot holds.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Slot {
    /// The CLSID of the driver loaded, if one is.
    loaded: Option<String>,
    /// The generation of the player whose stream runs on it, if one does.
    owner: Option<u64>,
}

/// What it takes to have a driver loaded in the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadStep {
    /// It is loaded already.
    Reuse,
    /// Nothing is loaded: open it.
    Open,
    /// Another driver is: close that one, then open it.
    Replace,
}

impl Slot {
    const EMPTY: Slot = Slot { loaded: None, owner: None };

    fn load_step(&self, clsid: &str) -> LoadStep {
        match self.loaded.as_deref() {
            Some(c) if c == clsid => LoadStep::Reuse,
            Some(_) => LoadStep::Replace,
            None => LoadStep::Open,
        }
    }

    fn is_owner(&self, generation: u64) -> bool {
        self.owner == Some(generation)
    }

    /// A stream runs, on another driver than `clsid`: that one can't be loaded now.
    fn busy_elsewhere(&self, clsid: &str) -> bool {
        self.owner.is_some() && self.loaded.as_deref() != Some(clsid)
    }
}

/// How often a stream may be restarted in place after driver events before it gives up and
/// reports the device reset: a driver that keeps asking would otherwise loop forever.
struct RestartBudget {
    recent: VecDeque<Instant>,
}

impl RestartBudget {
    const MAX: usize = 3;
    const WINDOW: Duration = Duration::from_secs(60);

    fn new() -> Self {
        Self { recent: VecDeque::new() }
    }

    /// One more restart at `now`, if fewer than [`Self::MAX`] were made in the last
    /// [`Self::WINDOW`].
    fn allow(&mut self, now: Instant) -> bool {
        while self.recent.front().is_some_and(|&t| now.duration_since(t) >= Self::WINDOW) {
            self.recent.pop_front();
        }
        if self.recent.len() >= Self::MAX {
            return false;
        }
        self.recent.push_back(now);
        true
    }
}

/// A driver that stopped calling back (its DAC unplugged without a reset request, a hung
/// driver): the stream's callback count stands still for [`Self::AFTER`] while the playback
/// isn't paused.
struct Stall {
    count: u64,
    since: Instant,
}

impl Stall {
    const AFTER: Duration = Duration::from_secs(2);

    fn new(count: u64, now: Instant) -> Self {
        Self { count, since: now }
    }

    /// Start counting afresh from `count` at `now` (after a restart, or while the driver's
    /// settings window holds it).
    fn restart(&mut self, count: u64, now: Instant) {
        *self = Self::new(count, now);
    }

    /// `count` callbacks so far, at `now`: true once none came for [`Self::AFTER`] while the
    /// playback played. A paused playback isn't judged (and starts the clock again).
    fn stalled(&mut self, count: u64, paused: bool, now: Instant) -> bool {
        if count != self.count || paused {
            self.restart(count, now);
            return false;
        }
        now.duration_since(self.since) >= Self::AFTER
    }
}

#[cfg(windows)]
pub(crate) use win::{
    Player, control_panel, find, info, listing, native_dsd_known, release, release_idle, retry_failed, supports_dop,
    supports_native_dsd,
};

#[cfg(windows)]
mod win {
    use std::collections::HashMap;
    use std::ops::Deref;
    use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use anyhow::{Result, anyhow, bail};
    use bitspring_asio::{self as asio, Caps, DsdCaps, Events, Host, MAX_CHANNELS, RenderFn, StreamConfig, StreamInfo};

    use super::*;
    use crate::UserError;
    use crate::decode::Track;
    use crate::output::{self, OutBuf, PlayOptions, Playback, SignalPath, StreamFormat, Transport};
    use crate::rate::RateRange;
    use crate::windows_out::{Device, DeviceInfo};

    const _: () = assert!(MAX_CHANNELS == MAX_OPEN);

    /// The host thread every driver call goes through, made on first use. None when it
    /// couldn't start (then there is no ASIO).
    static HOST: OnceLock<Option<Host>> = OnceLock::new();

    fn host() -> Option<&'static Host> {
        HOST.get_or_init(|| match Host::new() {
            Ok(h) => Some(h),
            Err(e) => {
                log::warn!("ASIO: the host thread didn't start: {e}");
                None
            }
        })
        .as_ref()
    }

    /// The host thread if something started it already: for letting go of a driver, which
    /// has nothing to do when none was ever loaded (and mustn't start the thread for that).
    fn started_host() -> Option<&'static Host> {
        HOST.get().and_then(Option::as_ref)
    }

    fn need_host() -> Result<&'static Host> {
        host().ok_or_else(|| anyhow!("ASIO is not available"))
    }

    static SLOT: Mutex<Slot> = Mutex::new(Slot::EMPTY);

    /// The slot, held while a driver is opened, probed, started or stopped. (Never taken by
    /// the realtime callback.)
    fn slot() -> MutexGuard<'static, Slot> {
        SLOT.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `Slot::owner` for the watchers to check without the lock (0 = none).
    static OWNER: AtomicU64 = AtomicU64::new(0);
    static GENERATION: AtomicU64 = AtomicU64::new(0);

    fn set_owner(s: &mut Slot, owner: Option<u64>) {
        s.owner = owner;
        OWNER.store(owner.unwrap_or(0), Ordering::Release);
    }

    /// The loaded driver's settings window is open (see [`control_panel`]). Many drivers show
    /// it modally, which holds the host thread until it is closed, so while this is set no
    /// driver call is made: nothing starts, stops, probes or closes a driver (a call would
    /// wait for the user, with whatever locks its caller holds). Set and checked under the
    /// slot's lock, so a holder of the lock that sees it clear makes its calls before the
    /// window opens.
    static PANEL: AtomicBool = AtomicBool::new(false);

    fn panel_open() -> bool {
        PANEL.load(Ordering::Acquire)
    }

    /// Clears [`PANEL`] when the window's call returns (or fails).
    struct PanelOpen;

    impl Drop for PanelOpen {
        fn drop(&mut self) {
            PANEL.store(false, Ordering::Release);
        }
    }

    fn panel_error(description: &str) -> anyhow::Error {
        UserError::DriverPanelOpen { device: display_name(description) }.into()
    }

    /// A driver's DSD mode, as far as it was probed.
    #[derive(Debug, Clone)]
    enum DsdProbe {
        /// Not yet: done once per session, when the driver is about to be used (see
        /// [`dsd_locked`]).
        NotYet,
        Found(DsdCaps),
        /// It didn't open or probe; tried again after a refresh (see [`retry_failed`]).
        Failed,
    }

    /// What a probe found out about one driver.
    #[derive(Debug, Clone)]
    struct Probed {
        /// Its PCM side (`Host::probe_pcm`: never switched to DSD for this), and whether it
        /// says it has a DSD mode (`can_dsd`).
        caps: Caps,
        /// The rate it ran at, after the probe put it back.
        rate: Option<f64>,
        /// Its own name for itself (`getDriverName`).
        driver_name: String,
        dsd: DsdProbe,
        /// Its settings may have changed since (its control panel was open, it asked for a
        /// reset): probed afresh at the next use that may probe; until then what was found
        /// stands.
        stale: bool,
        /// Likewise for its DSD mode, probed afresh the next time it is about to be used.
        dsd_stale: bool,
    }

    impl Probed {
        fn first_pcm(&self) -> Option<i32> {
            self.caps.pcm_types.first().map(|t| t.raw())
        }

        /// See [`native_capable`]: from the DSD mode as probed (false until it is).
        fn native(&self, dsd_rate: u32) -> bool {
            match &self.dsd {
                DsdProbe::Found(d) => native_capable(&d.rates, d.types.first().map(|t| t.raw()), dsd_rate),
                _ => false,
            }
        }

        /// Its DSD mode should be probed (now, if that is safe): it says it has one, and that
        /// wasn't probed yet this session, or not since its settings changed.
        fn wants_dsd_probe(&self) -> bool {
            self.caps.can_dsd && (matches!(self.dsd, DsdProbe::NotYet) || self.dsd_stale)
        }
    }

    #[derive(Debug, Clone)]
    enum Cached {
        Ok(Probed),
        /// It failed to open or probe, for this reason. Not tried again before the device list
        /// is refreshed (see [`retry_failed`]).
        Failed(String),
    }

    fn cache() -> MutexGuard<'static, HashMap<String, Cached>> {
        static CACHE: OnceLock<Mutex<HashMap<String, Cached>>> = OnceLock::new();
        CACHE.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
    }

    fn cached(clsid: &str) -> Option<Probed> {
        match cache().get(clsid) {
            Some(Cached::Ok(p)) => Some(p.clone()),
            _ => None,
        }
    }

    /// What is known of `clsid` without a driver call: `Err` says why nothing is.
    fn known(clsid: &str) -> Result<Probed, String> {
        match cache().get(clsid) {
            Some(Cached::Ok(p)) => Ok(p.clone()),
            Some(Cached::Failed(why)) => Err(why.clone()),
            None => Err("not probed yet".into()),
        }
    }

    /// Its settings may have changed: probe it afresh when next used (see `Probed::stale`).
    fn mark_stale(clsid: &str) {
        if let Some(Cached::Ok(p)) = cache().get_mut(clsid) {
            p.stale = true;
            p.dsd_stale = true;
        }
    }

    /// The user refreshed the device list (a DAC may have been plugged in, another app may
    /// have let go of one): drivers that failed to open or probe are tried again at the next
    /// listing or use. Never automatically: a driver that can't open may take seconds to say
    /// so, every time.
    pub(crate) fn retry_failed() {
        let mut c = cache();
        c.retain(|_, e| !matches!(e, Cached::Failed(_)));
        for e in c.values_mut() {
            if let Cached::Ok(p) = e
                && matches!(p.dsd, DsdProbe::Failed)
            {
                p.dsd = DsdProbe::NotYet;
            }
        }
    }

    /// An installed driver: its CLSID in the registry's form `{XXXXXXXX-…}` (upper case, the
    /// uid and cache key), and its registry description.
    struct Entry {
        clsid: String,
        description: String,
    }

    fn drivers() -> Vec<Entry> {
        asio::drivers()
            .into_iter()
            .filter_map(|d| Some(Entry { clsid: asio::parse_clsid(&d.clsid)?.to_string(), description: d.name }))
            .collect()
    }

    fn entry(clsid: &str) -> Option<Entry> {
        drivers().into_iter().find(|e| e.clsid == clsid)
    }

    /// Load `clsid` into the slot (nothing to do if it is). The caller has made sure no
    /// stream runs on another driver, and that the driver's settings window isn't open.
    fn ensure_loaded(s: &mut Slot, host: &Host, clsid: &str) -> Result<(), asio::Error> {
        let step = s.load_step(clsid);
        if step == LoadStep::Reuse {
            return Ok(());
        }
        if step == LoadStep::Replace {
            let _ = host.close();
            s.loaded = None;
        }
        host.open(clsid)?;
        s.loaded = Some(clsid.to_string());
        Ok(())
    }

    /// Stop a stream that runs on the slot (an older player the app hasn't dropped yet).
    /// Not while the settings window is open (the callers check).
    fn preempt(s: &mut Slot, host: &Host) {
        if let Some(old) = s.owner {
            log::info!("ASIO: stopping the stream of an older player ({old})");
            let _ = host.stop();
            set_owner(s, None);
        }
    }

    /// With no stream running (and no settings window open): a driver that asked for a reset
    /// (its control panel changed something) is closed, and what was probed of it marked
    /// stale, so it is opened and probed afresh when next needed.
    fn settle(s: &mut Slot, host: &Host) {
        if s.owner.is_some() || panel_open() {
            return;
        }
        let ev = host.take_events();
        if (ev.reset_requested || ev.sample_rate_changed.is_some())
            && let Some(clsid) = s.loaded.take()
        {
            log::info!("ASIO: {clsid} asked for a reset; closing it");
            let _ = host.close();
            mark_stale(&clsid);
        }
    }

    /// Probe the PCM side of `clsid` on the slot (no stream may run; never switches it to
    /// DSD), and keep what it found. A stale entry keeps its DSD probe, marked stale too.
    fn probe_locked(s: &mut Slot, host: &Host, clsid: &str) -> Result<Probed, String> {
        if s.owner.is_some() {
            return Err("a stream is running".into());
        }
        let before = cached(clsid);
        let found = ensure_loaded(s, host, clsid).and_then(|()| host.probe_pcm()).map(|caps| {
            let info = host.info().ok();
            let (dsd, dsd_stale) = match before {
                Some(p) if !matches!(p.dsd, DsdProbe::NotYet) => (p.dsd, true),
                _ => (DsdProbe::NotYet, false),
            };
            Probed {
                caps,
                rate: info.as_ref().and_then(|i| i.sample_rate),
                driver_name: info.map(|i| i.name).unwrap_or_default(),
                dsd,
                stale: false,
                dsd_stale,
            }
        });
        match found {
            Ok(p) => {
                log::info!(
                    "ASIO: {clsid} ({}): {} outputs, PCM {:?} Hz in {:?}, DSD mode: {}",
                    p.driver_name,
                    p.caps.output_channels,
                    p.caps.pcm_rates,
                    p.caps.pcm_types.first(),
                    if p.caps.can_dsd { "said to be there (probed when used)" } else { "none" },
                );
                cache().insert(clsid.to_string(), Cached::Ok(p.clone()));
                Ok(p)
            }
            Err(e) => {
                log::warn!("ASIO: {clsid} didn't open or probe: {e}");
                if s.loaded.as_deref() == Some(clsid) {
                    let _ = host.close();
                    s.loaded = None;
                }
                cache().insert(clsid.to_string(), Cached::Failed(e.to_string()));
                Err(e.to_string())
            }
        }
    }

    /// The driver wasn't probed yet, or is stale: a probe is due where it may be made.
    fn needs_probe(clsid: &str) -> bool {
        match cache().get(clsid) {
            None => true,
            Some(Cached::Ok(p)) => p.stale,
            Some(Cached::Failed(_)) => false,
        }
    }

    /// What is known of `clsid`: as probed before, else (or when stale) by probing its PCM
    /// side now when `may_probe` and no stream runs. One that failed isn't tried again before
    /// a refresh (see [`retry_failed`]). `Err`: why nothing is known.
    fn caps_locked(s: &mut Slot, host: &Host, clsid: &str, may_probe: bool) -> Result<Probed, String> {
        let can = may_probe && s.owner.is_none();
        match cache().get(clsid).cloned() {
            Some(Cached::Ok(p)) if !p.stale || !can => Ok(p),
            Some(Cached::Failed(why)) => Err(why),
            None if !can => Err("not probed yet: something plays".into()),
            _ => probe_locked(s, host, clsid),
        }
    }

    /// Probe the DSD mode of `clsid` (loading it into the slot) when that is due (see
    /// `Probed::wants_dsd_probe`) and safe: nothing of ours plays (on this driver or another:
    /// a DSD probe switches the driver's mode and rate), and no settings window is open.
    /// Called only where the driver is about to be used: before a stream on it starts, or
    /// when the app asks whether it takes DSD natively (right before playing DSD on it).
    /// `p` is updated with what was found.
    fn dsd_locked(s: &mut Slot, host: &Host, clsid: &str, p: &mut Probed) {
        if !p.wants_dsd_probe() || s.owner.is_some() || !may_probe() {
            return;
        }
        let found = ensure_loaded(s, host, clsid).and_then(|()| host.probe_dsd());
        p.dsd = match found {
            Ok(d) => {
                log::info!("ASIO: {clsid} ({}): DSD {:?} in {:?}", p.driver_name, d.rates, d.types.first());
                DsdProbe::Found(d)
            }
            Err(e) => {
                log::warn!("ASIO: {clsid}: its DSD mode didn't probe: {e}");
                DsdProbe::Failed
            }
        };
        p.dsd_stale = false;
        if let Some(Cached::Ok(c)) = cache().get_mut(clsid) {
            c.dsd = p.dsd.clone();
            c.dsd_stale = false;
        }
    }

    /// Probing is safe: no exclusive WASAPI stream of ours holds a device (an ASIO stream is
    /// checked on the slot), and no driver's settings window holds the host thread.
    fn may_probe() -> bool {
        !panel_open() && !crate::wasapi_out::exclusive_running()
    }

    fn device_info(entry: &Entry, p: &Probed) -> DeviceInfo {
        let wrapper = is_wrapper(&entry.description) || is_wrapper(&p.driver_name);
        DeviceInfo {
            device: Device::Asio { clsid: entry.clsid.clone() },
            name: display_name(&entry.description),
            uid: uid_of(&entry.clsid),
            transport: if wrapper { Transport::Virtual } else { Transport::Asio },
            output_channels: p.caps.output_channels.max(0) as u32,
            channel_mask: 0,
            nominal_rate: nominal_rate(p.rate, &p.caps.pcm_rates),
            rates: p.caps.pcm_rates.iter().map(|&r| RateRange::exact(r as f64)).collect(),
            is_default: false,
        }
    }

    /// The driver can be offered: it has outputs and PCM rates.
    fn usable(p: &Probed) -> bool {
        p.caps.output_channels > 0 && !p.caps.pcm_rates.is_empty()
    }

    /// The drivers probed before, as outputs, without a driver call.
    fn listing_known(drivers: &[Entry]) -> Vec<DeviceInfo> {
        drivers.iter().filter_map(|e| cached(&e.clsid).filter(usable).map(|p| device_info(e, &p))).collect()
    }

    /// The installed drivers as outputs. Ones not probed yet (or stale) are probed now when
    /// that is safe (nothing of ours plays exclusively, no settings window open), for their
    /// PCM side only: a listing never switches a driver to DSD (see [`dsd_locked`]). Else
    /// they are left out of this listing (stale ones listed as they were): without a probe
    /// their rates and channels aren't known, so they'd show what they can't do. A driver
    /// that fails to open or probe is left out too (logged) until the list is refreshed, so
    /// no broken entry is offered; one probed before stays listed even when another app holds
    /// it now.
    pub(crate) fn listing() -> Vec<DeviceInfo> {
        let drivers = drivers();
        if drivers.is_empty() {
            return Vec::new();
        }
        if panel_open() {
            return listing_known(&drivers);
        }
        let Some(host) = host() else { return Vec::new() };
        let may_probe = may_probe();
        let mut out = Vec::new();
        let mut s = slot();
        if panel_open() {
            drop(s);
            return listing_known(&drivers);
        }
        let before = s.loaded.clone();
        let mut probed = false;
        settle(&mut s, host);
        for e in &drivers {
            probed |= may_probe && s.owner.is_none() && needs_probe(&e.clsid);
            match caps_locked(&mut s, host, &e.clsid, may_probe) {
                Ok(p) if usable(&p) => out.push(device_info(e, &p)),
                Ok(_) => log::info!("ASIO: {} has no outputs or PCM rates; not listed", e.description),
                Err(why) => log::debug!("ASIO: {} not listed: {why}", e.description),
            }
        }
        // A driver loaded just to be probed isn't kept: it might hold hardware.
        if probed && s.owner.is_none() && s.loaded != before
            && let Some(clsid) = s.loaded.take()
        {
            let _ = host.close();
            log::debug!("ASIO: closed {clsid} after probing");
        }
        out
    }

    /// The output for `clsid`, probing it (its PCM side) if it hasn't been (or is stale) and
    /// that is safe. Touches no other driver than this one (except to close an idle one to
    /// load it).
    pub(crate) fn info(clsid: &str) -> Result<DeviceInfo> {
        let e = entry(clsid).ok_or(UserError::DeviceNotFound)?;
        let p = if panel_open() {
            known(clsid)
        } else {
            let host = need_host()?;
            let mut s = slot();
            if panel_open() {
                known(clsid)
            } else {
                settle(&mut s, host);
                caps_locked(&mut s, host, clsid, may_probe())
            }
        };
        let p = p.map_err(|why| UserError::OpenFailed { device: display_name(&e.description), detail: why })?;
        if !usable(&p) {
            bail!(UserError::NoRates { device: display_name(&e.description) });
        }
        Ok(device_info(&e, &p))
    }

    /// The output for `clsid` (from an `asio:` uid), for a lookup on the play path: no other
    /// driver is touched (see [`info`]). None: no such driver is installed.
    pub(crate) fn find(clsid: &str) -> Result<Option<DeviceInfo>> {
        if entry(clsid).is_none() {
            return Ok(None);
        }
        info(clsid).map(Some)
    }

    /// See [`dop_capable`]; from what was probed.
    pub(crate) fn supports_dop(clsid: &str, dsd_rate: u32) -> bool {
        cached(clsid).is_some_and(|p| dop_capable(&p.caps.pcm_rates, p.first_pcm(), dsd_rate))
    }

    /// See [`native_capable`]. The driver's DSD mode is probed here the first time this is
    /// asked (once per session, and again after its settings changed) when it says it has one
    /// and probing is safe now (see [`dsd_locked`]): the app asks right before playing DSD
    /// on it. Until then, and where it can't be probed, false.
    pub(crate) fn supports_native_dsd(clsid: &str, dsd_rate: u32) -> bool {
        let Some(mut p) = cached(clsid) else { return false };
        // (Not even the lock while a stream plays: no probe could be made.)
        if p.wants_dsd_probe() && OWNER.load(Ordering::Acquire) == 0 && may_probe()
            && let Some(host) = host()
        {
            let mut s = slot();
            settle(&mut s, host);
            if let Some(now) = cached(clsid) {
                p = now;
                dsd_locked(&mut s, host, clsid, &mut p);
            }
        }
        p.native(dsd_rate)
    }

    /// [`supports_native_dsd`] from what was probed, never probing: for the device list.
    pub(crate) fn native_dsd_known(clsid: &str, dsd_rate: u32) -> bool {
        cached(clsid).is_some_and(|p| p.native(dsd_rate))
    }

    /// Open the driver's own settings window: loaded into the slot if it isn't (never while a
    /// stream plays on another driver). What it changes is probed afresh at the next use; a
    /// stream on it that the driver resets restarts in place. Some drivers show a modal
    /// window: then this returns when it is closed, and meanwhile no driver call is made (see
    /// [`PANEL`]): a playback asked for on ASIO is refused, and the app can quit.
    pub(crate) fn control_panel(clsid: &str) -> Result<()> {
        let e = entry(clsid).ok_or(UserError::DeviceNotFound)?;
        let host = need_host()?;
        let open = {
            let mut s = slot();
            if s.busy_elsewhere(clsid) {
                bail!("stop playback on the other ASIO device first: only one ASIO driver can be open at a time");
            }
            if panel_open() {
                bail!(panel_error(&e.description));
            }
            settle(&mut s, host);
            ensure_loaded(&mut s, host, clsid).map_err(|err| load_error(err, &e.description))?;
            PANEL.store(true, Ordering::Release);
            PanelOpen
        };
        let shown = host.control_panel();
        drop(open);
        // Whatever it changed (a reset it asked for is handled where nothing plays, or by the
        // watcher of a stream on it): probed afresh when next used.
        mark_stale(clsid);
        shown.map_err(|err| anyhow!("{} control panel: {err}", display_name(&e.description)))
    }

    /// Let go of the driver (stopping a stream on it): before the app quits, so the driver
    /// is closed properly (and left in PCM mode for the next app). Nothing when no driver was
    /// ever loaded, or while its settings window holds the host thread (the app quits all the
    /// same).
    pub(crate) fn release() {
        let Some(host) = started_host() else { return };
        if panel_open() {
            log::info!("ASIO: the driver's settings window is open; not closing it");
            return;
        }
        let mut s = slot();
        if panel_open() {
            return;
        }
        preempt(&mut s, host);
        if s.loaded.take().is_some() {
            let _ = host.close();
        }
    }

    /// Close a driver kept loaded with nothing playing on it: before a WASAPI exclusive
    /// stream, which may want the same hardware. Nothing when no driver was ever loaded, or
    /// while its settings window is open.
    pub(crate) fn release_idle() {
        let Some(host) = started_host() else { return };
        if panel_open() {
            return;
        }
        let mut s = slot();
        if !panel_open() && s.owner.is_none() && s.loaded.take().is_some() {
            let _ = host.close();
        }
    }

    /// The driver didn't load or start up (not registered, its DAC not connected, or in use
    /// by another app: its own words are in `detail`).
    fn load_error(e: asio::Error, description: &str) -> anyhow::Error {
        match e {
            asio::Error::BadClsid(_) => UserError::DeviceNotFound.into(),
            e => UserError::OpenFailed { device: display_name(description), detail: e.to_string() }.into(),
        }
    }

    /// A stream didn't start.
    fn start_error(e: asio::Error, rate: f64, native: bool) -> anyhow::Error {
        let code = match &e {
            asio::Error::RateNotApplied { .. } | asio::Error::Asio { call: "setSampleRate", .. } if !native => {
                UserError::RateRefused { khz: rate / 1000.0 }
            }
            asio::Error::RateNotApplied { .. } | asio::Error::Unsupported(_) | asio::Error::Asio { call: "setSampleRate", .. } => {
                UserError::FormatRefused
            }
            _ => return anyhow!("ASIO: {e}"),
        };
        anyhow::Error::new(e).context(code)
    }

    /// What a player's stream renders, kept to restart it in place: the playback, the
    /// callbacks counted (for [`Stall`]) and the switch that silences it.
    #[derive(Clone)]
    struct Feed {
        playback: Arc<Playback>,
        /// Callbacks so far: one relaxed increment each.
        callbacks: Arc<AtomicU64>,
        /// Write silence instead of the playback (a player dropped while the driver's
        /// settings window keeps its stream from being stopped).
        muted: Arc<AtomicBool>,
    }

    /// The render closure: holds the playback, and builds the per-channel buffers for it on
    /// the stack — no allocation, no lock, no logging. Renders nothing (the driver's buffers
    /// keep the silence the host filled them with) until `format` says what they hold.
    fn render_fn(feed: Feed, format: Arc<AtomicU8>) -> RenderFn {
        Box::new(move |channels: &mut [&mut [u8]]| {
            feed.callbacks.fetch_add(1, Ordering::Relaxed);
            let Some(fmt) = format_of_code(format.load(Ordering::Acquire)) else { return };
            if feed.muted.load(Ordering::Relaxed) {
                for data in channels.iter_mut() {
                    match fmt {
                        // (The idle pattern, in the format's bit order.)
                        OutFormat::Dsd { .. } => fmt.write(data, 0.0),
                        _ => data.fill(0),
                    }
                }
                return;
            }
            let n = channels.len().min(MAX_CHANNELS);
            let mut bufs: [OutBuf<'_>; MAX_CHANNELS] = std::array::from_fn(|_| OutBuf { data: &mut [], channels: 0, format: fmt });
            for (buf, data) in bufs.iter_mut().zip(channels.iter_mut()) {
                buf.data = &mut **data;
                buf.channels = 1;
            }
            feed.playback.shared().render(&mut bufs[..n]);
        })
    }

    /// What a stream is started with, kept to restart it in place.
    #[derive(Clone)]
    struct Wire {
        clsid: String,
        description: String,
        native: bool,
        dop: bool,
        rate: f64,
        channels: usize,
    }

    /// Start a stream for `wire` on the slot (held by the caller) as player `generation`,
    /// rendering `feed`. Stops what plays there first. Returns the stream and the format it
    /// renders in. Refused while a settings window is open.
    fn start_locked(s: &mut Slot, host: &Host, generation: u64, wire: &Wire, feed: &Feed) -> Result<(StreamInfo, OutFormat)> {
        if panel_open() {
            return Err(panel_error(&wire.description));
        }
        let format = Arc::new(AtomicU8::new(0));
        let config = StreamConfig {
            dsd: wire.native,
            rate: wire.rate,
            channels: (0..wire.channels as i32).collect(),
            buffer_size: None,
            render: render_fn(feed.clone(), format.clone()),
        };
        preempt(s, host);
        ensure_loaded(s, host, &wire.clsid).map_err(|e| load_error(e, &wire.description))?;
        // What the driver said before this stream isn't news for it.
        host.take_events();
        let info = host.start(config).map_err(|e| start_error(e, wire.rate, wire.native))?;
        match stream_format(info.sample_type.raw(), info.rate, wire.rate, wire.native, wire.dop) {
            Ok(f) => {
                format.store(format_code(f), Ordering::Release);
                set_owner(s, Some(generation));
                Ok((info, f))
            }
            Err(why) => {
                let _ = host.stop();
                log::warn!("ASIO: {} started at {} Hz in {:?}: {why:?}", wire.description, info.rate, info.sample_type);
                Err(match why {
                    Refusal::Rate if !wire.native => UserError::RateRefused { khz: wire.rate / 1000.0 }.into(),
                    Refusal::Dop => output::dop_error(&display_name(&wire.description), (wire.rate * 16.0).round() as u32),
                    _ => UserError::FormatRefused.into(),
                })
            }
        }
    }

    pub(crate) struct Player {
        clsid: String,
        generation: u64,
        feed: Feed,
        failure: Arc<Mutex<Option<UserError>>>,
        /// Dropouts the driver reported (overloads, resyncs).
        dropouts: Arc<AtomicU64>,
        /// Dropping the sender wakes the watcher to leave.
        watcher: Option<(mpsc::Sender<()>, JoinHandle<()>)>,
        pub path: SignalPath,
    }

    impl Deref for Player {
        type Target = Playback;
        fn deref(&self) -> &Playback {
            &self.feed.playback
        }
    }

    impl Player {
        /// Play `track` on the driver `clsid`. Always exclusive (ASIO is), whatever `opts`
        /// says; native DSD where the driver has it at the rate, DoP where it carries it and
        /// the user allows it, else PCM. Refused at once while a driver's settings window is
        /// open ([`UserError::DriverPanelOpen`]).
        pub(crate) fn start(clsid: &str, track: Arc<Track>, opts: &PlayOptions) -> Result<Self> {
            let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
            let e = entry(clsid).ok_or(UserError::DeviceNotFound)?;
            if panel_open() {
                return Err(panel_error(&e.description));
            }
            let host = need_host()?;
            // An older stream stops first: it may be what keeps this driver from probing or
            // loading. The driver is about to be used: its DSD mode is probed now, if that is
            // due (once per session), so the plan knows it and the app can tell from now on.
            let probed = {
                let mut s = slot();
                if panel_open() {
                    return Err(panel_error(&e.description));
                }
                preempt(&mut s, host);
                settle(&mut s, host);
                caps_locked(&mut s, host, clsid, may_probe()).map(|mut p| {
                    dsd_locked(&mut s, host, clsid, &mut p);
                    p
                })
            }
            .map_err(|why| UserError::OpenFailed { device: display_name(&e.description), detail: why })?;
            if !usable(&probed) {
                bail!(UserError::NoRates { device: display_name(&e.description) });
            }
            let info = device_info(&e, &probed);
            let opts = &PlayOptions { exclusive: true, ..opts.clone() };
            let dsd_rate = output::wire_dsd_rate(&track, opts);
            let mut dsd = output::DsdSupport {
                dop: opts.allow_dop && dsd_rate.is_some_and(|r| dop_capable(&probed.caps.pcm_rates, probed.first_pcm(), r)),
                native: dsd_rate.is_some_and(|r| probed.native(r)),
            };
            let channels = open_channels(track.channels, probed.caps.output_channels.max(0) as usize);
            let speakers = crate::channels::default_layout(channels);
            let mut plan = output::plan(&track, opts, &info.rates, &info.name, dsd, &speakers)?;
            let callbacks = Arc::new(AtomicU64::new(0));
            let muted = Arc::new(AtomicBool::new(false));
            let attempt = |plan: &output::Plan| -> Result<(Wire, Feed, StreamInfo, OutFormat)> {
                let wire = Wire {
                    clsid: clsid.to_string(),
                    description: e.description.clone(),
                    native: plan.native,
                    dop: plan.dop,
                    rate: stream_rate(plan.target, plan.native),
                    channels,
                };
                // The playback exists before the stream, which renders it from its first
                // callback on; it counts our frames: the plan's rate (for native DSD the word
                // rate, not the driver's bit rate).
                let playback = Arc::new(Playback::new(track.clone(), opts, plan, plan.target)?);
                let feed = Feed { playback, callbacks: callbacks.clone(), muted: muted.clone() };
                let (stream, format) = start_locked(&mut slot(), host, generation, &wire, &feed)?;
                Ok((wire, feed, stream, format))
            };
            let mut native_refused = false;
            let (wire, feed, stream, format) = match attempt(&plan) {
                // The driver wouldn't run its DSD mode at that rate after all: once more as if
                // it had none (DoP where it carries that and the user allows it, else as the
                // plan decides). Not when the driver itself didn't open, or its settings window
                // is open: no better that way.
                Err(err)
                    if plan.native
                        && !matches!(
                            UserError::of(&err),
                            UserError::OpenFailed { .. } | UserError::DeviceNotFound | UserError::DriverPanelOpen { .. }
                        ) =>
                {
                    log::warn!("{}: native DSD didn't start ({err:#}); trying without", info.name);
                    dsd.native = false;
                    native_refused = true;
                    plan = output::plan(&track, opts, &info.rates, &info.name, dsd, &speakers)?;
                    attempt(&plan)?
                }
                r => r?,
            };
            log::info!(
                "ASIO: {} playing: {} Hz, {:?}, {} channels, buffer {} (latency {:?})",
                info.name,
                stream.rate,
                stream.sample_type,
                channels,
                stream.buffer_size,
                stream.output_latency
            );
            // (Native DSD: the driver's bit rate, and 1 bit, the bitstream's width.)
            let physical = StreamFormat {
                sample_rate: stream.rate,
                bits: if format == OutFormat::F32 { 32 } else { format.bits() },
                float: format == OutFormat::F32,
                channels: channels as u32,
                non_mixable: true,
            };
            let mut path = output::signal_path(&track, opts, &plan, plan.target, Some(physical), format, true, info.transport);
            path.native_refused = native_refused;
            let failure = Arc::new(Mutex::new(None));
            let dropouts = Arc::new(AtomicU64::new(0));
            let (tx, rx) = mpsc::channel();
            let watch = Watch { generation, wire, format, feed: feed.clone(), failure: failure.clone(), dropouts: dropouts.clone() };
            let watcher = std::thread::Builder::new().name("hifi-asio-watch".into()).spawn(move || watch.run(&rx));
            let watcher = match watcher {
                Ok(handle) => Some((tx, handle)),
                Err(err) => {
                    log::warn!("ASIO: no watcher thread ({err}); driver events go unhandled");
                    None
                }
            };
            Ok(Self { clsid: clsid.to_string(), generation, feed, failure, dropouts, watcher, path })
        }

        pub(crate) fn clsid(&self) -> &str {
            &self.clsid
        }

        /// The stream stopped: the driver reset and couldn't be restarted in place, it stopped
        /// calling back, or the render callback failed.
        pub(crate) fn output_error(&self) -> Option<UserError> {
            self.failure.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

        /// Dropouts: the processing's own and the ones the driver reported.
        pub(crate) fn underruns(&self) -> u64 {
            self.feed.playback.underruns() + self.dropouts.load(Ordering::Relaxed)
        }
    }

    impl Drop for Player {
        fn drop(&mut self) {
            if let Some((tx, handle)) = self.watcher.take() {
                drop(tx);
                let _ = handle.join();
            }
            // Only while the stream on the slot is still ours (a newer player may have
            // taken it over). The driver stays loaded. The render closure, which holds the
            // playback too, is dropped by the stop, before `playback` here.
            let Some(host) = started_host() else { return };
            let mut s = slot();
            if !s.is_owner(self.generation) {
                return;
            }
            if !panel_open() {
                let _ = host.stop();
                set_owner(&mut s, None);
                return;
            }
            // The driver's settings window holds the host thread: stopping now would wait for
            // the user to close it (the app may be quitting). The stream goes silent at once
            // and is stopped once the window is closed, unless a newer player took over.
            self.feed.muted.store(true, Ordering::Relaxed);
            drop(s);
            let generation = self.generation;
            let stopper = std::thread::Builder::new().name("hifi-asio-stop".into()).spawn(move || {
                loop {
                    while panel_open() {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    let mut s = slot();
                    if !s.is_owner(generation) {
                        return;
                    }
                    if !panel_open() {
                        let _ = host.stop();
                        set_owner(&mut s, None);
                        return;
                    }
                }
            });
            if let Err(err) = stopper {
                log::warn!("ASIO: the stream stays silent until another starts ({err})");
            }
        }
    }

    /// What became of a reset the watcher handled.
    enum Reset {
        /// The stream runs again, in place.
        Restarted,
        /// The settings window is open: try again once it is closed.
        Later,
        /// Nothing more to watch (stopped with [`UserError::DeviceReset`], or not ours).
        Over,
    }

    /// A player's watch over the driver's events and callbacks, on a thread of its own.
    struct Watch {
        generation: u64,
        wire: Wire,
        format: OutFormat,
        feed: Feed,
        failure: Arc<Mutex<Option<UserError>>>,
        dropouts: Arc<AtomicU64>,
    }

    impl Watch {
        fn run(self, quit: &mpsc::Receiver<()>) {
            let Some(host) = started_host() else { return };
            let mut budget = RestartBudget::new();
            let mut stall = Stall::new(self.feed.callbacks.load(Ordering::Relaxed), Instant::now());
            // A reset to handle (and whether the budget allowed a restart), a failure to
            // report: kept while the settings window delays them.
            let mut reset: Option<bool> = None;
            let mut fail: Option<UserError> = None;
            loop {
                match quit.recv_timeout(Duration::from_millis(100)) {
                    Err(RecvTimeoutError::Timeout) => {}
                    _ => return,
                }
                if OWNER.load(Ordering::Acquire) != self.generation {
                    // Stopped (failed) or taken over by a newer player: nothing to watch.
                    return;
                }
                let now = Instant::now();
                let callbacks = self.feed.callbacks.load(Ordering::Relaxed);
                if panel_open() {
                    // Its settings window holds the host thread (and many drivers stop calling
                    // back meanwhile): nothing to do until it is closed. The driver's events
                    // stay where they are until then.
                    stall.restart(callbacks, now);
                    continue;
                }
                // The driver's events are taken only by the slot's owner, checked under its
                // lock: a stream that took over the slot gets its own events, never an older
                // player's watcher. (Not this round when the lock is busy.)
                let ev = match SLOT.try_lock() {
                    Ok(s) if s.is_owner(self.generation) => host.take_events(),
                    Err(TryLockError::Poisoned(p)) if p.get_ref().is_owner(self.generation) => host.take_events(),
                    Ok(_) | Err(TryLockError::Poisoned(_)) => return,
                    Err(TryLockError::WouldBlock) => Events::default(),
                };
                if ev.dropouts > 0 {
                    self.dropouts.fetch_add(ev.dropouts, Ordering::Relaxed);
                    log::debug!("ASIO: the driver reported {} dropouts", ev.dropouts);
                }
                if ev.latencies_changed {
                    log::debug!("ASIO: the driver's latencies changed");
                }
                if ev.render_panicked {
                    log::error!("ASIO: the render callback panicked");
                    fail = Some(UserError::other("the audio callback failed"));
                }
                if ev.reset_requested || ev.sample_rate_changed.is_some() {
                    log::info!("ASIO: {} reset (reset asked: {}, rate now: {:?})", self.wire.description, ev.reset_requested, ev.sample_rate_changed);
                    reset = Some(budget.allow(now));
                }
                if fail.is_none() && reset.is_none() && stall.stalled(callbacks, self.feed.playback.is_paused(), now) {
                    log::warn!("ASIO: {} stopped calling back for {:?}; stopping the stream", self.wire.description, Stall::AFTER);
                    fail = Some(UserError::DeviceStalled);
                }
                if let Some(why) = fail.clone() {
                    if self.fail(host, why) {
                        return;
                    }
                    continue;
                }
                if let Some(allowed) = reset {
                    match self.reset(host, allowed) {
                        Reset::Restarted => {
                            reset = None;
                            stall.restart(self.feed.callbacks.load(Ordering::Relaxed), Instant::now());
                        }
                        Reset::Later => {}
                        Reset::Over => return,
                    }
                }
            }
        }

        /// Stop the stream (while still ours) and say why. False: not now, the settings
        /// window is open (try again).
        fn fail(&self, host: &Host, why: UserError) -> bool {
            let mut s = slot();
            if !s.is_owner(self.generation) {
                return true;
            }
            if panel_open() {
                return false;
            }
            let _ = host.stop();
            set_owner(&mut s, None);
            *self.failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
            true
        }

        /// A reset (or rate change) the driver reported: close and reopen it, as a reset
        /// asks, and start the same stream again on the same playback, which carries on from
        /// where it was. All under the slot's lock, so no other player's stream can come
        /// between. When that isn't `allowed` (too many lately) or doesn't play as before
        /// (same rate, same format), the stream stays stopped and the player reports
        /// [`UserError::DeviceReset`]. What was probed of the driver is stale from now on.
        fn reset(&self, host: &Host, allowed: bool) -> Reset {
            let mut s = slot();
            if !s.is_owner(self.generation) {
                return Reset::Over;
            }
            if panel_open() {
                return Reset::Later;
            }
            let _ = host.stop();
            set_owner(&mut s, None);
            mark_stale(&self.wire.clsid);
            let back = allowed && {
                let _ = host.close();
                s.loaded = None;
                match start_locked(&mut s, host, self.generation, &self.wire, &self.feed) {
                    Ok((_, format)) if format == self.format => {
                        log::info!("ASIO: {} restarted in place", self.wire.description);
                        true
                    }
                    Ok((_, format)) => {
                        log::warn!("ASIO: {} came back in {format:?}, not {:?}", self.wire.description, self.format);
                        let _ = host.stop();
                        set_owner(&mut s, None);
                        false
                    }
                    Err(e) => {
                        log::warn!("ASIO: {} didn't restart: {e:#}", self.wire.description);
                        false
                    }
                }
            };
            if back {
                Reset::Restarted
            } else {
                *self.failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(UserError::DeviceReset);
                Reset::Over
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_say_asio_once() {
        assert_eq!(display_name("FiiO USB DAC"), "FiiO USB DAC (ASIO)");
        assert_eq!(display_name("  FiiO USB DAC  "), "FiiO USB DAC (ASIO)");
        assert_eq!(display_name("ASIO4ALL v2"), "ASIO4ALL v2");
        assert_eq!(display_name("Focusrite USB ASIO"), "Focusrite USB ASIO");
        assert_eq!(display_name("FlexAsio"), "FlexAsio");
        assert_eq!(uid_of("{232685C6-6548-49D8-846D-4141A3EF7560}"), "asio:{232685C6-6548-49D8-846D-4141A3EF7560}");
    }

    #[test]
    fn wrappers_over_windows_audio_are_virtual() {
        for name in ["ASIO4ALL v2", "FlexASIO", "ASIO2WASAPI", "Realtek ASIO", "Generic Low Latency ASIO Driver"] {
            assert!(is_wrapper(name), "{name}");
        }
        for name in ["FiiO USB DAC", "Focusrite USB ASIO", "RME Fireface USB", "Topping USB Audio"] {
            assert!(!is_wrapper(name), "{name}");
        }
    }

    #[test]
    fn sample_types_map_to_our_formats() {
        assert_eq!(pcm_format(INT32_LSB), Some(OutFormat::I32 { bits: 32 }));
        assert_eq!(pcm_format(INT32_LSB24), Some(OutFormat::I32 { bits: 24 }));
        assert_eq!(pcm_format(INT32_LSB20), Some(OutFormat::I32 { bits: 20 }));
        assert_eq!(pcm_format(INT32_LSB18), Some(OutFormat::I32 { bits: 18 }));
        assert_eq!(pcm_format(INT32_LSB16), Some(OutFormat::I32 { bits: 16 }));
        assert_eq!(pcm_format(INT24_LSB), Some(OutFormat::I24Packed));
        assert_eq!(pcm_format(INT16_LSB), Some(OutFormat::I16));
        assert_eq!(pcm_format(FLOAT32_LSB), Some(OutFormat::F32));
        // Big-endian types, 64-bit float, DSD and unknown values: refused for PCM.
        for raw in [0, 1, 2, 3, 4, 8, 9, 10, 11, 20, DSD_INT8_LSB1, DSD_INT8_MSB1, 40, 99] {
            assert_eq!(pcm_format(raw), None, "{raw}");
        }
        assert_eq!(dsd_format(DSD_INT8_MSB1), Some(OutFormat::Dsd { bytes: 1, big_endian: true, lsb_first: false }));
        assert_eq!(dsd_format(DSD_INT8_LSB1), Some(OutFormat::Dsd { bytes: 1, big_endian: true, lsb_first: true }));
        assert_eq!(dsd_format(40), None, "DSDInt8NER8 isn't streamed");
        assert_eq!(dsd_format(INT32_LSB), None);
    }

    #[test]
    fn small_samples_sit_in_the_low_bits() {
        // Int32LSB20: full scale is ±2^19, in the low 20 bits of the word.
        let mut out = [0u8; 4];
        OutFormat::I32 { bits: 20 }.write(&mut out, 0.5);
        assert_eq!(i32::from_le_bytes(out), 1 << 18);
        OutFormat::I32 { bits: 16 }.write(&mut out, -1.0);
        assert_eq!(i32::from_le_bytes(out), -(1 << 15));
        OutFormat::I32 { bits: 18 }.write(&mut out, 1.0);
        assert_eq!(i32::from_le_bytes(out), (1 << 17) - 1);
        assert_eq!(OutFormat::I32 { bits: 20 }.bits(), 20);
    }

    #[test]
    fn dop_and_native_capability() {
        let rates = [44_100, 88_200, 176_400];
        assert!(dop_capable(&rates, Some(INT32_LSB), 2_822_400));
        assert!(dop_capable(&rates, Some(INT32_LSB24), 2_822_400));
        assert!(dop_capable(&rates, Some(INT24_LSB), 2_822_400));
        assert!(!dop_capable(&rates, Some(FLOAT32_LSB), 2_822_400), "float isn't sent untouched");
        assert!(!dop_capable(&rates, Some(INT16_LSB), 2_822_400));
        assert!(!dop_capable(&rates, Some(INT32_LSB20), 2_822_400));
        assert!(!dop_capable(&rates, Some(INT32_LSB), 5_644_800), "352.8 kHz isn't among its rates");
        assert!(!dop_capable(&rates, None, 2_822_400));
        let dsd = [2_822_400, 5_644_800];
        assert!(native_capable(&dsd, Some(DSD_INT8_MSB1), 2_822_400));
        assert!(native_capable(&dsd, Some(DSD_INT8_LSB1), 5_644_800));
        assert!(!native_capable(&dsd, Some(DSD_INT8_MSB1), 11_289_600));
        assert!(!native_capable(&dsd, None, 2_822_400));
        assert!(!native_capable(&[], Some(DSD_INT8_MSB1), 2_822_400));
    }

    #[test]
    fn channels_opened() {
        assert_eq!(open_channels(2, 2), 2);
        assert_eq!(open_channels(1, 8), 2, "a stereo floor");
        assert_eq!(open_channels(2, 8), 2);
        assert_eq!(open_channels(6, 8), 6);
        assert_eq!(open_channels(6, 2), 2);
        assert_eq!(open_channels(8, 4), 4);
        assert_eq!(open_channels(2, 1), 1, "a mono driver");
        assert_eq!(open_channels(128, 200), MAX_OPEN);
    }

    #[test]
    fn rates_asked_of_the_driver() {
        // DSD64's word rate is 176.4 kHz; the driver is set to the bit rate.
        assert_eq!(stream_rate(176_400.0, true), 2_822_400.0);
        assert_eq!(stream_rate(352_800.0, true), 5_644_800.0);
        assert_eq!(stream_rate(192_000.0, true), 3_072_000.0);
        assert_eq!(stream_rate(176_400.0, false), 176_400.0, "DoP goes at the word rate");
        assert_eq!(stream_rate(44_100.0, false), 44_100.0);
        assert_eq!(nominal_rate(Some(96_000.0), &[44_100, 96_000]), 96_000.0);
        assert_eq!(nominal_rate(Some(2_822_400.0), &[44_100, 96_000]), 44_100.0);
        assert_eq!(nominal_rate(None, &[48_000, 96_000]), 48_000.0);
        assert_eq!(nominal_rate(None, &[]), 0.0);
    }

    #[test]
    fn a_started_stream_is_checked() {
        let dsd = stream_format(DSD_INT8_MSB1, 2_822_400.2, 2_822_400.0, true, true);
        assert_eq!(dsd, Ok(OutFormat::Dsd { bytes: 1, big_endian: true, lsb_first: false }));
        assert_eq!(stream_format(INT32_LSB, 2_822_400.0, 2_822_400.0, true, true), Err(Refusal::Format));
        assert_eq!(stream_format(DSD_INT8_LSB1, 5_644_800.0, 2_822_400.0, true, true), Err(Refusal::Rate));
        assert_eq!(stream_format(INT32_LSB, 176_400.0, 176_400.0, false, true), Ok(OutFormat::I32 { bits: 32 }));
        assert_eq!(stream_format(FLOAT32_LSB, 176_400.0, 176_400.0, false, true), Err(Refusal::Dop));
        assert_eq!(stream_format(FLOAT32_LSB, 44_100.0, 44_100.0, false, false), Ok(OutFormat::F32));
        assert_eq!(stream_format(INT32_LSB, 48_000.0, 44_100.0, false, false), Err(Refusal::Rate));
        assert_eq!(stream_format(2, 44_100.0, 44_100.0, false, false), Err(Refusal::Format), "Int32MSB");
    }

    #[test]
    fn format_codes_round_trip() {
        assert_eq!(format_of_code(0), None);
        for f in FORMATS {
            let code = format_code(f);
            assert_ne!(code, 0);
            assert_eq!(format_of_code(code), Some(f));
        }
        assert_eq!(format_of_code(FORMATS.len() as u8 + 1), None);
        // Every format the stream can come up in has a code.
        for raw in 0..64 {
            for f in [pcm_format(raw), dsd_format(raw)].into_iter().flatten() {
                assert_ne!(format_code(f), 0, "{raw}");
            }
        }
    }

    #[test]
    fn one_driver_slot() {
        let mut s = Slot::EMPTY;
        assert_eq!(s.load_step("A"), LoadStep::Open);
        s.loaded = Some("A".into());
        assert_eq!(s.load_step("A"), LoadStep::Reuse);
        assert_eq!(s.load_step("B"), LoadStep::Replace);
        assert!(!s.busy_elsewhere("B"), "nothing plays: B may replace A");
        s.owner = Some(7);
        assert!(s.is_owner(7) && !s.is_owner(6), "only the player whose stream runs");
        assert!(s.busy_elsewhere("B"));
        assert!(!s.busy_elsewhere("A"), "the panel of the driver that plays");
    }

    #[test]
    fn a_driver_that_stops_calling_back_is_noticed() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut s = Stall::new(0, t0);
        assert!(!s.stalled(5, false, at(500)), "callbacks coming");
        assert!(!s.stalled(5, false, at(2_000)), "1.5 s without one: not yet");
        assert!(s.stalled(5, false, at(2_500)), "2 s without one");
        // A paused playback isn't judged, and the clock starts again when it plays on.
        let mut s = Stall::new(0, t0);
        assert!(!s.stalled(0, true, at(3_000)));
        assert!(!s.stalled(0, false, at(4_000)));
        assert!(s.stalled(0, false, at(5_000)));
        // Restarted (after a reset in place, or while the settings window is open).
        s.restart(0, at(5_000));
        assert!(!s.stalled(0, false, at(6_500)));
        assert!(!s.stalled(1, false, at(9_000)), "one more callback");
    }

    #[test]
    fn restarts_are_rationed() {
        let t0 = Instant::now();
        let mut b = RestartBudget::new();
        assert!(b.allow(t0));
        assert!(b.allow(t0 + Duration::from_secs(1)));
        assert!(b.allow(t0 + Duration::from_secs(2)));
        assert!(!b.allow(t0 + Duration::from_secs(3)), "a driver that keeps asking");
        assert!(b.allow(t0 + Duration::from_secs(61)), "a minute later the first one is forgotten");
    }
}
