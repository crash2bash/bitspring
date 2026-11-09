//! Playback, independent of the operating system.
//!
//! Everything between the decoded track and the device buffers lives here: the processing
//! chain on its render thread, the lock-free hand-off to the realtime callback, DoP packing,
//! sample format conversion, position, seek and meters. A backend (CoreAudio on macOS, WASAPI
//! on Windows) only finds the device, sets its rate and format, and calls
//! [`Shared::render`] from its realtime callback with the device's buffers.

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::decode::{DsdSource, Track};
use crate::dsd::{DOP_IDLE, PCM_GAIN};
use crate::dsp::crossfeed::CrossfeedLevel;
use crate::dsp::lossy::LossyPlan;
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
    /// Name of the EQ profile applied, if any.
    pub eq: Option<String>,
    pub crossfeed: Option<CrossfeedLevel>,
    /// The source is MP3/AAC/Vorbis/Opus.
    pub source_lossy: bool,
    /// What the lossy enhancers do to this track (None = nothing).
    pub lossy: Option<LossyPlan>,
    /// The source is DSD: its rate, and whether it goes out as DoP or was converted to PCM.
    pub dsd: Option<DsdSource>,
    pub bit_perfect: bool,
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
    let bits_ok = match (physical, track.bits_per_sample) {
        (Some(p), Some(b)) => (p.float || p.bits >= b) && out.bits() >= b,
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
        crossfeed: opts.dsp.crossfeed,
        source_lossy: track.lossy,
        lossy: plan.lossy,
        dsd: track.dsd,
        // DoP gets this far only with the exact rate, a ≥24-bit integer format and exclusive
        // access (the backend checked).
        bit_perfect: (plan.dop && transport.can_be_bit_perfect())
            || !plan.processing
                && (device_rate - src_hz).abs() < 0.5
                && exclusive
                && bits_ok
                && transport.can_be_bit_perfect()
                && track.channels <= 2
                && track.bits_per_sample.is_some_and(|b| b <= 24),
    }
}

// ---------------------------------------------------------------------------------------------
// Playback state shared with the realtime callback
// ---------------------------------------------------------------------------------------------

const NO_SEEK: usize = usize::MAX;
/// Largest callback buffer we serve, in frames (both CoreAudio's and WASAPI's are far below).
pub(crate) const MAX_CALLBACK_FRAMES: usize = 16_384;

/// State shared with the realtime thread. Only atomics and immutable data: the callback
/// never locks, allocates, or blocks.
pub(crate) struct Shared {
    track: Arc<Track>,
    /// Direct mode: next source frame to play.
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
    /// callbacks, pauses and seeks — a break in the alternation makes the DAC drop out of DSD.
    dop_phase: AtomicBool,
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
    /// Device frames played since `base_frame` (reset on seek).
    played: AtomicU64,
    /// Source frame where the current stretch of output started.
    base_frame: AtomicUsize,
    /// Seek handshake: render bumps `flush_gen`, the callback discards the ring and echoes it.
    flush_gen: AtomicU64,
    flush_ack: AtomicU64,
    seek_req: AtomicUsize,
    render_done: AtomicBool,
    quit: AtomicBool,
    /// Peak |sample| after processing, as f32 bits (for a clipping indicator).
    peak: AtomicU32,
    /// Callbacks that found the ring short while the render thread wasn't finished — each
    /// one is an audible dropout. Should stay 0.
    underruns: AtomicU64,
    /// Render thread is waiting for the decoder (a stream still downloading): silence then is
    /// buffering, not a dropout of ours.
    starved: AtomicBool,
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
}

impl Playback {
    /// Set up state (and the render thread when `plan.processing`) for a device running at
    /// `device_rate`. Returns once the ring has a head start, so IO can begin right away.
    pub(crate) fn new(track: Arc<Track>, opts: &PlayOptions, plan: &Plan, device_rate: f64) -> Result<Self> {
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
                flush_gen: AtomicU64::new(0),
                flush_ack: AtomicU64::new(0),
                seek_req: AtomicUsize::new(NO_SEEK),
                render_done: AtomicBool::new(false),
                quit: AtomicBool::new(false),
                peak: AtomicU32::new(0),
                underruns: AtomicU64::new(0),
                starved: AtomicBool::new(false),
            });
            let chain = Chain::new(&opts.dsp, plan.lossy.as_ref(), track.sample_rate, device_rate.round() as u32, channels);
            let handle = {
                let (track, rs) = (track.clone(), rs.clone());
                let start = plan.start_frame;
                std::thread::Builder::new()
                    .name("hifi-render".into())
                    .spawn(move || render_loop(track, rs, chain, start))?
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
            track,
            pos: AtomicUsize::new(plan.start_frame),
            paused: AtomicBool::new(opts.start_paused),
            finished: AtomicBool::new(false),
            ring,
            meter: [AtomicU32::new(0), AtomicU32::new(0)],
            dop: plan.dop,
            dop_phase: AtomicBool::new(false),
        });
        Ok(Self { shared, render })
    }

    /// Stable pointer for an OS callback's user data. Valid until this `Playback` drops;
    /// the backend must stop its callback before that.
    pub(crate) fn shared(&self) -> &Shared {
        &self.shared
    }

    pub fn position_secs(&self) -> f64 {
        let st = &self.shared;
        let pos = match &st.ring {
            None => st.pos.load(Ordering::Relaxed) as f64 / st.track.sample_rate as f64,
            Some(rs) => {
                rs.base_frame.load(Ordering::Relaxed) as f64 / rs.source_rate
                    + rs.played.load(Ordering::Relaxed) as f64 / rs.device_rate
            }
        };
        pos.min(self.duration_secs())
    }

    pub fn duration_secs(&self) -> f64 {
        self.shared.track.duration_secs()
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

    pub fn seek(&self, secs: f64) {
        let t = &self.shared.track;
        let frame = ((secs.max(0.0) * t.sample_rate as f64) as usize).min(t.frames());
        match &self.shared.ring {
            None => self.shared.pos.store(frame, Ordering::Relaxed),
            Some(rs) => rs.seek_req.store(frame, Ordering::Release),
        }
        self.shared.finished.store(t.is_complete() && frame >= t.frames(), Ordering::Relaxed);
    }

    /// Playback is waiting for audio that hasn't been decoded/downloaded yet.
    pub fn is_buffering(&self) -> bool {
        let t = &self.shared.track;
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
        let t = &self.shared.track;
        t.ready_frames() as f64 / t.sample_rate as f64
    }

    /// The decoder stopped on an error (a dropped connection, a corrupt file).
    pub fn source_error(&self) -> Option<String> {
        self.shared.track.error()
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
fn render_loop(track: Arc<Track>, rs: Arc<RingState>, mut chain: Chain, start_frame: usize) {
    const BLOCK: usize = 2048;
    let mut src = start_frame;
    let mut out: Vec<f32> = Vec::new();
    let mut sent = 0usize;
    let nap = || std::thread::sleep(Duration::from_millis(2));

    while !rs.quit.load(Ordering::Acquire) {
        let seek = rs.seek_req.swap(NO_SEEK, Ordering::AcqRel);
        if seek != NO_SEEK {
            src = seek.min(track.frames());
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

        // Read `complete` before the slice: if it was already complete, the slice is final.
        let complete = track.is_complete();
        let input = track.slice(src, BLOCK);
        if input.is_empty() {
            if complete {
                rs.render_done.store(true, Ordering::Release);
            } else {
                rs.starved.store(true, Ordering::Relaxed);
            }
            nap();
            continue;
        }
        rs.starved.store(false, Ordering::Relaxed);
        src += input.len() / track.channels;
        let last = complete && src >= track.ready_frames();
        let processed = chain.process(input, last);
        out.clear();
        out.extend(processed.iter().map(|&v| v.clamp(-1.0, 1.0) as f32));
        sent = 0;
        rs.peak.store((chain.peak as f32).to_bits(), Ordering::Relaxed);
    }
}

impl Shared {
    /// Fill the device's buffers for one callback. Realtime-safe: no locks, no allocation.
    #[inline]
    pub(crate) fn render(&self, buffers: &mut [OutBuf<'_>]) {
        let st = self;
        let paused = st.paused.load(Ordering::Relaxed);
        let frames = buffers
            .iter()
            .find(|b| b.channels > 0)
            .map(|b| b.data.len() / (b.channels * b.format.bytes()))
            .unwrap_or(0);

        match &st.ring {
            // Bit-perfect path: straight from the decoded track.
            None => {
                let track = &*st.track;
                // `complete` first: if decoding had finished, `ready` read after it is final.
                let complete = track.is_complete();
                let ready = track.ready_frames();
                let pos = st.pos.load(Ordering::Relaxed);
                // Past `ready` on a track still decoding = buffering: play silence, keep position.
                let avail = if paused { 0 } else { ready.saturating_sub(pos).min(frames) };
                let peaks = if st.dop {
                    fill_dop(buffers, avail, &st.dop_phase, |f, dc| dop_sample(track, pos + f, dc))
                } else {
                    fill(buffers, avail, |f, dc| track_sample(track, pos + f, dc))
                };
                st.meter(peaks);
                if !paused {
                    let new_pos = pos + avail;
                    // compare_exchange so a seek() that landed mid-callback isn't overwritten.
                    let _ = st.pos.compare_exchange(pos, new_pos, Ordering::Relaxed, Ordering::Relaxed);
                    if complete && new_pos >= ready {
                        st.finished.store(true, Ordering::Relaxed);
                    }
                }
            }
            // Processed path: pre-rendered samples from the ring.
            Some(rs) => {
                // SAFETY: only this (IO) thread ever touches `scratch`.
                let scratch = unsafe { &mut *rs.scratch.get() };
                let generation = rs.flush_gen.load(Ordering::Acquire);
                if generation != rs.flush_ack.load(Ordering::Relaxed) {
                    rs.ring.discard();
                    rs.played.store(0, Ordering::Relaxed);
                    rs.flush_ack.store(generation, Ordering::Release);
                    rs.refilling.store(true, Ordering::Relaxed);
                }
                let ch = rs.channels;
                let want = (frames * ch).min(scratch.len());
                let got = if paused { 0 } else { rs.ring.pop(&mut scratch[..want]) / ch };
                let peaks = fill(buffers, got, |f, dc| source_sample(scratch, ch, f, dc));
                st.meter(peaks);
                rs.played.fetch_add(got as u64, Ordering::Relaxed);
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

/// Write `avail` frames from `sample(frame, device_channel)` into every buffer, then silence
/// for the rest. Returns the peak |sample| written to device channels 0 and 1.
#[inline(always)]
fn fill(buffers: &mut [OutBuf<'_>], avail: usize, sample: impl Fn(usize, usize) -> f32) -> [f32; 2] {
    let mut peaks = [0f32; 2];
    let mut first_channel = 0; // device channel index of this buffer's first channel
    for buf in buffers.iter_mut() {
        let (nch, fmt) = (buf.channels, buf.format);
        if nch == 0 {
            continue;
        }
        for (f, frame) in buf.data.chunks_exact_mut(nch * fmt.bytes()).enumerate() {
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

/// DoP counterpart of [`fill`]: `avail` frames of DSD from `payload(frame, device_channel)`,
/// then the DSD idle pattern, every frame under the next marker in the alternation.
/// Returns a rough level per channel (DSD bit density over 256-bit windows), for the meters.
#[inline(always)]
fn fill_dop(buffers: &mut [OutBuf<'_>], avail: usize, phase: &AtomicBool, payload: impl Fn(usize, usize) -> u16) -> [f32; 2] {
    const WINDOW: usize = 16; // frames → 256 bits per channel
    let start = phase.load(Ordering::Relaxed);
    let mut frames = 0;
    let mut peaks = [0f32; 2];
    let mut ones = [0u32; 2];
    let mut first_channel = 0;
    for buf in buffers.iter_mut() {
        let (nch, fmt) = (buf.channels, buf.format);
        if nch == 0 {
            continue;
        }
        let mut n = 0;
        for (f, frame) in buf.data.chunks_exact_mut(nch * fmt.bytes()).enumerate() {
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
        frames = frames.max(n);
        first_channel += nch;
    }
    phase.store(start ^ (frames & 1 == 1), Ordering::Relaxed);
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
            fill_dop(&mut bufs, 2, &phase, |f, c| (f * 2 + c) as u16);
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
