//! Playback, independent of the operating system.
//!
//! Everything between the decoded track and the device buffers lives here: the processing
//! chain on its render thread, the lock-free hand-off to the realtime callback, DoP packing,
//! sample format conversion, position, seek and meters. A backend (CoreAudio on macOS, WASAPI
//! on Windows) only finds the device, sets its rate and format, and calls
//! [`Shared::render`] from its realtime callback with the device's buffers.

use std::cell::UnsafeCell;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::decode::{DsdSource, Track};
use crate::dsd::{DOP_IDLE, PCM_GAIN};
use crate::dsp::crossfeed::CrossfeedLevel;
use crate::dsp::lossy::LossyPlan;
use crate::dsp::eq::{EqPreset, EqStage, IirPlan, Phase};
use crate::dsp::fir::{FirControl, FirDesigner};
use crate::dsp::{Chain, DspSettings, Upsample};
use crate::rate::{RateDecision, RateRange, choose_device_rate, upsample_target};
use crate::ring::Ring;

#[cfg(target_os = "macos")]
pub use crate::coreaudio::{Device, DeviceInfo, Player, output_devices};
#[cfg(windows)]
pub use crate::wasapi_out::{Device, DeviceInfo, Player, output_devices};
#[cfg(target_os = "ios")]
pub use crate::ios_out::{Device, DeviceInfo, Player, dop_blocker, output_devices, system_volume};
#[cfg(target_os = "android")]
pub use crate::android_out::{Device, DeviceInfo, Player, output_devices};

/// The system volume (0…1), where the OS may scale our samples with it (iOS). None elsewhere:
/// on a Mac or PC in exclusive mode the OS volume doesn't touch the stream.
#[cfg(not(target_os = "ios"))]
pub fn system_volume() -> Option<f32> {
    None
}

/// Why DSD can't go out as DoP right now, if something system-wide prevents it (iOS: volume).
#[cfg(not(target_os = "ios"))]
pub fn dop_blocker() -> Option<crate::UserError> {
    None
}

// ---------------------------------------------------------------------------------------------
// Types shared by every backend
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    BuiltIn,
    Usb,
    Bluetooth,
    Hdmi,
    DisplayPort,
    Thunderbolt,
    AirPlay,
    Aggregate,
    Virtual,
    /// A network player we stream to (UPnP/DLNA): the samples reach it exactly, over TCP.
    Network,
    Other(u32),
}

impl Transport {
    /// Can audio reach this device without being lossy-compressed or remixed by the OS?
    /// (Bluetooth re-encodes to AAC/SBC; AirPlay goes over the network.)
    pub fn can_be_bit_perfect(self) -> bool {
        !matches!(self, Self::Bluetooth | Self::AirPlay | Self::Aggregate | Self::Virtual)
    }
}

/// One concrete PCM format a device stream can run in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamFormat {
    pub sample_rate: f64,
    pub bits: u32,
    pub float: bool,
    pub channels: u32,
    /// Non-mixable = the OS passes our samples straight through ("integer mode").
    pub non_mixable: bool,
}

#[derive(Debug, Clone)]
pub struct PlayOptions {
    /// Exclusive access (hog mode / WASAPI exclusive). Off = the system mixer may touch it.
    pub exclusive: bool,
    /// Sound processing. Default (all off) keeps the bit-perfect path.
    pub dsp: DspSettings,
    /// Where to start, in seconds — so a restart (new device or settings) resumes in place
    /// without a blip of the track's beginning.
    pub start_at: f64,
    pub start_paused: bool,
}

impl Default for PlayOptions {
    fn default() -> Self {
        Self { exclusive: true, dsp: DspSettings::default(), start_at: 0.0, start_paused: false }
    }
}

/// What actually happens to the signal on its way to the DAC — shown to the user so
/// "bit-perfect" is a verifiable claim, not a badge.
#[derive(Debug, Clone)]
pub struct SignalPath {
    pub source_rate: u32,
    pub source_bits: Option<u32>,
    pub device_rate: f64,
    pub physical: Option<StreamFormat>,
    /// We write integers straight to the device (no float→int step in the OS).
    pub integer_mode: bool,
    pub exclusive: bool,
    pub transport: Transport,
    /// Sample rate converted on the way (upsampling, or a DAC that can't do the source rate).
    pub resampled: bool,
    /// Name of the EQ preset applied, if any.
    pub eq: Option<String>,
    /// The EQ runs in linear phase.
    pub eq_linear: bool,
    /// Name of the impulse response convolved with, if any.
    pub convolution: Option<String>,
    pub crossfeed: Option<CrossfeedLevel>,
    /// The source is MP3/AAC/Vorbis/Opus.
    pub source_lossy: bool,
    /// What the lossy enhancers do to this track (None = nothing).
    pub lossy: Option<LossyPlan>,
    /// The source is DSD: its rate, and whether it goes out as DoP or was converted to PCM.
    pub dsd: Option<DsdSource>,
    pub bit_perfect: bool,
    /// Significant bits of the samples we write (for a following track's bit-perfect check).
    out_bits: u32,
    /// Bit-perfect apart from the source's bit depth (rate, access, transport, no processing).
    pcm_clean: bool,
}

impl SignalPath {
    /// The path for `track` joining on after the current one (same device setup, same
    /// processing — see `Playback::queue_next`): only what depends on the source changes.
    pub fn for_next(&self, track: &Track) -> SignalPath {
        let bits_ok = match (self.physical, track.bits_per_sample) {
            (Some(p), Some(b)) => (p.float || p.bits >= b) && self.out_bits >= b && b <= 24,
            _ => false,
        };
        SignalPath {
            source_bits: track.bits_per_sample,
            source_lossy: track.lossy,
            dsd: track.dsd,
            bit_perfect: if track.is_dop() { self.bit_perfect } else { self.pcm_clean && bits_ok },
            ..self.clone()
        }
    }
}

pub fn format_rates(rates: &[RateRange]) -> String {
    rates
        .iter()
        .map(|r| {
            if (r.max - r.min).abs() < 0.5 {
                format!("{}", r.min / 1000.0)
            } else {
                format!("{}–{}", r.min / 1000.0, r.max / 1000.0)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
        + " kHz"
}

// ---------------------------------------------------------------------------------------------
// Sample formats
// ---------------------------------------------------------------------------------------------

/// Sample layout of one device buffer.
///
/// Usually float32 on macOS, but a hogged USB DAC may expose its integer format directly
/// ("integer mode"); WASAPI exclusive mode is always the DAC's own integer format.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(target_os = "ios", allow(dead_code))] // iOS writes float only
pub(crate) enum OutFormat {
    F32,
    I16,
    I24Packed,
    /// 4-byte container; `bits` = full-scale width of the value (32 when aligned high).
    I32 { bits: u32 },
}

impl OutFormat {
    pub(crate) fn bytes(self) -> usize {
        match self {
            Self::F32 | Self::I32 { .. } => 4,
            Self::I16 => 2,
            Self::I24Packed => 3,
        }
    }

    /// Significant bits the format carries (float32: its 24-bit mantissa).
    pub(crate) fn bits(self) -> u32 {
        match self {
            Self::I16 => 16,
            Self::I24Packed | Self::F32 => 24,
            Self::I32 { bits } => bits,
        }
    }

    /// Write one sample. f32 → int scaling is by a power of two, so a 16/24-bit source comes
    /// out as exactly its original integer (16-bit lands as `x << 8` in a 24-bit slot).
    #[inline(always)]
    pub(crate) fn write(self, out: &mut [u8], s: f32) {
        fn to_int(s: f32, bits: u32) -> i32 {
            let full = (1i64 << (bits - 1)) as f64;
            ((s as f64 * full).round() as i64).clamp(-(full as i64), full as i64 - 1) as i32
        }
        match self {
            Self::F32 => out.copy_from_slice(&s.to_le_bytes()),
            Self::I16 => out.copy_from_slice(&(to_int(s, 16) as i16).to_le_bytes()),
            Self::I24Packed => out.copy_from_slice(&to_int(s, 24).to_le_bytes()[..3]),
            Self::I32 { bits } => out.copy_from_slice(&to_int(s, bits).to_le_bytes()),
        }
    }

    /// Can carry a DoP word untouched (24 significant bits or more, no rescaling).
    #[cfg_attr(any(target_os = "ios", target_os = "android"), allow(dead_code))]
    pub(crate) fn carries_dop(self) -> bool {
        matches!(self, Self::F32 | Self::I24Packed | Self::I32 { bits: 24 | 32 })
    }

    /// Write one 24-bit DoP word (marker in bits 23..16, DSD in 15..0), MSB-aligned the way
    /// the format expects. F32 is exact too: a 24-bit integer / 2^23 is representable, and
    /// the OS's float→int step on a ≥24-bit integer DAC multiplies it straight back.
    #[inline(always)]
    pub(crate) fn write_dop(self, out: &mut [u8], word: u32) {
        let signed = ((word << 8) as i32) >> 8; // sign-extend 24 → 32
        match self {
            Self::I24Packed => out.copy_from_slice(&word.to_le_bytes()[..3]),
            Self::I32 { bits: 32 } => out.copy_from_slice(&(word << 8).to_le_bytes()),
            Self::I32 { .. } => out.copy_from_slice(&signed.to_le_bytes()),
            Self::F32 => out.copy_from_slice(&(signed as f32 / 8_388_608.0).to_le_bytes()),
            Self::I16 => out.fill(0),
        }
    }
}

/// One device buffer handed to [`Shared::render`]: interleaved frames of `channels` samples.
pub(crate) struct OutBuf<'a> {
    pub data: &'a mut [u8],
    pub channels: usize,
    pub format: OutFormat,
}

// ---------------------------------------------------------------------------------------------
// Planning a playback
// ---------------------------------------------------------------------------------------------

/// The decisions every backend makes the same way before touching the device.
pub(crate) struct Plan {
    /// Device rate to ask for.
    pub target: f64,
    pub start_frame: usize,
    pub lossy: Option<LossyPlan>,
    pub resampled: bool,
    /// Audio goes through the render thread (resampling, DSP or lossy enhancers).
    pub processing: bool,
    pub dop: bool,
}

pub(crate) fn dop_error(device_name: &str, track: &Track) -> anyhow::Error {
    crate::UserError::DopRefused {
        device: device_name.trim().to_string(),
        dsd: crate::dsd::dsd_name(track.dsd.map_or(0, |d| d.rate)),
        khz: track.sample_rate as f64 / 1000.0,
    }
    .into()
}

/// `rates`: what the device can run at. `dop_ok`: the device could carry this DoP track.
pub(crate) fn plan(track: &Track, opts: &PlayOptions, rates: &[RateRange], device_name: &str, dop_ok: bool) -> Result<Plan> {
    let src_hz = track.sample_rate as f64;
    // DoP: the DSD bits must reach the DAC exactly, so no processing and no rate change.
    let dop = track.is_dop();
    if dop && (!opts.exclusive || !dop_ok) {
        return Err(dop_error(device_name, track));
    }
    let mut target = match choose_device_rate(track.sample_rate, rates) {
        RateDecision::BitPerfect(hz) | RateDecision::Resample { device_hz: hz } => hz,
        RateDecision::Unsupported => bail!(crate::UserError::NoRates { device: device_name.trim().to_string() }),
    };
    if let Upsample::UpTo(max_hz) = opts.dsp.upsample {
        if !dop {
            target = upsample_target(track.sample_rate, rates, max_hz);
        }
    }
    let start_frame = ((opts.start_at.max(0.0) * src_hz) as usize).min(track.frames());
    // A progressively decoded track: give the decoder a head start (local files get there
    // in milliseconds; a stream may take a moment — then playback opens on buffering).
    track.wait_for(start_frame + track.sample_rate as usize / 2, Duration::from_secs(3));
    // Lossy enhancers measure the whole track (true peak, codec cutoff). A local MP3
    // decodes in well under a second; a stream gets the safe defaults instead.
    if track.lossy && opts.dsp.lossy.any() {
        track.wait_for(usize::MAX, Duration::from_millis(1500));
    }
    let lossy = crate::dsp::lossy::plan(track, &opts.dsp.lossy);
    let resampled = (target - src_hz).abs() >= 0.5;
    let processing = !dop && (resampled || opts.dsp.is_active() || lossy.is_some());
    if resampled && crate::dsp::resample::phases(track.sample_rate, target as u32) > crate::dsp::resample::MAX_PHASES {
        bail!(crate::UserError::Resample { from: track.sample_rate, to: target as u32 });
    }
    Ok(Plan { target, start_frame, lossy, resampled, processing, dop })
}

/// Fill in the signal path from what the backend actually got from the device.
pub(crate) fn signal_path(
    track: &Track,
    opts: &PlayOptions,
    plan: &Plan,
    device_rate: f64,
    physical: Option<StreamFormat>,
    out: OutFormat,
    exclusive: bool,
    transport: Transport,
) -> SignalPath {
    let src_hz = track.sample_rate as f64;
    let pcm_clean = !plan.processing
        && (device_rate - src_hz).abs() < 0.5
        && exclusive
        && transport.can_be_bit_perfect()
        && track.channels <= 2;
    // (Up to 24 bits: f32 holds those exactly.)
    let bits_ok = match (physical, track.bits_per_sample) {
        (Some(p), Some(b)) => (p.float || p.bits >= b) && out.bits() >= b && b <= 24,
        _ => false,
    };
    SignalPath {
        source_rate: track.sample_rate,
        source_bits: track.bits_per_sample,
        device_rate,
        physical,
        integer_mode: out != OutFormat::F32,
        exclusive,
        transport,
        resampled: plan.resampled,
        eq: opts.dsp.eq.as_ref().map(|p| p.name.clone()),
        eq_linear: opts.dsp.eq.as_ref().is_some_and(|p| p.phase == Phase::Linear),
        convolution: opts.dsp.convolution.as_ref().map(|c| c.0.name.clone()),
        crossfeed: opts.dsp.crossfeed,
        source_lossy: track.lossy,
        lossy: plan.lossy,
        dsd: track.dsd,
        // DoP gets this far only with the exact rate, a ≥24-bit integer format and exclusive
        // access (the backend checked).
        bit_perfect: (plan.dop && transport.can_be_bit_perfect()) || pcm_clean && bits_ok,
        out_bits: out.bits(),
        pcm_clean,
    }
}

// ---------------------------------------------------------------------------------------------
// Playback state shared with the realtime callback
// ---------------------------------------------------------------------------------------------

const NO_SEEK: usize = usize::MAX;
const NO_BOUNDARY: u64 = u64::MAX;
/// Callback scratch, in frames (CoreAudio's and WASAPI's buffers are far below). A bigger
/// request is served in pieces of this size.
pub(crate) const MAX_CALLBACK_FRAMES: usize = 16_384;

/// Which tracks play, for gapless playback: the one the source side reads, the one queued to
/// follow it, and the one being heard. The pointers are borrowed from `Playback::tracks`,
/// which keeps every track any of them names alive; only the control side frees tracks.
///
/// A track moves `next` → `cur` (by the side that reads the source: the callback on the
/// direct path, the render thread when processing), and `cur` → `heard` (by the callback,
/// when the track's first sample reaches the device). Each move stores the new slot before
/// clearing the old one, so a track the audio side may touch is always in one of them.
struct Slots {
    cur: AtomicPtr<Track>,
    next: AtomicPtr<Track>,
    heard: AtomicPtr<Track>,
    /// Bumped each time `heard` moves on to a following track.
    switches: AtomicU64,
    /// A seek: the frame, and the track it is for (ignored if that track no longer plays).
    seek_req: AtomicUsize,
    seek_track: AtomicPtr<Track>,
}

fn ptr(t: &Arc<Track>) -> *mut Track {
    Arc::as_ptr(t) as *mut Track
}

/// State shared with the realtime thread. Only atomics and immutable data: the callback
/// never locks, allocates, or blocks.
pub(crate) struct Shared {
    slots: Arc<Slots>,
    /// Direct mode: next frame of `slots.cur` to play.
    pos: AtomicUsize,
    paused: AtomicBool,
    finished: AtomicBool,
    /// Present when processing is on: audio comes from the render thread instead of `track`.
    ring: Option<Arc<RingState>>,
    /// Peak |sample| per output channel (L, R) since the UI last read it, as f32 bits.
    /// Non-negative f32 bit patterns order like u32, so `fetch_max` works on them directly.
    meter: [AtomicU32; 2],
    /// The track is raw DSD: the callback packs it as DoP.
    dop: bool,
    /// Callback-only: marker of the next DoP frame (false = 0x05, true = 0xFA). Runs on across
    /// callbacks, pauses, seeks and tracks — a break in the alternation makes the DAC drop out
    /// of DSD.
    dop_phase: AtomicBool,
    /// The lossy enhancers are on: they measure each track, so no track may follow another
    /// in the same chain when either is lossy.
    lossy_enhancers: bool,
}

/// Hand-off between the render thread (producer) and the callback (consumer).
struct RingState {
    ring: Ring,
    channels: usize,
    source_rate: f64,
    device_rate: f64,
    /// Callback-only scratch, preallocated so the callback never allocates.
    scratch: UnsafeCell<Vec<f32>>,
    /// Callback-only: set by a seek flush until the ring is flowing again, so the expected
    /// gap right after a seek isn't counted as a dropout.
    refilling: AtomicBool,
    /// Device frames played since the heard track's `base_frame` (reset on seek and when the
    /// next track starts being heard).
    played: AtomicU64,
    /// Source frame of the heard track where the current stretch of output started.
    base_frame: AtomicUsize,
    /// The render thread moved on to the next track: at this many `played` frames it is
    /// heard (NO_BOUNDARY: no switch pending).
    boundary: AtomicU64,
    /// Seek handshake: render bumps `flush_gen`, the callback discards the ring and echoes it.
    flush_gen: AtomicU64,
    flush_ack: AtomicU64,
    render_done: AtomicBool,
    /// The render thread has let the chain's tail out at the end of the last track: nothing
    /// can join on any more (until a seek).
    tail_out: AtomicBool,
    quit: AtomicBool,
    /// Peak |sample| after processing, as f32 bits (for a clipping indicator).
    peak: AtomicU32,
    /// Callbacks that found the ring short while the render thread wasn't finished — each
    /// one is an audible dropout. Should stay 0.
    underruns: AtomicU64,
    /// Render thread is waiting for the decoder (a stream still downloading): silence then is
    /// buffering, not a dropout of ours.
    starved: AtomicBool,
    /// The parametric EQ, run by the callback on what it pops from the ring.
    eq: Option<EqStage>,
    /// The render thread's FIR stage (linear-phase EQ, curves, convolution).
    fir: Option<Arc<FirControl>>,
}

// SAFETY: `scratch` is touched only by the callback thread; everything else is atomic or `Ring`
// (itself SPSC-safe: render thread produces, callback consumes).
unsafe impl Sync for RingState {}

/// The OS-independent half of a player: shared state plus the render thread.
/// Backends deref their `Player` to this for position, seek, pause and meters.
pub struct Playback {
    /// Boxed: its address is handed to the OS callback and must not move.
    shared: Box<Shared>,
    render: Option<JoinHandle<()>>,
    /// Owns every track the slots point at (and, briefly, ones they no longer do).
    tracks: Mutex<Vec<Arc<Track>>>,
}

impl Playback {
    /// Set up state (and the render thread when `plan.processing`) for a device running at
    /// `device_rate`. Returns once the ring has a head start, so IO can begin right away.
    pub(crate) fn new(track: Arc<Track>, opts: &PlayOptions, plan: &Plan, device_rate: f64) -> Result<Self> {
        let slots = Arc::new(Slots {
            cur: AtomicPtr::new(ptr(&track)),
            next: AtomicPtr::new(std::ptr::null_mut()),
            heard: AtomicPtr::new(ptr(&track)),
            switches: AtomicU64::new(0),
            seek_req: AtomicUsize::new(NO_SEEK),
            seek_track: AtomicPtr::new(std::ptr::null_mut()),
        });
        let (ring, render) = if plan.processing {
            let channels = track.channels;
            // Check against the rate the device actually settled on — it can differ from the
            // one planned — so the resampler is never built for a ratio it refuses.
            let out_rate = device_rate.round() as u32;
            if out_rate != track.sample_rate
                && crate::dsp::resample::phases(track.sample_rate, out_rate) > crate::dsp::resample::MAX_PHASES
            {
                bail!(crate::UserError::Resample { from: track.sample_rate, to: out_rate });
            }
            let fs = out_rate as f64;
            let eq = opts.dsp.eq.as_ref();
            let ir = opts.dsp.convolution.as_ref().map(|c| &*c.0);
            let fir = FirDesigner::new(eq, ir, fs, channels)?.map(|d| {
                let control = FirControl::new(d);
                let first = control.designer.design(eq, opts.dsp.eq_bypass);
                (control, first)
            });
            let rs = Arc::new(RingState {
                // ~0.5 s of audio: enough to ride out scheduling hiccups, short enough that
                // a seek or a settings change is heard promptly.
                ring: Ring::new(((device_rate * 0.5) as usize).max(32_768) * channels),
                channels,
                source_rate: track.sample_rate as f64,
                device_rate,
                scratch: UnsafeCell::new(vec![0.0; MAX_CALLBACK_FRAMES * channels]),
                refilling: AtomicBool::new(true),
                played: AtomicU64::new(0),
                base_frame: AtomicUsize::new(plan.start_frame),
                boundary: AtomicU64::new(NO_BOUNDARY),
                flush_gen: AtomicU64::new(0),
                flush_ack: AtomicU64::new(0),
                render_done: AtomicBool::new(false),
                tail_out: AtomicBool::new(false),
                quit: AtomicBool::new(false),
                peak: AtomicU32::new(0),
                underruns: AtomicU64::new(0),
                starved: AtomicBool::new(false),
                eq: eq.map(|e| EqStage::new(e, fs, channels, opts.dsp.eq_bypass)),
                fir: fir.as_ref().map(|(c, _)| c.clone()),
            });
            let chain = Chain::new(&opts.dsp, plan.lossy.as_ref(), fir, track.sample_rate, out_rate, channels);
            let handle = {
                let (slots, rs) = (slots.clone(), rs.clone());
                let start = plan.start_frame;
                std::thread::Builder::new()
                    .name("hifi-render".into())
                    .spawn(move || render_loop(slots, rs, chain, start))?
            };
            // Let the ring fill a little before IO starts, so playback doesn't open on silence.
            let deadline = Instant::now() + Duration::from_millis(500);
            while rs.ring.available() < rs.ring.capacity() / 4
                && !rs.render_done.load(Ordering::Acquire)
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(2));
            }
            (Some(rs), Some(handle))
        } else {
            (None, None)
        };
        let shared = Box::new(Shared {
            slots,
            pos: AtomicUsize::new(plan.start_frame),
            paused: AtomicBool::new(opts.start_paused),
            finished: AtomicBool::new(false),
            ring,
            meter: [AtomicU32::new(0), AtomicU32::new(0)],
            dop: plan.dop,
            dop_phase: AtomicBool::new(false),
            lossy_enhancers: opts.dsp.lossy.any(),
        });
        Ok(Self { shared, render, tracks: Mutex::new(vec![track]) })
    }

    /// Stable pointer for an OS callback's user data. Valid until this `Playback` drops;
    /// the backend must stop its callback before that.
    pub(crate) fn shared(&self) -> &Shared {
        &self.shared
    }

    /// Frees tracks no slot names any more, and returns the owned tracks behind the slots
    /// (heard, cur, next), read in an order that never misses a track the audio side holds.
    fn tidy(&self) -> (std::sync::MutexGuard<'_, Vec<Arc<Track>>>, [*mut Track; 3]) {
        let mut tracks = self.tracks.lock().unwrap_or_else(|p| p.into_inner());
        let s = &self.shared.slots;
        // next → cur → heard: the order tracks move in, so one moving while we read is seen
        // in its old slot or its new one.
        let next = s.next.load(Ordering::Acquire);
        let cur = s.cur.load(Ordering::Acquire);
        let heard = s.heard.load(Ordering::Acquire);
        tracks.retain(|t| [next, cur, heard].contains(&ptr(t)));
        (tracks, [heard, cur, next])
    }

    fn owned(tracks: &[Arc<Track>], p: *mut Track) -> Option<Arc<Track>> {
        tracks.iter().find(|t| ptr(t) == p).cloned()
    }

    /// The track being heard now.
    pub fn heard(&self) -> Arc<Track> {
        let (tracks, [heard, ..]) = self.tidy();
        Self::owned(&tracks, heard).expect("the heard track is owned")
    }

    /// The track that will follow the heard one without a break, if one is lined up.
    pub fn upcoming(&self) -> Option<Arc<Track>> {
        let (tracks, [heard, cur, next]) = self.tidy();
        Self::owned(&tracks, if cur != heard { cur } else { next })
    }

    /// Right after a render that moved on to the next track: how many of the frames it
    /// produced already belong to that track (the rest were the previous one's last).
    pub(crate) fn frames_since_switch(&self) -> usize {
        match &self.shared.ring {
            Some(rs) => rs.played.load(Ordering::Relaxed) as usize,
            None => self.shared.pos.load(Ordering::Relaxed),
        }
    }

    /// How many times playback has moved on to a following track by itself.
    pub fn switches(&self) -> u64 {
        self.shared.slots.switches.load(Ordering::Acquire)
    }

    /// Whether `next` can follow `prev` in this playback without a break: the device keeps its
    /// rate and format, and the processing treats it the same way.
    fn can_follow(&self, prev: &Track, next: &Track) -> bool {
        prev.sample_rate == next.sample_rate
            && prev.channels == next.channels
            && prev.is_dop() == next.is_dop()
            && !(self.shared.lossy_enhancers && (prev.lossy || next.lossy))
    }

    /// Line up `track` to follow the current one without a break (replacing one lined up
    /// before). False when it can't: a different rate, channel count or kind (then let the
    /// current track end and start the next one anew), or it is too late (the current track
    /// has already been handed to the device in full and the one after it is on its way).
    pub fn queue_next(&self, track: Arc<Track>) -> bool {
        let (mut tracks, [heard, cur, _]) = self.tidy();
        let tail_out = self.shared.ring.as_ref().is_some_and(|rs| rs.tail_out.load(Ordering::Acquire));
        if cur != heard || tail_out || self.is_finished() {
            return false;
        }
        // SAFETY: `cur` is owned by `tracks`, which we hold locked.
        if !self.can_follow(unsafe { &*cur }, &track) {
            return false;
        }
        let p = ptr(&track);
        tracks.push(track);
        // The one it replaces, if still waiting, is freed by the next tidy.
        let next = &self.shared.slots.next;
        next.store(p, Ordering::SeqCst);
        // The render thread may have reached the end meanwhile. It sets `tail_out` and then
        // looks at `next`, we store `next` and then look at `tail_out` (both SeqCst): one of
        // us sees the other. If it is us, take the track back unless it was already taken.
        if self.shared.ring.as_ref().is_some_and(|rs| rs.tail_out.load(Ordering::SeqCst))
            && next.compare_exchange(p, std::ptr::null_mut(), Ordering::AcqRel, Ordering::Relaxed).is_ok()
        {
            return false;
        }
        true
    }

    /// Nothing follows the current track any more (the queue changed).
    pub fn clear_next(&self) {
        let _tracks = self.tidy().0;
        self.shared.slots.next.store(std::ptr::null_mut(), Ordering::Release);
    }

    pub fn position_secs(&self) -> f64 {
        let st = &self.shared;
        let (tracks, [heard, ..]) = self.tidy();
        // SAFETY: owned by `tracks`, held locked.
        let t = unsafe { &*heard };
        let pending = st.slots.seek_req.load(Ordering::Acquire);
        let pos = if pending != NO_SEEK && st.slots.seek_track.load(Ordering::Acquire) == heard {
            pending as f64 / t.sample_rate as f64
        } else {
            match &st.ring {
                None => st.pos.load(Ordering::Relaxed) as f64 / t.sample_rate as f64,
                Some(rs) => {
                    rs.base_frame.load(Ordering::Relaxed) as f64 / rs.source_rate
                        + rs.played.load(Ordering::Relaxed) as f64 / rs.device_rate
                }
            }
        };
        let duration = t.duration_secs();
        drop(tracks);
        pos.min(duration)
    }

    pub fn duration_secs(&self) -> f64 {
        self.heard().duration_secs()
    }

    pub fn is_finished(&self) -> bool {
        self.shared.finished.load(Ordering::Relaxed)
    }

    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    /// Highest |sample| the processing chain has produced (1.0 = full scale), or None on
    /// the bit-perfect path.
    pub fn output_peak(&self) -> Option<f32> {
        self.shared.ring.as_ref().map(|rs| f32::from_bits(rs.peak.load(Ordering::Relaxed)))
    }

    /// Peak level per channel (linear, 0..1) since the last call — for level meters.
    pub fn take_meter(&self) -> [f32; 2] {
        [0, 1].map(|c| f32::from_bits(self.shared.meter[c].swap(0, Ordering::Relaxed)))
    }

    /// Dropouts on the processed path so far (see `RingState::underruns`).
    pub fn underruns(&self) -> u64 {
        self.shared.ring.as_ref().map_or(0, |rs| rs.underruns.load(Ordering::Relaxed))
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    /// Jump within the track being heard. False when this playback can't — then open the
    /// track anew at that point and start it there:
    /// - the point is far from the part of a long file held in memory;
    /// - the processing has already moved on to the next track (the last ~half second of
    ///   this one is still in the buffer).
    pub fn seek(&self, secs: f64) -> bool {
        let (tracks, [heard, cur, _]) = self.tidy();
        if self.shared.ring.is_some() && cur != heard {
            return false;
        }
        // SAFETY: owned by `tracks`, held locked.
        let t = unsafe { &*heard };
        let frame = ((secs.max(0.0) * t.sample_rate as f64) as usize).min(t.frames());
        if !t.can_seek_to(frame) {
            return false;
        }
        let s = &self.shared.slots;
        s.seek_track.store(heard, Ordering::Release);
        s.seek_req.store(frame, Ordering::Release);
        self.shared.finished.store(t.is_complete() && frame >= t.frames(), Ordering::Relaxed);
        drop(tracks);
        true
    }

    /// Playback is waiting for audio that hasn't been decoded/downloaded yet.
    pub fn is_buffering(&self) -> bool {
        let t = self.heard();
        if t.is_complete() || self.is_finished() {
            return false;
        }
        match &self.shared.ring {
            None => self.shared.pos.load(Ordering::Relaxed) >= t.ready_frames(),
            Some(rs) => rs.starved.load(Ordering::Relaxed) && rs.ring.available() == 0,
        }
    }

    /// How far decoding/downloading has got, in seconds (for a buffer bar on the scrubber).
    pub fn buffered_secs(&self) -> f64 {
        let t = self.heard();
        t.ready_frames() as f64 / t.sample_rate as f64
    }

    /// The decoder stopped on an error (a dropped connection, a corrupt file).
    pub fn source_error(&self) -> Option<String> {
        self.heard().error()
    }

    /// Apply new equaliser settings to what plays, without a restart: the callback crossfades
    /// to them within a buffer (minimum phase), the FIR stage after the render buffer. False
    /// when this playback can't take them live — it started without the EQ, or the change
    /// needs a different FIR layout (phase mode, a curve added or removed): start a new one.
    pub fn update_eq(&self, eq: &EqPreset, bypass: bool) -> bool {
        let Some(rs) = &self.shared.ring else { return false };
        let Some(stage) = &rs.eq else { return false };
        let fits = match &rs.fir {
            Some(c) => c.designer.fits(Some(eq)),
            None => !eq.needs_fir(),
        };
        if !fits {
            return false;
        }
        let fs = rs.device_rate.round();
        stage.mailbox.post(IirPlan::new(eq, fs, rs.channels, eq.phase == Phase::Minimum));
        stage.mailbox.set_bypass(bypass);
        if let Some(c) = rs.fir.as_ref().filter(|c| c.designer.has_eq()) {
            c.post(c.designer.design(Some(eq), bypass));
        }
        true
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        if let (Some(rs), Some(handle)) = (&self.shared.ring, self.render.take()) {
            rs.quit.store(true, Ordering::Release);
            let _ = handle.join();
        }
    }
}

/// The render thread: pulls source audio from RAM, runs the processing chain, keeps the ring
/// topped up. Not realtime — it may sleep, and only has to stay ahead of the DAC on average.
///
/// At the end of a track it carries straight on with the one lined up next, through the same
/// chain (no reset: filters ring on across the join as they would within a track), and tells
/// the callback at which output frame the new track begins to be heard.
fn render_loop(slots: Arc<Slots>, rs: Arc<RingState>, mut chain: Chain, start_frame: usize) {
    // Our own thread: flush-to-zero for its whole life (the FIR's tails, see fpu.rs).
    let _ftz = crate::fpu::FlushDenormals::on();
    const BLOCK: usize = 2048;
    let mut cur = slots.cur.load(Ordering::Acquire);
    let mut src = start_frame;
    // Where this stretch of the current track started (a seek, or 0 for a following track).
    let mut stretch = start_frame;
    let mut flushed = false;
    let mut out: Vec<f32> = Vec::new();
    let mut sent = 0usize;
    let nap = || std::thread::sleep(Duration::from_millis(2));

    while !rs.quit.load(Ordering::Acquire) {
        // SAFETY: `cur` stays in a slot (and so owned by the Playback) until we move it on.
        let track = unsafe { &*cur };
        if let Some(eq) = &rs.eq {
            eq.mailbox.collect();
        }
        let seek = slots.seek_req.swap(NO_SEEK, Ordering::AcqRel);
        if seek != NO_SEEK && slots.seek_track.load(Ordering::Acquire) == cur {
            src = seek.min(track.frames());
            stretch = src;
            flushed = false;
            rs.tail_out.store(false, Ordering::Release);
            chain.reset();
            out.clear();
            sent = 0;
            rs.render_done.store(false, Ordering::Release);
            rs.base_frame.store(src, Ordering::Relaxed);
            let generation = rs.flush_gen.fetch_add(1, Ordering::AcqRel) + 1;
            // Wait for the callback to drop what's queued, so no pre-seek audio slips through.
            while rs.flush_ack.load(Ordering::Acquire) != generation {
                if rs.quit.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            continue;
        }

        if sent < out.len() {
            sent += rs.ring.push(&out[sent..]);
            if sent < out.len() {
                nap();
            }
            continue;
        }

        // Tell a windowed track where we read, before reading (see Track::claim).
        src = track.claim(src);
        // Read `complete` before the slice: if it was already complete, the slice is final.
        let complete = track.is_complete();
        let input = track.slice(src, BLOCK);
        if input.is_empty() {
            if !complete {
                rs.starved.store(true, Ordering::Relaxed);
            } else if !flushed {
                let next = slots.next.load(Ordering::Acquire);
                if next.is_null() {
                    // The end: let the chain's tail out (filter delay, resampler). From here
                    // a track lined up late can't join; queue_next says so.
                    rs.tail_out.store(true, Ordering::SeqCst);
                    if !slots.next.load(Ordering::SeqCst).is_null() {
                        // Lined up just now, before it could see `tail_out`: join after all.
                        rs.tail_out.store(false, Ordering::Release);
                        continue;
                    }
                    let processed = chain.process(&[], true);
                    out.clear();
                    out.extend(processed.iter().map(|&v| v as f32));
                    sent = 0;
                    flushed = true;
                    continue;
                }
                if rs.boundary.load(Ordering::Acquire) == NO_BOUNDARY {
                    // The next track joins on. Its first frame is heard once the device has
                    // played this track's remaining frames (at the device rate).
                    let rest = (track.frames().saturating_sub(stretch)) as f64 * rs.device_rate / rs.source_rate;
                    slots.cur.store(next, Ordering::Release);
                    let _ = slots.next.compare_exchange(next, std::ptr::null_mut(), Ordering::AcqRel, Ordering::Relaxed);
                    rs.boundary.store(rest.round() as u64, Ordering::Release);
                    cur = next;
                    src = 0;
                    stretch = 0;
                    continue;
                }
                // A very short track is still waiting to be heard: one join at a time.
            } else {
                rs.render_done.store(true, Ordering::Release);
            }
            nap();
            continue;
        }
        rs.starved.store(false, Ordering::Relaxed);
        src += input.len() / track.channels;
        let processed = chain.process(input, false);
        out.clear();
        // Not clamped yet: the callback's EQ may still lower the level (its preamp).
        out.extend(processed.iter().map(|&v| v as f32));
        sent = 0;
    }
}

impl Shared {
    /// Fill the device's buffers for one callback. Realtime-safe: no locks, no allocation.
    /// Returns how many frames came from the music; the rest of the buffers (paused, still
    /// decoding, the end) is silence. A DAC plays that silence; a network stream leaves it
    /// out and waits.
    #[inline]
    pub(crate) fn render(&self, buffers: &mut [OutBuf<'_>]) -> usize {
        // The EQ's filters decay into denormals on quiet passages, which run many times
        // slower on the CPU and can make the callback miss its deadline.
        let _ftz = crate::fpu::FlushDenormals::on();
        let st = self;
        let slots = &*st.slots;
        let paused = st.paused.load(Ordering::Relaxed);
        let frames = buffers
            .iter()
            .find(|b| b.channels > 0)
            .map(|b| b.data.len() / (b.channels * b.format.bytes()))
            .unwrap_or(0);

        match &st.ring {
            // Bit-perfect path: straight from the decoded track, on into the next one.
            None => {
                let mut cur = slots.cur.load(Ordering::Acquire);
                let seek = slots.seek_req.load(Ordering::Acquire);
                if seek != NO_SEEK
                    && slots.seek_req.compare_exchange(seek, NO_SEEK, Ordering::AcqRel, Ordering::Relaxed).is_ok()
                    && slots.seek_track.load(Ordering::Acquire) == cur
                {
                    st.pos.store(seek, Ordering::Relaxed);
                }
                let mut done = 0;
                let mut peaks = [0f32; 2];
                loop {
                    // SAFETY: `cur` is in a slot, so the Playback owns it.
                    let track = unsafe { &*cur };
                    // Tell a windowed track where we read, before reading (see Track::claim).
                    let pos = track.claim(st.pos.load(Ordering::Relaxed));
                    // `complete` first: if decoding had finished, `ready` read after it is final.
                    let complete = track.is_complete();
                    let ready = track.ready_frames();
                    // Past `ready` on a track still decoding = buffering: play silence, keep position.
                    let avail = if paused { 0 } else { ready.saturating_sub(pos).min(frames - done) };
                    let ended = !paused && complete && pos + avail >= ready;
                    let next = if ended { slots.next.load(Ordering::Acquire) } else { std::ptr::null_mut() };
                    // Joining the next track: only this track's frames now, the rest from it.
                    let to = if next.is_null() { frames } else { done + avail };
                    let p = if st.dop {
                        fill_dop(buffers, done..to, avail, &st.dop_phase, |f, dc| dop_sample(track, pos + f, dc))
                    } else {
                        fill(buffers, done..to, avail, |f, dc| track_sample(track, pos + f, dc))
                    };
                    peaks = [peaks[0].max(p[0]), peaks[1].max(p[1])];
                    done += avail;
                    st.pos.store(pos + avail, Ordering::Relaxed);
                    if next.is_null() {
                        if ended {
                            st.finished.store(true, Ordering::Relaxed);
                        }
                        break;
                    }
                    // Store the new slot before clearing the old one (see `Slots`).
                    slots.cur.store(next, Ordering::Release);
                    slots.heard.store(next, Ordering::Release);
                    let _ = slots.next.compare_exchange(next, std::ptr::null_mut(), Ordering::AcqRel, Ordering::Relaxed);
                    st.pos.store(0, Ordering::Relaxed);
                    st.finished.store(false, Ordering::Relaxed);
                    slots.switches.fetch_add(1, Ordering::AcqRel);
                    cur = next;
                    if done >= frames {
                        break;
                    }
                }
                st.meter(peaks);
                done
            }
            // Processed path: pre-rendered samples from the ring.
            Some(rs) => {
                // SAFETY: only this (IO) thread ever touches `scratch`.
                let scratch = unsafe { &mut *rs.scratch.get() };
                let generation = rs.flush_gen.load(Ordering::Acquire);
                if generation != rs.flush_ack.load(Ordering::Relaxed) {
                    if let Some(eq) = &rs.eq {
                        // SAFETY: this is the callback thread.
                        unsafe { eq.live() }.reset();
                    }
                    rs.ring.discard();
                    rs.played.store(0, Ordering::Relaxed);
                    rs.flush_ack.store(generation, Ordering::Release);
                    rs.refilling.store(true, Ordering::Relaxed);
                }
                let ch = rs.channels;
                let piece = scratch.len() / ch;
                let mut got = 0;
                let mut peaks = [0f32; 2];
                // In pieces of the scratch size; once the ring runs short, the rest is silence.
                while got < frames {
                    let want = (frames - got).min(piece);
                    let n = if paused { 0 } else { rs.ring.pop(&mut scratch[..want * ch]) / ch };
                    let block = &mut scratch[..n * ch];
                    let peak = match &rs.eq {
                        // SAFETY: this is the callback thread.
                        Some(eq) => unsafe { eq.live() }.process(&eq.mailbox, block),
                        None => block.iter_mut().fold(0f32, |m, s| {
                            let p = m.max(s.abs());
                            *s = s.clamp(-1.0, 1.0);
                            p
                        }),
                    };
                    rs.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
                    let to = if n < want { frames } else { got + want };
                    let p = fill(buffers, got..to, n, |f, dc| source_sample(scratch, ch, f, dc));
                    peaks = [peaks[0].max(p[0]), peaks[1].max(p[1])];
                    got += n;
                    if n < want {
                        break;
                    }
                }
                st.meter(peaks);
                let played = rs.played.fetch_add(got as u64, Ordering::Relaxed) + got as u64;
                // The next track's first frame has reached the device: it is what plays now.
                let boundary = rs.boundary.load(Ordering::Acquire);
                if boundary != NO_BOUNDARY && played >= boundary {
                    slots.heard.store(slots.cur.load(Ordering::Acquire), Ordering::Release);
                    rs.played.store(played - boundary, Ordering::Relaxed);
                    rs.base_frame.store(0, Ordering::Relaxed);
                    rs.boundary.store(NO_BOUNDARY, Ordering::Release);
                    slots.switches.fetch_add(1, Ordering::AcqRel);
                }
                if got == frames {
                    rs.refilling.store(false, Ordering::Relaxed);
                } else if !paused
                    && !rs.refilling.load(Ordering::Relaxed)
                    && !rs.starved.load(Ordering::Relaxed)
                    && !rs.render_done.load(Ordering::Acquire)
                {
                    rs.underruns.fetch_add(1, Ordering::Relaxed);
                }
                if !paused && got < frames && rs.render_done.load(Ordering::Acquire) && rs.ring.available() == 0 {
                    st.finished.store(true, Ordering::Relaxed);
                }
                got
            }
        }
    }

    #[inline(always)]
    fn meter(&self, peaks: [f32; 2]) {
        for (m, p) in self.meter.iter().zip(peaks) {
            m.fetch_max(p.to_bits(), Ordering::Relaxed);
        }
    }
}

/// Write the device frames in `frames`: the first `avail` of them from `sample(i,
/// device_channel)` (i counted from the start of the range), silence after. Returns the peak
/// |sample| written to device channels 0 and 1.
#[inline(always)]
fn fill(
    buffers: &mut [OutBuf<'_>],
    frames: std::ops::Range<usize>,
    avail: usize,
    sample: impl Fn(usize, usize) -> f32,
) -> [f32; 2] {
    let mut peaks = [0f32; 2];
    let mut first_channel = 0; // device channel index of this buffer's first channel
    for buf in buffers.iter_mut() {
        let (nch, fmt) = (buf.channels, buf.format);
        if nch == 0 {
            continue;
        }
        let all = buf.data.chunks_exact_mut(nch * fmt.bytes());
        for (f, frame) in all.skip(frames.start).take(frames.len()).enumerate() {
            for (c, out) in frame.chunks_exact_mut(fmt.bytes()).enumerate() {
                let s = if f < avail { sample(f, first_channel + c) } else { 0.0 };
                if let Some(p) = peaks.get_mut(first_channel + c) {
                    *p = p.max(s.abs());
                }
                fmt.write(out, s);
            }
        }
        first_channel += nch;
    }
    peaks
}

/// DoP counterpart of [`fill`]: in the device frames `frames`, `avail` frames of DSD from
/// `payload(i, device_channel)`, then the DSD idle pattern, every frame under the next marker
/// in the alternation. Returns a rough level per channel (DSD bit density over 256-bit
/// windows), for the meters.
#[inline(always)]
fn fill_dop(
    buffers: &mut [OutBuf<'_>],
    frames: std::ops::Range<usize>,
    avail: usize,
    phase: &AtomicBool,
    payload: impl Fn(usize, usize) -> u16,
) -> [f32; 2] {
    const WINDOW: usize = 16; // frames → 256 bits per channel
    let start = phase.load(Ordering::Relaxed);
    let mut written = 0;
    let mut peaks = [0f32; 2];
    let mut ones = [0u32; 2];
    let mut first_channel = 0;
    for buf in buffers.iter_mut() {
        let (nch, fmt) = (buf.channels, buf.format);
        if nch == 0 {
            continue;
        }
        let mut n = 0;
        let all = buf.data.chunks_exact_mut(nch * fmt.bytes());
        for (f, frame) in all.skip(frames.start).take(frames.len()).enumerate() {
            let marker: u32 = if start ^ (f & 1 == 1) { 0xFA } else { 0x05 };
            for (c, out) in frame.chunks_exact_mut(fmt.bytes()).enumerate() {
                let dc = first_channel + c;
                let p = if f < avail { payload(f, dc) } else { DOP_IDLE };
                if dc < 2 {
                    ones[dc] += p.count_ones();
                }
                fmt.write_dop(out, marker << 16 | p as u32);
            }
            if first_channel == 0 && (f + 1) % WINDOW == 0 {
                for c in 0..2 {
                    let level = (ones[c] as f32 / (8 * WINDOW) as f32 - 1.0).abs() * PCM_GAIN as f32;
                    peaks[c] = peaks[c].max(level);
                    ones[c] = 0;
                }
            }
            n = f + 1;
        }
        written = written.max(n);
        first_channel += nch;
    }
    phase.store(start ^ (written & 1 == 1), Ordering::Relaxed);
    peaks
}

#[inline(always)]
fn dop_sample(track: &Track, frame: usize, device_channel: usize) -> u16 {
    if track.channels == 1 && device_channel < 2 {
        track.dop_payload(frame, 0)
    } else if device_channel < track.channels {
        track.dop_payload(frame, device_channel)
    } else {
        DOP_IDLE
    }
}

#[inline(always)]
fn track_sample(track: &Track, frame: usize, device_channel: usize) -> f32 {
    if track.channels == 1 && device_channel < 2 {
        track.sample(frame, 0) // mono → both speakers
    } else if device_channel < track.channels {
        track.sample(frame, device_channel)
    } else {
        0.0
    }
}

#[inline(always)]
fn source_sample(samples: &[f32], channels: usize, frame: usize, device_channel: usize) -> f32 {
    if channels == 1 && device_channel < 2 {
        samples[frame] // mono → both speakers
    } else if device_channel < channels {
        samples[frame * channels + device_channel]
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The processed path end to end, as a device would pull it: the EQ runs in the callback,
    /// takes edits live, and the compare bypass and the linear-phase FIR work there too.
    mod eq_path {
        use super::*;
        use crate::decode::TrackMeta;
        use crate::dsp::eq::Band;
        use crate::dsp::eq::FilterKind;

        const FS: u32 = 48_000;

        fn sine_track(secs: f64) -> Arc<Track> {
            let samples: Vec<f32> = (0..(FS as f64 * secs) as usize)
                .flat_map(|i| {
                    let v = ((2.0 * std::f64::consts::PI * 1000.0 * i as f64 / FS as f64).sin() * 0.1) as f32;
                    [v, v]
                })
                .collect();
            Arc::new(Track::from_samples(FS, 2, Some(24), &samples, TrackMeta::default()))
        }

        fn peak_eq(gain: f64, phase: Phase) -> EqPreset {
            EqPreset { phase, bands: vec![Band { kind: FilterKind::Peak, freq: 1000.0, gain, q: 1.0, ..Default::default() }], ..Default::default() }
        }

        fn start(eq: EqPreset) -> Playback {
            let opts = PlayOptions { dsp: DspSettings { eq: Some(eq), ..Default::default() }, ..Default::default() };
            let plan = Plan { target: FS as f64, start_frame: 0, lossy: None, resampled: false, processing: true, dop: false };
            Playback::new(sine_track(8.0), &opts, &plan, FS as f64).unwrap()
        }

        /// Pull `frames` from the callback, 512 at a time, waiting for the render thread like
        /// a real device's clock would. Returns the left channel.
        fn pull(p: &Playback, frames: usize) -> Vec<f32> {
            let rs = p.shared().ring.as_ref().unwrap();
            let mut out = Vec::new();
            let mut data = vec![0u8; 512 * 2 * 4];
            while out.len() < frames {
                while rs.ring.available() < 512 * 2 && !rs.render_done.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                p.shared().render(&mut [OutBuf { data: &mut data, channels: 2, format: OutFormat::F32 }]);
                out.extend(data.chunks_exact(8).map(|f| f32::from_le_bytes(f[..4].try_into().unwrap())));
            }
            out
        }

        fn level_db(x: &[f32]) -> f64 {
            20.0 * (x.iter().fold(0f32, |m, v| m.max(v.abs())) as f64 / 0.1).log10()
        }

        #[test]
        fn minimum_phase_edits_are_heard_within_a_buffer() {
            let p = start(peak_eq(6.0, Phase::Minimum));
            let a = pull(&p, 24_000);
            assert!((level_db(&a[12_000..]) - 6.0).abs() < 0.1, "{}", level_db(&a[12_000..]));
            assert!(p.update_eq(&peak_eq(-6.0, Phase::Minimum), false));
            let b = pull(&p, 6_000);
            // A 20 ms crossfade, then the new setting — no waiting for the render buffer.
            assert!((level_db(&b[2_048..]) + 6.0).abs() < 0.1, "{}", level_db(&b[2_048..]));
            let step = a.iter().chain(&b).collect::<Vec<_>>().windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
            assert!(step < 0.03, "click: {step}");
            // Compare: the level-matched bypass.
            let eq = peak_eq(-6.0, Phase::Minimum);
            assert!(p.update_eq(&eq, true));
            let c = pull(&p, 6_000);
            assert!((level_db(&c[2_048..]) - eq.loudness_db(FS as f64)).abs() < 0.1);
        }

        #[test]
        fn linear_phase_runs_on_the_fir_and_updates_after_the_buffer() {
            let p = start(peak_eq(6.0, Phase::Linear));
            let a = pull(&p, 48_000);
            assert!((level_db(&a[24_000..]) - 6.0).abs() < 0.1, "{}", level_db(&a[24_000..]));
            assert!(p.update_eq(&peak_eq(-6.0, Phase::Linear), false));
            let b = pull(&p, 96_000);
            assert!((level_db(&b[60_000..]) + 6.0).abs() < 0.1, "{}", level_db(&b[60_000..]));
            // A different layout can't go live: phase change → the caller restarts.
            assert!(!p.update_eq(&peak_eq(-6.0, Phase::Minimum), false));
        }

        /// Linear phase: the preamp travels with the bands through the FIR, so turning a boost
        /// and the preamp that made room for it off together never plays the boost without
        /// it (the preamp would otherwise jump ~0.7 s ahead of the bands).
        #[test]
        fn linear_phase_preamp_changes_with_the_bands() {
            let mut eq = peak_eq(6.0, Phase::Linear);
            eq.preamp = -6.0;
            let p = start(eq);
            let a = pull(&p, 48_000);
            assert!(level_db(&a[24_000..]).abs() < 0.1, "{}", level_db(&a[24_000..]));
            assert!(p.update_eq(&peak_eq(0.0, Phase::Linear), false));
            let b = pull(&p, 96_000);
            assert!(level_db(&b).abs() < 0.1, "{}", level_db(&b));
        }
    }

    /// Gapless playback: tracks joined end to end, as the device hears them.
    mod gapless {
        use super::*;
        use crate::decode::TrackMeta;

        /// Stereo, every sample distinct: (start + i) / 2^20 on the left, its negative on the right.
        fn ramp(rate: u32, start: usize, frames: usize) -> Arc<Track> {
            let samples: Vec<f32> = (start..start + frames).flat_map(|i| {
                let v = (i + 1) as f32 / 1_048_576.0;
                [v, -v]
            }).collect();
            Arc::new(Track::from_samples(rate, 2, Some(24), &samples, TrackMeta::default()))
        }

        fn playback(first: Arc<Track>, processing: bool) -> Playback {
            let plan = Plan { target: first.sample_rate as f64, start_frame: 0, lossy: None, resampled: false, processing, dop: first.is_dop() };
            Playback::new(first.clone(), &PlayOptions::default(), &plan, first.sample_rate as f64).unwrap()
        }

        /// One callback of `frames` stereo F32 frames; returns the left channel.
        fn callback(p: &Playback, frames: usize) -> Vec<f32> {
            let mut data = vec![0u8; frames * 8];
            p.shared().render(&mut [OutBuf { data: &mut data, channels: 2, format: OutFormat::F32 }]);
            data.chunks_exact(8).map(|f| f32::from_le_bytes(f[..4].try_into().unwrap())).collect()
        }

        fn expected(frames: usize) -> Vec<f32> {
            (0..frames).map(|i| (i + 1) as f32 / 1_048_576.0).collect()
        }

        #[test]
        fn direct_path_joins_tracks_sample_for_sample() {
            let (a, b) = (ramp(44_100, 0, 10_000), ramp(44_100, 10_000, 7_000));
            let p = playback(a.clone(), false);
            assert!(p.queue_next(b.clone()));
            assert!(Arc::ptr_eq(&p.upcoming().unwrap(), &b));
            let mut out = Vec::new();
            // 997 frames a callback: the join falls inside one.
            while out.len() < 20_000 {
                out.extend(callback(&p, 997));
                if out.len() < 10_000 {
                    assert!(Arc::ptr_eq(&p.heard(), &a));
                }
            }
            assert_eq!(&out[..17_000], &expected(17_000)[..]);
            assert!(out[17_000..].iter().all(|&v| v == 0.0));
            assert!(Arc::ptr_eq(&p.heard(), &b));
            assert_eq!(p.switches(), 1);
            assert!(p.is_finished());
            assert!((p.duration_secs() - 7_000.0 / 44_100.0).abs() < 1e-9);
            // A is no longer kept.
            assert_eq!(Arc::strong_count(&a), 1);
        }

        #[test]
        fn processed_path_joins_tracks_and_switches_when_heard() {
            // A is longer than the ring, so it is lined up before the render thread gets to its end.
            let (a, b) = (ramp(48_000, 0, 60_000), ramp(48_000, 60_000, 20_000));
            let p = playback(a.clone(), true);
            assert!(p.queue_next(b.clone()));
            let rs = p.shared().ring.as_ref().unwrap();
            let mut out = Vec::new();
            while out.len() < 90_000 {
                while rs.ring.available() < 512 * 2 && !rs.render_done.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                out.extend(callback(&p, 512));
                let heard_b = Arc::ptr_eq(&p.heard(), &b);
                // The switch follows what is heard, not what the render thread has reached.
                assert_eq!(heard_b, out.len() > 60_000, "at {}", out.len());
                if heard_b {
                    let want = (out.len() - 60_000) as f64 / 48_000.0;
                    assert!((p.position_secs() - want.min(p.duration_secs())).abs() < 1e-6);
                }
            }
            assert_eq!(&out[..80_000], &expected(80_000)[..]);
            assert!(p.is_finished());
            assert_eq!(p.underruns(), 0);
        }

        #[test]
        fn a_join_through_the_eq_does_not_click() {
            // One sine cut in two tracks: through a linear-phase EQ the join must not be heard.
            let fs = 48_000u32;
            let sine: Vec<f32> = (0..fs as usize * 2).flat_map(|i| {
                let v = ((2.0 * std::f64::consts::PI * 440.0 * i as f64 / fs as f64).sin() * 0.25) as f32;
                [v, v]
            }).collect();
            // Longer than the ring, so B is lined up before the render thread reaches the cut.
            let cut = 70_000 * 2;
            let a = Arc::new(Track::from_samples(fs, 2, Some(24), &sine[..cut], TrackMeta::default()));
            let b = Arc::new(Track::from_samples(fs, 2, Some(24), &sine[cut..], TrackMeta::default()));
            let eq = crate::dsp::eq::EqPreset {
                phase: Phase::Linear,
                bands: vec![crate::dsp::eq::Band { kind: crate::dsp::eq::FilterKind::Peak, freq: 2000.0, gain: -3.0, q: 1.0, ..Default::default() }],
                ..Default::default()
            };
            let opts = PlayOptions { dsp: DspSettings { eq: Some(eq), ..Default::default() }, ..Default::default() };
            let plan = Plan { target: fs as f64, start_frame: 0, lossy: None, resampled: false, processing: true, dop: false };
            let p = Playback::new(a, &opts, &plan, fs as f64).unwrap();
            assert!(p.queue_next(b));
            let rs = p.shared().ring.as_ref().unwrap();
            let mut out = Vec::new();
            while out.len() < 90_000 {
                while rs.ring.available() < 512 * 2 && !rs.render_done.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                out.extend(callback(&p, 512));
            }
            // 440 Hz at 0.25: neighbouring samples differ by at most ~0.0145.
            let step = out[1_000..90_000].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
            assert!(step < 0.016, "click at the join: {step}");
            let level = out[68_000..72_000].iter().fold(0f32, |m, v| m.max(v.abs()));
            assert!((level - 0.25).abs() < 0.01, "level at the join {level}");
        }

        #[test]
        fn dop_markers_alternate_across_the_join() {
            fn dop(frames: usize, fill: u8) -> Arc<Track> {
                let t = Track::new_dop(2_822_400, 2, TrackMeta::default(), frames);
                t.push_dop(&vec![fill; frames * 4]);
                t.finish(None);
                Arc::new(t)
            }
            let (a, b) = (dop(1_001, 0x69), dop(700, 0x96));
            let p = playback(a, false);
            assert!(p.queue_next(b));
            let mut markers = Vec::new();
            let mut payload = Vec::new();
            for _ in 0..4 {
                let mut data = vec![0u8; 512 * 2 * 3];
                p.shared().render(&mut [OutBuf { data: &mut data, channels: 2, format: OutFormat::I24Packed }]);
                for f in data.chunks(6) {
                    markers.push(f[2]);
                    payload.push(f[1]);
                }
            }
            assert!(markers.windows(2).all(|w| w[0] != w[1]), "markers break");
            assert!(payload[..1_001].iter().all(|&b| b == 0x69));
            assert!(payload[1_001..1_701].iter().all(|&b| b == 0x96));
        }

        #[test]
        fn a_different_rate_or_kind_does_not_join() {
            let p = playback(ramp(44_100, 0, 1_000), false);
            assert!(!p.queue_next(ramp(96_000, 0, 1_000)));
            let mono = Arc::new(Track::from_samples(44_100, 1, Some(16), &[0.0; 100], TrackMeta::default()));
            assert!(!p.queue_next(mono));
            assert!(p.upcoming().is_none());
            // Lined up, then the queue changed.
            assert!(p.queue_next(ramp(44_100, 0, 1_000)));
            p.clear_next();
            assert!(p.upcoming().is_none());
        }

        #[test]
        fn seeking_into_a_track_whose_end_is_already_rendered_asks_for_a_restart() {
            let (a, b) = (ramp(48_000, 0, 60_000), ramp(48_000, 60_000, 48_000));
            let p = playback(a, true);
            assert!(p.queue_next(b));
            assert!(p.seek(0.1), "A still being rendered: an ordinary seek");
            // Play until the render thread is on B while A's end is still in the ring.
            let slots = &p.shared().slots;
            let deadline = Instant::now() + Duration::from_secs(5);
            while slots.cur.load(Ordering::Acquire) == slots.heard.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline);
                callback(&p, 512);
                std::thread::sleep(Duration::from_micros(200));
            }
            assert!(!p.seek(0.1));
        }

        /// Far from what a long file holds in memory, seek says no (the app then opens the file
        /// anew there); close by, it jumps as usual.
        #[test]
        fn a_seek_far_from_the_window_asks_for_a_restart() {
            let path = std::env::temp_dir().join(format!("hifi-seekwin-{}.wav", std::process::id()));
            crate::decode::tests::ramp_wav(&path, 48_000, 48_000 * 60);
            let t = crate::decode::open_windowed(&path, 4 << 20); // ≈ 11 s of the minute
            let p = playback(t.clone(), false);
            // Play 40 s, as fast as the decoder keeps up.
            let mut played = 0;
            while played < 48_000 * 40 {
                if t.ready_frames() > played + 4_096 || t.is_complete() {
                    callback(&p, 4_096);
                    played += 4_096;
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            assert!(t.held_from() > 0);
            assert!(!p.seek(1.0), "the start is long gone");
            assert!(!p.seek(59.0), "the end isn't decoded yet");
            assert!(p.seek(39.0), "just behind: still held");
            let out = callback(&p, 16);
            let want = ((39 * 48_000) % 30_000) as f32 / 32_768.0;
            assert_eq!(out[0], want);
            drop(p);
            let _ = std::fs::remove_file(&path);
        }

        /// The render thread reads a windowed track right through, every sample as decoded.
        #[test]
        fn processing_plays_a_windowed_track_whole() {
            let path = std::env::temp_dir().join(format!("hifi-procwin-{}.wav", std::process::id()));
            crate::decode::tests::ramp_wav(&path, 48_000, 48_000 * 30);
            let t = crate::decode::open_windowed(&path, 2 << 20); // ≈ 5 s of the 30
            let p = playback(t.clone(), true);
            let rs = p.shared().ring.as_ref().unwrap();
            let mut out = Vec::new();
            while !p.is_finished() {
                if rs.ring.available() >= 1_024 * 2 || rs.render_done.load(Ordering::Acquire) {
                    out.extend(callback(&p, 1_024));
                } else {
                    std::thread::sleep(Duration::from_micros(200));
                }
            }
            assert!(out.len() >= 48_000 * 30);
            for (f, &v) in out[..48_000 * 30].iter().enumerate() {
                assert_eq!(v, ((f % 30_000) as f32) / 32_768.0, "frame {f}");
            }
            assert!(t.held_bytes() <= (2 << 20) + 3 * (1 << 16) * 8);
            drop(p);
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn a_track_lined_up_after_the_tail_is_out_is_refused() {
            let p = playback(ramp(48_000, 0, 2_000), true);
            let rs = p.shared().ring.as_ref().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !rs.tail_out.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(!p.queue_next(ramp(48_000, 0, 1_000)));
        }
    }

    /// A device asking for more than the scratch holds gets all of it, not silence at the end.
    #[test]
    fn a_callback_bigger_than_the_scratch_is_served_whole() {
        let fs = 48_000u32;
        let samples: Vec<f32> = (0..fs as usize * 4).flat_map(|i| [((i % 100) as f32 + 1.0) / 1000.0; 2]).collect();
        let track = Arc::new(Track::from_samples(fs, 2, Some(24), &samples, crate::decode::TrackMeta::default()));
        let opts = PlayOptions { dsp: DspSettings { crossfeed: None, ..Default::default() }, ..Default::default() };
        let plan = Plan { target: fs as f64, start_frame: 0, lossy: None, resampled: false, processing: true, dop: false };
        let p = Playback::new(track, &opts, &plan, fs as f64).unwrap();
        let rs = p.shared().ring.as_ref().unwrap();
        // Bigger than the scratch, smaller than the ring (which holds ~0.68 s here).
        let frames = MAX_CALLBACK_FRAMES + 5_000;
        assert!(frames * 2 < rs.ring.capacity());
        while rs.ring.available() < frames * 2 {
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut data = vec![0u8; frames * 2 * 4];
        p.shared().render(&mut [OutBuf { data: &mut data, channels: 2, format: OutFormat::F32 }]);
        let left: Vec<f32> = data.chunks_exact(8).map(|f| f32::from_le_bytes(f[..4].try_into().unwrap())).collect();
        assert!(left.iter().all(|&v| v > 0.0), "silence at {:?}", left.iter().position(|&v| v <= 0.0));
        assert_eq!(rs.played.load(Ordering::Relaxed), frames as u64);
    }

    /// The bit-perfect claim, checked exhaustively: every 24-bit integer survives
    /// decode (i24 / 2^23 → f32) and output (f32 → packed i24) unchanged.
    #[test]
    fn every_24bit_sample_round_trips_exactly() {
        let mut out = [0u8; 3];
        for v in -(1i32 << 23)..(1i32 << 23) {
            OutFormat::I24Packed.write(&mut out, v as f32 / 8_388_608.0);
            let back = i32::from_le_bytes([0, out[0], out[1], out[2]]) >> 8;
            assert_eq!(back, v);
        }
    }

    /// A 16-bit source on a 24-bit DAC is still bit-perfect: same integer, 8 zero bits below.
    #[test]
    fn every_16bit_sample_lands_as_shifted_24bit() {
        let mut out = [0u8; 3];
        for v in i16::MIN..=i16::MAX {
            OutFormat::I24Packed.write(&mut out, v as f32 / 32_768.0);
            let back = i32::from_le_bytes([0, out[0], out[1], out[2]]) >> 8;
            assert_eq!(back, (v as i32) << 8);
        }
    }

    /// DoP words land in every supported container with the marker in the top byte and the
    /// 16 DSD bits right below it, and the markers alternate across callback boundaries.
    #[test]
    fn dop_words_and_markers() {
        let word = 0xFA_1234u32;
        let mut b3 = [0u8; 3];
        OutFormat::I24Packed.write_dop(&mut b3, word);
        assert_eq!(b3, [0x34, 0x12, 0xFA]);
        let mut b4 = [0u8; 4];
        OutFormat::I32 { bits: 32 }.write_dop(&mut b4, word);
        assert_eq!(b4, [0x00, 0x34, 0x12, 0xFA]);
        OutFormat::F32.write_dop(&mut b4, word);
        let i = (f32::from_le_bytes(b4) as f64 * 2f64.powi(31)) as i64 as i32 as u32;
        assert_eq!(i, word << 8, "float path must convert back to the exact word");
        OutFormat::I32 { bits: 24 }.write_dop(&mut b4, 0x05_ABCD);
        assert_eq!(i32::from_le_bytes(b4), 0x05_ABCD);

        // Two callbacks of 3 frames each, stereo I24: markers must run 05 FA 05 | FA 05 FA.
        let phase = AtomicBool::new(false);
        let mut markers = Vec::new();
        for _ in 0..2 {
            let mut data = vec![0u8; 3 * 2 * 3];
            let mut bufs = [OutBuf { data: &mut data, channels: 2, format: OutFormat::I24Packed }];
            fill_dop(&mut bufs, 0..3, 2, &phase, |f, c| (f * 2 + c) as u16);
            for frame in data.chunks(6) {
                assert_eq!(frame[2], frame[5], "both channels share a frame's marker");
                markers.push(frame[2]);
            }
            // Frame 2 is past `avail`: the idle pattern.
            assert_eq!(&data[12..14], &DOP_IDLE.to_le_bytes());
            assert_eq!(&data[0..2], &[0, 0]);
            assert_eq!(&data[3..5], &[1, 0]);
        }
        assert_eq!(markers, [0x05, 0xFA, 0x05, 0xFA, 0x05, 0xFA]);
    }

    #[test]
    fn full_scale_clamps_instead_of_wrapping() {
        let mut out = [0u8; 2];
        OutFormat::I16.write(&mut out, 1.0);
        assert_eq!(i16::from_le_bytes(out), i16::MAX);
        OutFormat::I16.write(&mut out, -1.0);
        assert_eq!(i16::from_le_bytes(out), i16::MIN);
    }
}
