//! Playback, independent of the operating system.
//!
//! Everything between the decoded track and the device buffers lives here: the processing
//! chain on its render thread, the lock-free hand-off to the realtime callback, DoP packing
//! (of a DSD track's own bits, or of PCM the render thread encodes to DSD), sample format
//! conversion, position, seek and meters. A backend (CoreAudio on macOS, WASAPI
//! on Windows, ALSA on Linux) only finds the device, sets its rate and format, and calls
//! [`Shared::render`] from its realtime callback with the device's buffers.

use crate::channels::{Route, Speaker};
use std::borrow::Cow;
use std::cell::UnsafeCell;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::decode::{DsdSource, Track};
use crate::dsd::{DOP_IDLE, PCM_GAIN};
use crate::dsp::crossfeed::CrossfeedLevel;
use crate::dsp::dsd_encode::{DsdOut, Encoder};
use crate::dsp::lossy::LossyPlan;
use crate::dsp::loudness::Level;
use crate::dsp::eq::{EqPreset, EqStage, IirPlan, Phase};
use crate::dsp::fir::{FirControl, FirDesigner};
use crate::dsp::upsample::FilterSpec;
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
#[cfg(target_os = "linux")]
pub use crate::linux_out::{Device, DeviceInfo, Player, output_devices};

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
    /// Fade over a few milliseconds where playback is cut off or picked up mid-sound (pause,
    /// seek, stop), instead of a click. A track playing through, and a gapless join, are never
    /// touched. Off for a network stream: there the player on the other end does the pausing.
    pub fades: bool,
}

impl Default for PlayOptions {
    fn default() -> Self {
        Self { exclusive: true, dsp: DspSettings::default(), start_at: 0.0, start_paused: false, fades: true }
    }
}

/// What actually happens to the signal on its way to the DAC — shown to the user so
/// "bit-perfect" is a verifiable claim, not a badge.
#[derive(Debug, Clone)]
pub struct SignalPath {
    /// The source's channels: "Stereo", "5.1"…
    pub layout: String,
    /// The source is folded down to fewer speakers on the way.
    pub downmix: Option<Downmix>,
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
    /// The long upsampling filter, when it runs for this playback (see
    /// [`crate::dsp::long_filter`]): its kind and its length as built. None = the standard one.
    pub upsample_filter: Option<FilterSpec>,
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
    /// PCM encoded to DSD on the way and sent as DoP: the rate and modulator it runs with.
    /// None when it isn't, also where it was asked for and the device couldn't take it.
    pub dsd_out: Option<DsdOut>,
    /// The gain this track plays at (volume levelling, peak headroom), when one applies.
    pub level: Option<Level>,
    pub bit_perfect: bool,
    /// Significant bits of the samples we write (for a following track's bit-perfect check).
    out_bits: u32,
    /// Bit-perfect apart from the source's bit depth (rate, access, transport, no processing).
    pcm_clean: bool,
    /// This playback applies each track's level.
    leveled: bool,
}

/// A multichannel source folded down to the speakers the device has.
#[derive(Debug, Clone, PartialEq)]
pub struct Downmix {
    /// "Stereo", "5.1"…
    pub to: String,
    /// Overall level change that keeps the mix from clipping, in dB.
    pub gain_db: f64,
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
            level: if self.leveled { track.level() } else { None },
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
    /// The wire carries DoP: a DSD track's own bits sent straight, or PCM encoded (`encode`).
    /// The device must run at `target` in a format that carries the words untouched.
    pub dop: bool,
    /// PCM is encoded to DSD on the render thread (after the whole chain and the EQ) and the
    /// words go out as DoP. A DSD track played straight is `dop` with this None.
    pub encode: Option<DsdOut>,
    /// How the track's channels reach the device's (straight, or folded down).
    pub route: Route,
}

/// The device couldn't take DoP for `dsd_hz` (its wire rate is `dsd_hz / 16`): what a backend
/// returns when the format it got can't carry the words, or the rate isn't the one planned.
pub(crate) fn dop_error(device_name: &str, dsd_hz: u32) -> anyhow::Error {
    crate::UserError::DopRefused {
        device: device_name.trim().to_string(),
        dsd: crate::dsd::dsd_name(dsd_hz),
        khz: dsd_hz as f64 / 16.0 / 1000.0,
    }
    .into()
}

/// The DSD rate that would go over the wire as DoP for `track`: its own when it is a DSD
/// track sent straight, else the rate PCM is to be encoded at, if that is asked for. What a
/// backend checks the device's DoP support against, before planning.
pub(crate) fn wire_dsd_rate(track: &Track, opts: &PlayOptions) -> Option<u32> {
    if track.is_dop() {
        Some(track.dsd.map_or(track.sample_rate * 16, |d| d.rate))
    } else {
        opts.dsp.dsd_out.map(|o| o.rate.hz())
    }
}

/// The settings as this playback runs them: `dsd_out` is what the plan decided (`encode`),
/// whatever was asked for. Everything after `plan` goes by these, so a playback that couldn't
/// encode is exactly the one planned without it.
fn effective(dsp: &DspSettings, encode: Option<DsdOut>) -> Cow<'_, DspSettings> {
    if dsp.dsd_out == encode {
        Cow::Borrowed(dsp)
    } else {
        Cow::Owned(DspSettings { dsd_out: encode, ..dsp.clone() })
    }
}

/// `rates`: what the device can run at. `dop_ok`: the device could carry this DoP track.
/// `speakers`: what each device channel feeds (see [`crate::channels`]).
pub(crate) fn plan(track: &Track, opts: &PlayOptions, rates: &[RateRange], device_name: &str, dop_ok: bool, speakers: &[Speaker]) -> Result<Plan> {
    let src_hz = track.sample_rate as f64;
    let route = Route::new(&track.layout, speakers);
    // DoP: the DSD bits must reach the DAC exactly, so no processing, no rate change and no
    // folding channels together.
    let dop = track.is_dop();
    let dsd_hz = wire_dsd_rate(track, opts).unwrap_or(0);
    if dop && (!opts.exclusive || !dop_ok || !route.is_direct()) {
        return Err(dop_error(device_name, dsd_hz));
    }
    // PCM encoded to DSD: only with the device to ourselves and able to carry DoP at the wire
    // rate (`dop_ok` is for that rate: see `wire_dsd_rate`). Otherwise the playback is planned
    // exactly as if it weren't asked for, without an error: the app says why, and the signal
    // path shows what plays. A DSD track sent straight ignores it.
    let encode = opts.dsp.dsd_out.filter(|out| {
        let wire = out.rate.wire_rate();
        !dop && opts.exclusive
            && dop_ok
            && rates.iter().any(|r| r.contains(wire as f64))
            && crate::dsp::resample::phases(track.sample_rate, wire) <= crate::dsp::resample::MAX_PHASES
    });
    let dsp = effective(&opts.dsp, encode);
    let mut target = match choose_device_rate(track.sample_rate, rates) {
        RateDecision::BitPerfect(hz) | RateDecision::Resample { device_hz: hz } => hz,
        RateDecision::Unsupported => bail!(crate::UserError::NoRates { device: device_name.trim().to_string() }),
    };
    if let Upsample::UpTo(max_hz) = dsp.upsample {
        if !dop {
            target = upsample_target(track.sample_rate, rates, max_hz);
        }
    }
    // The DSD bits go out at their own rate or not at all: the backends check the device got
    // `target`, so it must be that rate.
    if dop && (target - src_hz).abs() >= 0.5 {
        return Err(dop_error(device_name, dsd_hz));
    }
    if let Some(out) = encode {
        // The chain always upsamples to the wire rate, whatever "upsample to" says.
        target = out.rate.wire_rate() as f64;
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
    let processing = encode.is_some() || (!dop && (resampled || dsp.is_active() || lossy.is_some()));
    if resampled && crate::dsp::resample::phases(track.sample_rate, target as u32) > crate::dsp::resample::MAX_PHASES {
        bail!(crate::UserError::Resample { from: track.sample_rate, to: target as u32 });
    }
    Ok(Plan { target, start_frame, lossy, resampled, processing, dop: dop || encode.is_some(), encode, route })
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
    let dsp = effective(&opts.dsp, plan.encode);
    // (Never for a DSD track sent straight: its bits can't be scaled. PCM encoded to DSD is
    // levelled ahead of the encoder.)
    let leveled = plan.processing && opts.dsp.levels();
    let pcm_clean = !plan.processing
        && (device_rate - src_hz).abs() < 0.5
        && exclusive
        && transport.can_be_bit_perfect()
        && plan.route.is_direct();
    // (Up to 24 bits: f32 holds those exactly.)
    let bits_ok = match (physical, track.bits_per_sample) {
        (Some(p), Some(b)) => (p.float || p.bits >= b) && out.bits() >= b && b <= 24,
        _ => false,
    };
    SignalPath {
        layout: crate::channels::name(&track.layout),
        downmix: plan.route.folds().then(|| Downmix {
            to: crate::channels::name(&plan.route.program_layout),
            gain_db: plan.route.gain_db,
        }),
        source_rate: track.sample_rate,
        source_bits: track.bits_per_sample,
        device_rate,
        physical,
        integer_mode: out != OutFormat::F32,
        exclusive,
        transport,
        resampled: plan.resampled,
        // As the chain decides it, from the rate the device actually runs at.
        upsample_filter: if plan.processing { crate::dsp::long_filter(&dsp, track.sample_rate, device_rate.round() as u32) } else { None },
        eq: opts.dsp.eq.as_ref().map(|p| p.name.clone()),
        eq_linear: opts.dsp.eq.as_ref().is_some_and(|p| p.phase == Phase::Linear),
        convolution: opts.dsp.convolution.as_ref().map(|c| c.0.name.clone()),
        crossfeed: opts.dsp.crossfeed,
        source_lossy: track.lossy,
        lossy: plan.lossy,
        dsd: track.dsd,
        dsd_out: plan.encode,
        level: if leveled { track.level() } else { None },
        // DoP gets this far only with the exact rate, a ≥24-bit integer format and exclusive
        // access (the backend checked). Only a DSD track's own bits are bit-perfect: PCM
        // encoded to DSD is a new signal.
        bit_perfect: (plan.dop && plan.encode.is_none() && track.is_dop() && transport.can_be_bit_perfect()) || pcm_clean && bits_ok,
        out_bits: out.bits(),
        pcm_clean,
        leveled,
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
    /// A track to carry on with in place of the one playing, at the same position (see
    /// `Playback::swap`). Taken by the side that reads the source, which moves it to `cur`
    /// and `heard`.
    swap: AtomicPtr<Track>,
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
    /// The track is raw DSD: the callback packs it as DoP, straight from the track. (False
    /// when PCM is encoded: those words come through the ring, see `RingState::dop_words`.)
    dop: bool,
    /// Track channels → device channels.
    route: Route,
    /// Callback-only: marker of the next DoP frame (false = 0x05, true = 0xFA). Runs on across
    /// callbacks, pauses, seeks, dropouts and tracks — a break in the alternation makes the
    /// DAC drop out of DSD.
    dop_phase: AtomicBool,
    /// The lossy enhancers are on: they measure each track, so no track may follow another
    /// in the same chain when either is lossy.
    lossy_enhancers: bool,
    /// Length of a fade in device frames (see [`PlayOptions::fades`]); 0 = no fades now.
    /// Set from the control side (`Playback::set_fades`), read by the callback.
    fade_len: AtomicU32,
    /// What `fade_len` is with fades on; 0 = this playback never fades (DoP: the DSD bits
    /// can't be scaled, and the DAC mutes by itself; PCM encoded to DSD fades in ahead of the
    /// encoder instead, see `Encode`).
    fade_max: u32,
    /// Where the fade stands: 0 (silent) ..= `fade_len` (full level). Written by the callback
    /// only.
    fade: AtomicU32,
    /// Sound is going out (written by the callback only). Only then is there something to
    /// fade out: a playback that starts paused, or is still waiting for its first audio,
    /// has made no sound yet.
    sounding: AtomicBool,
}

/// How long a fade takes.
const FADE_SECS: f64 = 0.010;

/// Where the fade stands at the end of a callback. With fades on, where the callback left it.
/// With fades off it follows the sound instead — full while it plays, down once it has
/// stopped — so that turning fades on in the middle of things neither dips what is playing
/// nor replays a tail of what was cut.
#[inline(always)]
fn settled(level: u32, fade_len: u32, fade_max: u32, was_sounding: bool, sounding: bool) -> u32 {
    if fade_len > 0 {
        level
    } else if sounding {
        fade_max
    } else if was_sounding {
        0
    } else {
        level
    }
}

/// The gain `pos` frames into a fade of `len`: half a cosine, so it starts and ends without a
/// corner. Exactly 1.0 at the end (and a sample times 1.0 is that sample, bit for bit).
#[inline(always)]
fn fade_gain(pos: u32, len: u32) -> f32 {
    if pos >= len {
        return 1.0;
    }
    0.5 - 0.5 * (std::f32::consts::PI * pos as f32 / len as f32).cos()
}

/// Hand-off between the render thread (producer) and the callback (consumer).
struct RingState {
    /// Processed samples at the device rate, program channels interleaved. When `dop_words`,
    /// DoP payloads instead: each 16-bit word the encoder made, as its numeric value (0..=65535
    /// is exact in f32; the callback takes it back with `as u16`).
    ring: Ring,
    /// The ring carries DSD words, not PCM (see [`Plan::encode`]): the render thread has run
    /// the EQ, the clamp, the fade-in and the encoder; the callback only packs the words as
    /// DoP, under the next markers.
    dop_words: bool,
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
    /// Peak |sample| after processing, as f32 bits (for a clipping indicator). Written by the
    /// side that runs the EQ: the callback, or the render thread when it encodes.
    peak: AtomicU32,
    /// Callbacks that found the ring short while the render thread wasn't finished — each
    /// one is an audible dropout. Should stay 0.
    underruns: AtomicU64,
    /// Render thread is waiting for the decoder (a stream still downloading): silence then is
    /// buffering, not a dropout of ours.
    starved: AtomicBool,
    /// The render thread's processing time over the audio it handled, as f32 bits (see
    /// [`Playback::load`]). With the EQ and the encoder when it encodes.
    load: AtomicU32,
    /// The parametric EQ, run by the callback on what it pops from the ring — or, when
    /// `dop_words`, by the render thread ahead of the encoder (and then never by the callback).
    eq: Option<EqStage>,
    /// The render thread's FIR stage (linear-phase EQ, curves, convolution).
    fir: Option<Arc<FirControl>>,
}

// SAFETY: `scratch` is touched only by the callback thread; `eq`'s live half by one thread only
// (see its field); everything else is atomic or `Ring` (itself SPSC-safe: render thread
// produces, callback consumes).
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
            swap: AtomicPtr::new(std::ptr::null_mut()),
        });
        let (ring, render) = if plan.processing {
            // What runs is what the plan decided, not what was asked for (see `effective`).
            let dsp = effective(&opts.dsp, plan.encode);
            // The chain works on the program: the track's channels, or the speakers it is
            // folded down to (so EQ and crossfeed see the stereo pair they were made for).
            let channels = plan.route.program_channels();
            // Check against the rate the device actually settled on — it can differ from the
            // one planned — so the resampler is never built for a ratio it refuses.
            let out_rate = device_rate.round() as u32;
            if out_rate != track.sample_rate
                && crate::dsp::resample::phases(track.sample_rate, out_rate) > crate::dsp::resample::MAX_PHASES
            {
                bail!(crate::UserError::Resample { from: track.sample_rate, to: out_rate });
            }
            // The words are made for one wire rate: the backend has checked the device runs
            // at it, this only guards against a caller that didn't.
            if let Some(out) = plan.encode
                && out_rate != out.rate.wire_rate()
            {
                bail!("DSD{} needs the device at {} Hz, it runs at {out_rate} Hz", out.rate.multiple(), out.rate.wire_rate());
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
                dop_words: plan.encode.is_some(),
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
                load: AtomicU32::new(0),
                eq: eq.map(|e| EqStage::new(e, fs, channels, opts.dsp.eq_bypass)),
                fir: fir.as_ref().map(|(c, _)| c.clone()),
            });
            let chain = Chain::new(&dsp, plan.lossy.as_ref(), fir, track.sample_rate, out_rate, channels);
            // (The modulator's design is computed here, once per rate and order, before the
            // render thread starts: see `Encoder::new`.)
            let encode = plan.encode.map(|out| {
                let fade_len = if opts.fades { ((device_rate * FADE_SECS) as u32).max(1) } else { 0 };
                Encode {
                    encoder: Encoder::new(out, channels),
                    pcm: Vec::new(),
                    words: Vec::new(),
                    fade_len,
                    // As on the PCM path: a track from its top starts at full level, one picked
                    // up in the middle fades in.
                    fade: if plan.start_frame == 0 { fade_len } else { 0 },
                }
            });
            let handle = {
                let (slots, rs) = (slots.clone(), rs.clone());
                let start = plan.start_frame;
                let route = plan.route.clone();
                let leveled = opts.dsp.levels();
                std::thread::Builder::new()
                    .name("hifi-render".into())
                    .spawn(move || render_loop(slots, rs, chain, encode, start, route, leveled))?
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
        let fade_max = if plan.dop { 0 } else { ((device_rate * FADE_SECS) as u32).max(1) };
        let shared = Box::new(Shared {
            slots,
            pos: AtomicUsize::new(plan.start_frame),
            paused: AtomicBool::new(opts.start_paused),
            finished: AtomicBool::new(false),
            ring,
            meter: [AtomicU32::new(0), AtomicU32::new(0)],
            dop: plan.dop && plan.encode.is_none(),
            route: plan.route.clone(),
            dop_phase: AtomicBool::new(false),
            lossy_enhancers: opts.dsp.lossy.any(),
            fade_len: AtomicU32::new(if opts.fades { fade_max } else { 0 }),
            fade_max,
            // A track from its top starts at full level: its first samples are its own. One
            // picked up in the middle (a seek that reopened the file, new settings) fades in.
            fade: AtomicU32::new(if plan.start_frame == 0 { fade_max } else { 0 }),
            sounding: AtomicBool::new(false),
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
        // next (or swap) → cur → heard: the order tracks move in, so one moving while we read
        // is seen in its old slot or its new one.
        let swap = s.swap.load(Ordering::Acquire);
        let next = s.next.load(Ordering::Acquire);
        let cur = s.cur.load(Ordering::Acquire);
        let heard = s.heard.load(Ordering::Acquire);
        tracks.retain(|t| [swap, next, cur, heard].contains(&ptr(t)));
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
            && prev.layout == next.layout
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

    /// Carry on with `track` in place of the one being heard, from the same position: what
    /// a blind comparison switches with. The old sound fades out and the new one in (see
    /// [`PlayOptions::fades`]), a few milliseconds each — and exactly the same way when
    /// `track` is the one already playing, so that nothing about a switch tells whether the
    /// track changed. False when this playback can't: the two differ in rate or channels (the
    /// device would have to switch), it is DoP (a DSD track, or PCM encoded to DSD: no fade
    /// can follow the modulator), or a following track is already on its way.
    pub fn swap(&self, track: Arc<Track>) -> bool {
        let (mut tracks, [heard, cur, _]) = self.tidy();
        let words = self.shared.ring.as_ref().is_some_and(|rs| rs.dop_words);
        if cur != heard || self.shared.dop || words || track.is_dop() {
            return false;
        }
        // SAFETY: `heard` is owned by `tracks`, which we hold locked.
        let now = unsafe { &*heard };
        if now.sample_rate != track.sample_rate || now.channels != track.channels || now.layout != track.layout {
            return false;
        }
        let p = ptr(&track);
        if !tracks.iter().any(|t| ptr(t) == p) {
            tracks.push(track);
        }
        // (One it replaces, if still waiting, is freed by the next tidy.)
        self.shared.slots.swap.store(p, Ordering::Release);
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

    /// How hard the processing works: the render thread's time in the chain over the duration
    /// of the audio it went through, refreshed about once a second of audio. 0.02 = 2% of one
    /// core; 1.0 or more = it can't keep up (dropouts follow once the buffer runs dry). 0 until
    /// the first second is through; None on the direct, unprocessed path.
    pub fn load(&self) -> Option<f32> {
        self.shared.ring.as_ref().map(|rs| f32::from_bits(rs.load.load(Ordering::Relaxed)))
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    /// Turn the fades on or off for what plays now (see [`PlayOptions::fades`]). A playback
    /// that never fades (DoP) stays that way. PCM encoded to DSD keeps what it was started
    /// with: its only fade, the fade-in after a seek, is made on the render thread ahead of the
    /// encoder, and is not followed by this.
    pub fn set_fades(&self, on: bool) {
        let st = &self.shared;
        st.fade_len.store(if on { st.fade_max } else { 0 }, Ordering::Relaxed);
    }

    /// Before this playback is dropped while it sounds (stop, another track picked): pause it
    /// and wait for the fade to reach the device, so the sound doesn't end in a click. Returns
    /// at once when there is nothing to fade, and within a tenth of a second in any case.
    pub fn fade_out(&self) {
        let st = &self.shared;
        if st.fade_len.load(Ordering::Relaxed) == 0 || self.is_paused() || self.is_finished() || !st.sounding.load(Ordering::Relaxed) {
            return;
        }
        st.paused.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_millis(100);
        while st.sounding.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        // The callback has written the fade; give the device a moment to play it.
        std::thread::sleep(Duration::from_millis(15));
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

/// What `track` (whole in memory) sounds like through `dsp` at its own rate, as interleaved
/// samples: the live path itself — the render thread, and the callback with its equaliser —
/// pulled as fast as it runs instead of by a device's clock. No fades, no device, nothing
/// heard. (For comparing the processing blind: see [`crate::abx`].)
pub(crate) fn render_offline(track: Arc<Track>, dsp: &DspSettings) -> Result<Vec<f32>> {
    let rate = track.sample_rate as f64;
    let ch = track.channels;
    let opts = PlayOptions { exclusive: true, dsp: dsp.clone(), start_at: 0.0, start_paused: false, fades: false };
    let plan = Plan {
        target: rate,
        start_frame: 0,
        lossy: crate::dsp::lossy::plan(&track, &opts.dsp.lossy),
        resampled: false,
        processing: true,
        dop: false,
        // PCM out, whatever `dsp.dsd_out` says: the chain runs as if it were unset.
        encode: None,
        route: Route::new(&track.layout, &track.layout),
    };
    let p = Playback::new(track.clone(), &opts, &plan, rate)?;
    let rs = p.shared.ring.as_ref().expect("a processing playback has a ring");
    const FRAMES: usize = 4096;
    let mut data = vec![0u8; FRAMES * ch * 4];
    let mut out: Vec<f32> = Vec::with_capacity(track.frames() * ch);
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        // A full buffer, or all there will be.
        while rs.ring.available() < FRAMES * ch && !rs.render_done.load(Ordering::Acquire) {
            if Instant::now() > deadline {
                bail!("processing stalled");
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        let done = rs.render_done.load(Ordering::Acquire) && rs.ring.available() < FRAMES * ch;
        let got = p.shared.render(&mut [OutBuf { data: &mut data, channels: ch, format: OutFormat::F32 }]);
        out.extend(data[..got * ch * 4].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
        if done && rs.ring.available() == 0 {
            break;
        }
    }
    Ok(out)
}

/// The render thread: pulls source audio from RAM, runs the processing chain, keeps the ring
/// topped up. Not realtime — it may sleep, and only has to stay ahead of the DAC on average.
///
/// At the end of a track it carries straight on with the one lined up next, through the same
/// chain (no reset: filters ring on across the join as they would within a track), and tells
/// the callback at which output frame the new track begins to be heard.
///
/// `leveled`: each track goes in at its own gain (volume levelling, peak headroom).
///
/// `encode`: PCM out becomes DSD here (see [`Plan::encode`]). After the chain come the EQ (or
/// the clamp to full scale), a fade-in where one is due, and the encoder, and the ring gets
/// the words. The position, `played`, the boundary of a join and the ring all count wire
/// frames, one per device frame, just as they count PCM frames otherwise. The encoder's state
/// runs on across a join like the filters'; a seek starts it, the chain and the EQ over.
fn render_loop(slots: Arc<Slots>, rs: Arc<RingState>, mut chain: Chain, mut encode: Option<Encode>, start_frame: usize, route: Route, leveled: bool) {
    // Our own thread: flush-to-zero for its whole life (the FIR's tails, see fpu.rs).
    let _ftz = crate::fpu::FlushDenormals::on();
    const BLOCK: usize = 2048;
    let mut cur = slots.cur.load(Ordering::Acquire);
    let mut src = start_frame;
    // Where this stretch of the current track started (a seek, or 0 for a following track).
    let mut stretch = start_frame;
    let mut flushed = false;
    let mut out: Vec<f32> = Vec::new();
    // The track folded down to the program, when the route folds it.
    let mut folded: Vec<f32> = Vec::new();
    let mut sent = 0usize;
    let nap = || std::thread::sleep(Duration::from_millis(2));
    let mut load = Load::default();

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
            if let Some(e) = &mut encode {
                e.reset(&rs, src == 0);
            }
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
        let swap = slots.swap.load(Ordering::Acquire);
        if !swap.is_null() {
            // Carry on with another track from where this one is heard now: what the device
            // has played of the current stretch, in source frames.
            let at = rs.base_frame.load(Ordering::Relaxed) as f64 + rs.played.load(Ordering::Relaxed) as f64 * rs.source_rate / rs.device_rate;
            // Store the new slot before clearing the old one (see `Slots`).
            slots.cur.store(swap, Ordering::Release);
            let _ = slots.swap.compare_exchange(swap, std::ptr::null_mut(), Ordering::AcqRel, Ordering::Relaxed);
            cur = swap;
            // SAFETY: just stored in a slot, so the Playback owns it.
            src = (at.round() as usize).min(unsafe { &*cur }.frames());
            stretch = src;
            flushed = false;
            rs.tail_out.store(false, Ordering::Release);
            chain.reset();
            // (`Playback::swap` refuses while encoding; should one get here, it starts over
            // like a seek.)
            if let Some(e) = &mut encode {
                e.reset(&rs, src == 0);
            }
            out.clear();
            sent = 0;
            rs.render_done.store(false, Ordering::Release);
            rs.base_frame.store(src, Ordering::Relaxed);
            let generation = rs.flush_gen.fetch_add(1, Ordering::AcqRel) + 1;
            // As for a seek: the callback fades out what it has, then drops the rest.
            while rs.flush_ack.load(Ordering::Acquire) != generation {
                if rs.quit.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            slots.heard.store(swap, Ordering::Release);
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
            } else if !flushed && chain.finishing() {
                // The tail is on its way out, a block at a time (a long upsampling filter's
                // can be many seconds): nothing joins on any more.
                flushed = tail(&mut chain, encode.as_mut(), &rs, &mut load, &mut out);
                sent = 0;
                continue;
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
                    flushed = tail(&mut chain, encode.as_mut(), &rs, &mut load, &mut out);
                    sent = 0;
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
        let gain = if leveled { track.gain() as f64 } else { 1.0 };
        let t = Instant::now();
        let processed = if route.folds() {
            route.fold_block(input, &mut folded);
            chain.process(&folded, gain, false)
        } else {
            chain.process(input, gain, false)
        };
        let (frames_in, frames_out) = (input.len() / track.channels, processed.len() / rs.channels);
        match &mut encode {
            Some(e) => {
                e.run(&rs, processed, &mut out);
                // (The EQ and the encoder are work of this thread too.)
                load.add(&rs, t.elapsed(), frames_in, frames_out);
            }
            None => {
                load.add(&rs, t.elapsed(), frames_in, frames_out);
                out.clear();
                // Not clamped yet: the callback's EQ may still lower the level (its preamp).
                out.extend(processed.iter().map(|&v| v as f32));
            }
        }
        sent = 0;
    }
}

/// One step of letting the chain's tail out at the end, into `out`. True once it is all out
/// (and, when encoding, the encoder's own after it).
fn tail(chain: &mut Chain, encode: Option<&mut Encode>, rs: &RingState, load: &mut Load, out: &mut Vec<f32>) -> bool {
    let t = Instant::now();
    let processed = chain.process(&[], 1.0, true);
    match encode {
        Some(e) => {
            e.run(rs, processed, out);
            if !chain.finishing() {
                e.finish(rs, out);
            }
            load.add(rs, t.elapsed(), 0, out.len() / rs.channels);
        }
        None => {
            load.add(rs, t.elapsed(), 0, processed.len() / rs.channels);
            out.clear();
            out.extend(processed.iter().map(|&v| v as f32));
        }
    }
    !chain.finishing()
}

/// The render thread's end of PCM → DSD (see [`Plan::encode`]): what the callback does to PCM
/// on the processed path — the EQ, the clamp, a fade-in — done here instead, ahead of the
/// modulator, since no level can change after it. Pause, stop and the cut at a seek are
/// abrupt, as for a DSD track (the DAC mutes on idle DoP by itself).
struct Encode {
    encoder: Encoder,
    /// One block of PCM at the wire rate on its way to the encoder.
    pcm: Vec<f32>,
    words: Vec<u16>,
    /// Length of the fade-in in wire frames; 0 = no fades (they were off when the playback
    /// was created: `Playback::set_fades` isn't followed here).
    fade_len: u32,
    /// Where the fade-in stands: 0 (silent) ..= `fade_len` (full level).
    fade: u32,
}

impl Encode {
    /// A seek (or a swap): start over, fading in unless the new place is the track's top.
    fn reset(&mut self, rs: &RingState, at_top: bool) {
        self.encoder.reset();
        if let Some(eq) = &rs.eq {
            // SAFETY: while encoding, the render thread is the only one that runs the EQ: the
            // callback never touches it in this mode (see `Shared::render_words`).
            unsafe { eq.live() }.reset();
        }
        self.fade = if at_top { self.fade_len } else { 0 };
    }

    /// One block from the chain (program channels at the wire rate) into words, which go
    /// into `out` (replacing what it held) as their numeric values (see `RingState::ring`).
    fn run(&mut self, rs: &RingState, block: &[f64], out: &mut Vec<f32>) {
        let ch = rs.channels;
        self.pcm.clear();
        self.pcm.extend(block.iter().map(|&v| v as f32));
        // The EQ, or just the clamp to full scale, as the callback does them on PCM.
        let peak = match &rs.eq {
            // SAFETY: as in `reset`: this thread is the EQ's only one in this mode.
            Some(eq) => unsafe { eq.live() }.process(&eq.mailbox, &mut self.pcm),
            None => self.pcm.iter_mut().fold(0f32, |m, s| {
                let p = m.max(s.abs());
                *s = s.clamp(-1.0, 1.0);
                p
            }),
        };
        rs.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
        if self.fade < self.fade_len {
            for frame in self.pcm.chunks_exact_mut(ch) {
                if self.fade >= self.fade_len {
                    break;
                }
                let g = fade_gain(self.fade + 1, self.fade_len);
                frame.iter_mut().for_each(|s| *s *= g);
                self.fade += 1;
            }
        }
        self.words.clear();
        self.encoder.process(&self.pcm, &mut self.words);
        out.clear();
        out.extend(self.words.iter().map(|&w| w as f32));
    }

    /// The end of the stream: silence through the encoder for as long as its delay and 2 ms
    /// more, appended to `out`, so the last of the music is out and the stream ends in coded
    /// silence rather than in the middle of a waveform.
    fn finish(&mut self, rs: &RingState, out: &mut Vec<f32>) {
        let frames = self.encoder.latency_frames() + (rs.device_rate * 0.002).ceil() as usize;
        self.pcm.clear();
        self.pcm.resize(frames * rs.channels, 0.0);
        self.words.clear();
        self.encoder.process(&self.pcm, &mut self.words);
        out.extend(self.words.iter().map(|&w| w as f32));
    }
}

/// The render thread's tally for [`Playback::load`]: time in the chain, and the audio that
/// went in and came out, since it was last published.
#[derive(Default)]
struct Load {
    busy: Duration,
    frames_in: usize,
    frames_out: usize,
}

impl Load {
    /// Count one `chain.process` call (with the EQ and the encoder after it, when encoding):
    /// `took`, `frames_in` source frames, `frames_out` device frames. Published once a second
    /// of audio has come out. The work is set against the longer of what went in and what came
    /// out: a long filter takes seconds of input before its first output (and after each
    /// seek), and lets its tail out with no input at the end.
    fn add(&mut self, rs: &RingState, took: Duration, frames_in: usize, frames_out: usize) {
        self.busy += took;
        self.frames_in += frames_in;
        self.frames_out += frames_out;
        let out_secs = self.frames_out as f64 / rs.device_rate;
        if out_secs >= 1.0 {
            let secs = out_secs.max(self.frames_in as f64 / rs.source_rate);
            rs.load.store(((self.busy.as_secs_f64() / secs) as f32).to_bits(), Ordering::Relaxed);
            *self = Self::default();
        }
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
                let fade_len = st.fade_len.load(Ordering::Relaxed);
                let mut level = st.fade.load(Ordering::Relaxed);
                let was_sounding = st.sounding.load(Ordering::Relaxed);
                let mut sounding = was_sounding;
                let mut cur = slots.cur.load(Ordering::Acquire);
                let seek = slots.seek_req.load(Ordering::Acquire);
                let mut done = 0;
                let mut peaks = [0f32; 2];
                // What is about to be cut off (a pause, a jump elsewhere) fades out first.
                // (Not for a seek meant for a track that no longer plays: nothing is cut.)
                let jumping = seek != NO_SEEK && slots.seek_track.load(Ordering::Acquire) == cur;
                // Another track is to carry on from here (see `Playback::swap`): this one
                // fades out like before a jump.
                let swap = slots.swap.load(Ordering::Acquire);
                if fade_len > 0 && sounding && level > 0 && (paused || jumping || !swap.is_null()) {
                    // SAFETY: `cur` is in a slot, so the Playback owns it.
                    let track = unsafe { &*cur };
                    let pos = track.claim(st.pos.load(Ordering::Relaxed));
                    let want = (level as usize).min(frames);
                    let n = track.ready_frames().saturating_sub(pos).min(want);
                    let (route, from) = (&st.route, level);
                    peaks = fill(buffers, 0..n, n, |f, dc| track_sample(track, route, pos + f, dc) * fade_gain(from - 1 - f as u32, fade_len));
                    st.pos.store(pos + n, Ordering::Relaxed);
                    done = n;
                    // (Fewer than wanted: the track ended or is still decoding — silence anyway.)
                    level = if n < want { 0 } else { level - n as u32 };
                    if level > 0 {
                        // The buffer is shorter than the fade: the rest next time.
                        st.fade.store(level, Ordering::Relaxed);
                        st.meter(peaks);
                        return done;
                    }
                    sounding = false;
                }
                if seek != NO_SEEK
                    && slots.seek_req.compare_exchange(seek, NO_SEEK, Ordering::AcqRel, Ordering::Relaxed).is_ok()
                    && slots.seek_track.load(Ordering::Acquire) == cur
                {
                    st.pos.store(seek, Ordering::Relaxed);
                    // Into the middle of the track: fade in. To its top: its first samples
                    // as they are.
                    if fade_len > 0 {
                        level = if seek == 0 { fade_len } else { 0 };
                    }
                    sounding = false;
                }
                if !swap.is_null() {
                    // Store the new slots before clearing the old one (see `Slots`). The
                    // position stays: the new track picks up where the old one stopped.
                    slots.cur.store(swap, Ordering::Release);
                    slots.heard.store(swap, Ordering::Release);
                    let _ = slots.swap.compare_exchange(swap, std::ptr::null_mut(), Ordering::AcqRel, Ordering::Relaxed);
                    cur = swap;
                    st.finished.store(false, Ordering::Relaxed);
                    if fade_len > 0 && was_sounding {
                        level = 0;
                    }
                    sounding = false;
                }
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
                    let route = &st.route;
                    let p = if st.dop {
                        fill_dop(buffers, done..to, avail, &st.dop_phase, |f, dc| dop_sample(track, route, pos + f, dc))
                    } else if level < fade_len && avail > 0 {
                        // Picking up after a pause or a jump: fade in.
                        let from = level;
                        level = level.saturating_add(avail as u32).min(fade_len);
                        fill(buffers, done..to, avail, |f, dc| track_sample(track, route, pos + f, dc) * fade_gain(from + 1 + f as u32, fade_len))
                    } else {
                        fill(buffers, done..to, avail, |f, dc| track_sample(track, route, pos + f, dc))
                    };
                    peaks = [peaks[0].max(p[0]), peaks[1].max(p[1])];
                    if avail > 0 {
                        sounding = true;
                    }
                    // Ran out mid-track while playing (a stream still loading): the sound was
                    // cut off, so it fades in when it is back.
                    if !paused && !ended && done + avail < frames {
                        if sounding {
                            level = 0;
                        }
                        sounding = false;
                    }
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
                if paused {
                    sounding = false;
                }
                st.fade.store(settled(level, fade_len, st.fade_max, was_sounding, sounding), Ordering::Relaxed);
                st.sounding.store(sounding, Ordering::Relaxed);
                st.meter(peaks);
                done
            }
            // PCM encoded to DSD: words from the ring, packed as DoP.
            Some(rs) if rs.dop_words => self.render_words(rs, buffers, frames, paused),
            // Processed path: pre-rendered samples from the ring.
            Some(rs) => {
                // SAFETY: only this (IO) thread ever touches `scratch`.
                let scratch = unsafe { &mut *rs.scratch.get() };
                let fade_len = st.fade_len.load(Ordering::Relaxed);
                let mut level = st.fade.load(Ordering::Relaxed);
                let was_sounding = st.sounding.load(Ordering::Relaxed);
                let mut sounding = was_sounding;
                let ch = rs.channels;
                let piece = scratch.len() / ch;
                let route = &st.route;
                let mut got = 0;
                // Frames written that don't count as played: the old sound fading out in
                // front of a seek.
                let mut before_seek = 0;
                let mut peaks = [0f32; 2];
                // The EQ, or just the clamp to full scale; returns the block's peak.
                let finish = |block: &mut [f32]| match &rs.eq {
                    // SAFETY: this is the callback thread.
                    Some(eq) => unsafe { eq.live() }.process(&eq.mailbox, block),
                    None => block.iter_mut().fold(0f32, |m, s| {
                        let p = m.max(s.abs());
                        *s = s.clamp(-1.0, 1.0);
                        p
                    }),
                };
                let generation = rs.flush_gen.load(Ordering::Acquire);
                let flush = generation != rs.flush_ack.load(Ordering::Relaxed);
                // What is about to be cut off (a pause, a seek throwing the queue away) fades
                // out first.
                if fade_len > 0 && sounding && level > 0 && (paused || flush) {
                    let want = (level as usize).min(frames).min(piece);
                    let n = rs.ring.pop(&mut scratch[..want * ch]) / ch;
                    let block = &mut scratch[..n * ch];
                    rs.peak.fetch_max(finish(block).to_bits(), Ordering::Relaxed);
                    for (f, frame) in block.chunks_exact_mut(ch).enumerate() {
                        let g = fade_gain(level - 1 - f as u32, fade_len);
                        frame.iter_mut().for_each(|s| *s *= g);
                    }
                    peaks = fill(buffers, 0..n, n, |f, dc| source_sample(scratch, ch, route, f, dc));
                    got = n;
                    if flush {
                        before_seek = n;
                    }
                    // (Fewer than wanted: the ring ran dry — silence anyway.)
                    level = if n < want { 0 } else { level - n as u32 };
                    if level == 0 {
                        sounding = false;
                    }
                }
                // Still fading out (a buffer shorter than the fade): the rest next time, and a
                // seek waits for it.
                let fading_out = fade_len > 0 && sounding && level > 0 && (paused || flush);
                if flush && !fading_out {
                    if let Some(eq) = &rs.eq {
                        // SAFETY: this is the callback thread.
                        unsafe { eq.live() }.reset();
                    }
                    rs.ring.discard();
                    rs.played.store(0, Ordering::Relaxed);
                    rs.flush_ack.store(generation, Ordering::Release);
                    rs.refilling.store(true, Ordering::Relaxed);
                    // Into the middle of the track: fade in. To its top: its first samples
                    // as they are.
                    if fade_len > 0 {
                        level = if rs.base_frame.load(Ordering::Relaxed) == 0 { fade_len } else { 0 };
                    }
                    sounding = false;
                }
                let hold = paused || fading_out;
                // In pieces of the scratch size; once the ring runs short, the rest is silence.
                while got < frames {
                    let want = (frames - got).min(piece);
                    let n = if hold { 0 } else { rs.ring.pop(&mut scratch[..want * ch]) / ch };
                    let block = &mut scratch[..n * ch];
                    rs.peak.fetch_max(finish(block).to_bits(), Ordering::Relaxed);
                    if level < fade_len && n > 0 {
                        // Picking up after a pause or a seek: fade in.
                        for (f, frame) in block.chunks_exact_mut(ch).enumerate() {
                            let g = fade_gain(level + 1 + f as u32, fade_len);
                            frame.iter_mut().for_each(|s| *s *= g);
                        }
                        level = level.saturating_add(n as u32).min(fade_len);
                    }
                    if n > 0 {
                        sounding = true;
                    }
                    let to = if n < want { frames } else { got + want };
                    let p = fill(buffers, got..to, n, |f, dc| source_sample(scratch, ch, route, f, dc));
                    peaks = [peaks[0].max(p[0]), peaks[1].max(p[1])];
                    got += n;
                    if n < want {
                        // Ran dry mid-stream (a dropout, a stream still loading): the sound was
                        // cut off, so it fades in when it is back. (Before any sound — the
                        // ring first filling — the level stays: a track from its top starts
                        // at full level.)
                        if !hold {
                            if sounding {
                                level = 0;
                            }
                            sounding = false;
                        }
                        break;
                    }
                }
                if paused && !fading_out {
                    sounding = false;
                }
                st.fade.store(settled(level, fade_len, st.fade_max, was_sounding, sounding), Ordering::Relaxed);
                st.sounding.store(sounding, Ordering::Relaxed);
                st.meter(peaks);
                self.ring_played(rs, got, (got - before_seek) as u64, frames, paused, flush);
                got
            }
        }
    }

    /// The processed path's bookkeeping at the end of a callback that wrote `got` of `frames`
    /// frames from the ring, `counted` of them played music: the position, the switch to the
    /// next track once its first frame is out, dropouts, and the end.
    #[inline(always)]
    fn ring_played(&self, rs: &RingState, got: usize, counted: u64, frames: usize, paused: bool, flush: bool) {
        let slots = &*self.slots;
        let played = rs.played.fetch_add(counted, Ordering::Relaxed) + counted;
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
            // (Not while the old sound still fades out in front of a seek.)
            if !flush {
                rs.refilling.store(false, Ordering::Relaxed);
            }
        } else if !paused
            && !rs.refilling.load(Ordering::Relaxed)
            && !rs.starved.load(Ordering::Relaxed)
            && !rs.render_done.load(Ordering::Acquire)
        {
            rs.underruns.fetch_add(1, Ordering::Relaxed);
        }
        if !paused && got < frames && rs.render_done.load(Ordering::Acquire) && rs.ring.available() == 0 {
            self.finished.store(true, Ordering::Relaxed);
        }
    }

    /// The callback when the ring carries DSD words (PCM the render thread encoded): every
    /// device frame goes out as DoP under the next marker, the words while there are any, the
    /// idle pattern otherwise (paused, the ring dry, after the end). Nothing here changes a
    /// level — no EQ (the render thread ran it, and the callback must never touch it in this
    /// mode), no clamp, no fades: the words are bits, not samples. A seek drops what is queued
    /// at once.
    fn render_words(&self, rs: &RingState, buffers: &mut [OutBuf<'_>], frames: usize, paused: bool) -> usize {
        // SAFETY: only this (IO) thread ever touches `scratch`.
        let scratch = unsafe { &mut *rs.scratch.get() };
        let ch = rs.channels;
        let piece = scratch.len() / ch;
        let route = &self.route;
        let generation = rs.flush_gen.load(Ordering::Acquire);
        let flush = generation != rs.flush_ack.load(Ordering::Relaxed);
        if flush {
            rs.ring.discard();
            rs.played.store(0, Ordering::Relaxed);
            rs.flush_ack.store(generation, Ordering::Release);
            rs.refilling.store(true, Ordering::Relaxed);
        }
        let mut got = 0;
        let mut peaks = [0f32; 2];
        // In pieces of the scratch size; once the ring runs short, the rest is idle DoP. Each
        // piece follows on from the last, so the markers run on unbroken (see `fill_dop`).
        while got < frames {
            let want = (frames - got).min(piece);
            let n = if paused { 0 } else { rs.ring.pop(&mut scratch[..want * ch]) / ch };
            let block = &scratch[..n * ch];
            let to = if n < want { frames } else { got + want };
            // (A word is exact in f32; `as u16` takes it back.)
            let p = fill_dop(buffers, got..to, n, &self.dop_phase, |f, dc| match route.program_channel(dc) {
                Some(pc) => block[f * ch + pc] as u16,
                None => DOP_IDLE,
            });
            peaks = [peaks[0].max(p[0]), peaks[1].max(p[1])];
            got += n;
            if n < want {
                break;
            }
        }
        // `fill_dop` reads the density as a DSD file's level (70 % modulation = PCM full scale);
        // here PCM full scale went into the encoder as `LEVEL` (50 %), so the meters show the
        // PCM level the music had before it was encoded.
        const TO_PCM: f32 = (1.0 / (crate::dsp::dsd_encode::LEVEL * PCM_GAIN)) as f32;
        self.meter(peaks.map(|p| p * TO_PCM));
        self.ring_played(rs, got, got as u64, frames, paused, flush);
        got
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
fn dop_sample(track: &Track, route: &Route, frame: usize, device_channel: usize) -> u16 {
    // DoP only ever goes channel for channel (see `plan`).
    match route.program_channel(device_channel) {
        Some(c) => track.dop_payload(frame, c),
        None => DOP_IDLE,
    }
}

#[inline(always)]
fn track_sample(track: &Track, route: &Route, frame: usize, device_channel: usize) -> f32 {
    match route.program_channel(device_channel) {
        Some(pc) => route.program_sample(pc, |c| track.sample(frame, c)),
        None => 0.0,
    }
}

/// From the ring, which holds program channels already folded down.
#[inline(always)]
fn source_sample(samples: &[f32], channels: usize, route: &Route, frame: usize, device_channel: usize) -> f32 {
    match route.program_channel(device_channel) {
        Some(pc) => samples[frame * channels + pc],
        None => 0.0,
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
            let plan = Plan { target: FS as f64, start_frame: 0, lossy: None, resampled: false, processing: true, dop: false, encode: None, route: stereo_route() };
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
            let plan = Plan { target: first.sample_rate as f64, start_frame: 0, lossy: None, resampled: false, processing, dop: first.is_dop(), encode: None, route: stereo_route() };
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
            let plan = Plan { target: fs as f64, start_frame: 0, lossy: None, resampled: false, processing: true, dop: false, encode: None, route: stereo_route() };
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
            // 10 ms of the old place fading out, 10 ms of the new one fading in, then every
            // sample as it is in the file.
            let out = callback(&p, 2_000);
            let want = |i: usize| ((39 * 48_000 + i - 480) % 30_000) as f32 / 32_768.0;
            assert_eq!(out[960], want(960));
            assert_eq!(out[1_999], want(1_999));
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

    /// Upsampling through the long filter (44.1 kHz tracks on a device at 88.2 kHz): a stage
    /// that holds back up to `startup_frames()` of input and lets its output out in bursts,
    /// through the join, the seek and the ring the rest of the path was built around.
    mod long_filter {
        use super::*;
        use crate::decode::TrackMeta;
        use crate::dsp::upsample::FilterKind;

        const SRC: u32 = 44_100;
        const DEV: u32 = 88_200;
        /// 10 ms at 88.2 kHz.
        const FADE: usize = 882;

        fn spec(taps: usize) -> FilterSpec {
            FilterSpec { kind: FilterKind::Linear, taps }
        }

        /// Frames `from..from + frames` of one stereo sine (997 Hz and 19 kHz).
        fn sine(from: usize, frames: usize) -> Arc<Track> {
            let samples: Vec<f32> = (from..from + frames)
                .flat_map(|i| {
                    let t = i as f64 / SRC as f64;
                    let v = 0.3 * (2.0 * std::f64::consts::PI * 997.0 * t).sin() + 0.2 * (2.0 * std::f64::consts::PI * 19_000.0 * t).sin();
                    [v as f32, -v as f32]
                })
                .collect();
            Arc::new(Track::from_samples(SRC, 2, Some(24), &samples, TrackMeta::default()))
        }

        fn opts(filter: Option<FilterSpec>, fades: bool) -> PlayOptions {
            let dsp = DspSettings { upsample: Upsample::UpTo(DEV as f64), upsample_filter: filter, ..Default::default() };
            PlayOptions { dsp, fades, ..Default::default() }
        }

        fn plan(start_frame: usize) -> Plan {
            Plan { target: DEV as f64, start_frame, lossy: None, resampled: true, processing: true, dop: false, encode: None, route: stereo_route() }
        }

        fn start(track: Arc<Track>, filter: Option<FilterSpec>, start_frame: usize, fades: bool) -> Playback {
            Playback::new(track, &opts(filter, fades), &plan(start_frame), DEV as f64).unwrap()
        }

        /// One callback of 512 stereo F32 frames, once the ring can serve it (or has nothing
        /// more coming); returns the left channel.
        fn callback(p: &Playback, wait: bool) -> Vec<f32> {
            let rs = p.shared().ring.as_ref().unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            while wait && rs.ring.available() < 512 * 2 && !rs.render_done.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "the render thread stalled");
                std::thread::sleep(Duration::from_millis(1));
            }
            let mut data = vec![0u8; 512 * 8];
            p.shared().render(&mut [OutBuf { data: &mut data, channels: 2, format: OutFormat::F32 }]);
            data.chunks_exact(8).map(|f| f32::from_le_bytes(f[..4].try_into().unwrap())).collect()
        }

        fn worst(a: &[f32], b: &[f32]) -> f32 {
            a.iter().zip(b).fold(0f32, |m, (x, y)| m.max((x - y).abs()))
        }

        /// Two tracks cut from one sine play exactly as the sine as one track, and the heard
        /// track changes on the very frame the second begins.
        fn join_through(taps: usize, a_frames: usize, b_frames: usize) {
            let whole = start(sine(0, a_frames + b_frames), Some(spec(taps)), 0, true);
            let total = 2 * (a_frames + b_frames);
            let mut want = Vec::new();
            while want.len() < total {
                want.extend(callback(&whole, true));
            }
            drop(whole);

            let (a, b) = (sine(0, a_frames), sine(a_frames, b_frames));
            let p = start(a.clone(), Some(spec(taps)), 0, true);
            assert!(p.queue_next(b.clone()), "lined up before the render thread reaches A's end");
            let mut out = Vec::new();
            while out.len() < total {
                out.extend(callback(&p, true));
                let heard_b = Arc::ptr_eq(&p.heard(), &b);
                assert_eq!(heard_b, out.len() >= 2 * a_frames, "at {}", out.len());
                if heard_b {
                    let at = (out.len() - 2 * a_frames) as f64 / DEV as f64;
                    assert!((p.position_secs() - at.min(p.duration_secs())).abs() < 1e-6);
                }
            }
            let err = worst(&out[..total], &want[..total]);
            let load = p.load().unwrap();
            println!("join through Linear {taps}: largest difference from one track {err:e}, load {:.4}, underruns {}", load, p.underruns());
            assert!(err <= 1e-6, "differs from one track by {err}");
            assert_eq!(p.switches(), 1);
            assert_eq!(p.underruns(), 0);
            assert!(load > 0.0 && load < 1.0, "load {load}");
            while !p.is_finished() {
                callback(&p, true);
            }
        }

        #[test]
        fn a_join_through_the_long_filter_is_seamless() {
            join_through(65_536, 3 * SRC as usize + 777, 2 * SRC as usize);
        }

        /// The same with 2^20 taps (6.3 s of input held back before the first output):
        /// `cargo test --release -p hifi-engine -- --ignored join_through_the_longest`
        #[test]
        #[ignore]
        fn a_join_through_the_longest_filter_is_seamless() {
            join_through(1 << 20, 10 * SRC as usize + 777, 3 * SRC as usize);
        }

        /// After a seek the chain starts over at the new frame: what comes out (once it has
        /// faded in) is what a playback started there plays.
        #[test]
        fn a_seek_lands_where_a_fresh_start_would() {
            let track = sine(0, 5 * SRC as usize);
            let p = start(track.clone(), Some(spec(65_536)), 0, true);
            for _ in 0..200 {
                callback(&p, true);
            }
            let rs = p.shared().ring.as_ref().unwrap();
            let generation = rs.flush_gen.load(Ordering::Acquire);
            assert!(p.seek(3.0));
            // The render thread takes the seek and waits for the callback to drop the ring,
            // which it does once the old sound has faded out.
            let deadline = Instant::now() + Duration::from_secs(5);
            while rs.flush_gen.load(Ordering::Acquire) == generation {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
            while rs.flush_ack.load(Ordering::Acquire) != rs.flush_gen.load(Ordering::Acquire) {
                callback(&p, false);
            }
            let mut after = Vec::new();
            while after.len() < DEV as usize {
                after.extend(callback(&p, true));
            }
            let fresh = start(track, Some(spec(65_536)), 3 * SRC as usize, true);
            let mut want = Vec::new();
            while want.len() < DEV as usize {
                want.extend(callback(&fresh, true));
            }
            let (fading, settled) = (worst(&after[..FADE], &want[..FADE]), worst(&after[FADE..], &want[FADE..]));
            println!("seek through Linear 65536: vs a fresh start there, largest difference {fading:e} in the fade-in, {settled:e} after");
            assert!(settled <= 1e-6, "lands elsewhere: {settled}");
            assert!((p.position_secs() - (3.0 + DEV as f64 / DEV as f64)).abs() < 0.02, "at {}", p.position_secs());
            assert_eq!(p.underruns(), 0);
        }

        #[test]
        fn load_is_measured_on_the_processed_path_only() {
            for (filter, name) in [(Some(spec(65_536)), "Linear 65536"), (Some(spec(1 << 20)), "Linear 2^20"), (None, "standard")] {
                let p = start(sine(0, 4 * SRC as usize), filter, 0, false);
                while !p.is_finished() {
                    callback(&p, true);
                }
                let load = p.load().unwrap();
                println!("44.1 → 88.2 kHz stereo, {name}: load {load:.4} ({:.0}× real time)", 1.0 / load);
                assert!(load > 0.0 && load < 1.0, "{name}: {load}");
            }
            let plan = Plan { target: SRC as f64, start_frame: 0, lossy: None, resampled: false, processing: false, dop: false, encode: None, route: stereo_route() };
            let direct = Playback::new(sine(0, 1_000), &PlayOptions::default(), &plan, SRC as f64).unwrap();
            assert_eq!(direct.load(), None);
        }

        #[test]
        fn the_signal_path_names_the_filter_only_when_it_runs() {
            let track = sine(0, 1_000);
            let path = |filter: Option<FilterSpec>, upsample: Upsample, device: f64, processing: bool| {
                let dsp = DspSettings { upsample, upsample_filter: filter, ..Default::default() };
                let opts = PlayOptions { dsp, ..Default::default() };
                let plan = Plan { target: device, processing, resampled: device != SRC as f64, ..plan(0) };
                signal_path(&track, &opts, &plan, device, None, OutFormat::F32, true, Transport::Usb).upsample_filter
            };
            let up = Upsample::UpTo(384_000.0);
            assert_eq!(path(Some(spec(65_536)), up, 352_800.0, true), Some(spec(65_536)));
            // As built: rounded up to a power of two.
            assert_eq!(path(Some(spec(50_000)), up, 88_200.0, true), Some(spec(65_536)));
            // The device settled below twice the source rate, or at it.
            assert_eq!(path(Some(spec(65_536)), up, 48_000.0, true), None);
            assert_eq!(path(Some(spec(65_536)), up, 44_100.0, false), None);
            // Upsampling off (resampling only because the DAC needs it), or no filter chosen.
            assert_eq!(path(Some(spec(65_536)), Upsample::Off, 88_200.0, true), None);
            assert_eq!(path(None, up, 88_200.0, true), None);
            // A track joining on keeps it.
            let dsp = DspSettings { upsample: up, upsample_filter: Some(spec(65_536)), ..Default::default() };
            let full = signal_path(&track, &PlayOptions { dsp, ..Default::default() }, &plan(0), 88_200.0, None, OutFormat::F32, true, Transport::Usb);
            assert_eq!(full.for_next(&sine(0, 500)).upsample_filter, Some(spec(65_536)));
        }
    }

    /// PCM encoded to DSD on the render thread and sent as DoP, pulled as a device would pull
    /// it: the markers, what the words decode to, and what runs ahead of the encoder (the EQ,
    /// the level, the fade-in), through pauses, seeks, dropouts and joins.
    mod dsd_out {
        use super::*;
        use crate::decode::TrackMeta;
        use crate::dsd::{Converter, kaiser_beta, kaiser_lowpass};
        use crate::dsp::dsd_encode::{DsdRate, LEVEL, ModOrder};
        use crate::dsp::eq::{Band, FilterKind};
        use crate::dsp::loudness::{Basis, Level, LevelMode, LevelSettings};
        use std::f64::consts::PI;

        const DSD64_5: DsdOut = DsdOut { rate: DsdRate::Dsd64, order: ModOrder::Fifth };
        const DSD64_7: DsdOut = DsdOut { rate: DsdRate::Dsd64, order: ModOrder::Seventh };
        /// DSD64's wire rate, in frames per second.
        const WIRE: usize = 176_400;
        /// What the converter (`crate::dsd::Converter`) puts out for every DSD rate of the
        /// 44.1 kHz family.
        const DECODED: f64 = 176_400.0;

        /// Stereo at `fs`: `left` Hz on the left, `right` Hz on the right (0 Hz: a steady
        /// level), both at `amp`; frames `from..from + frames` of the signal.
        fn tones(fs: u32, left: f64, right: f64, amp: f64, from: usize, frames: usize) -> Arc<Track> {
            let wave = |f: f64, i: usize| if f == 0.0 { amp } else { amp * (2.0 * PI * f * i as f64 / fs as f64).sin() };
            let samples: Vec<f32> = (from..from + frames).flat_map(|i| [wave(left, i) as f32, wave(right, i) as f32]).collect();
            Arc::new(Track::from_samples(fs, 2, Some(24), &samples, TrackMeta::default()))
        }

        fn opts(out: DsdOut, fades: bool) -> PlayOptions {
            PlayOptions { dsp: DspSettings { dsd_out: Some(out), ..Default::default() }, fades, ..Default::default() }
        }

        /// A DAC with the usual PCM rates, the DoP wire rates among them.
        fn dac_rates() -> Vec<RateRange> {
            [44_100.0, 48_000.0, 88_200.0, 96_000.0, 176_400.0, 192_000.0, 352_800.0, 384_000.0, 705_600.0, 768_000.0].map(RateRange::exact).to_vec()
        }

        /// Planned as a backend plans it, on a DAC that takes DoP at the wire rate, and started
        /// at that rate.
        fn start(track: Arc<Track>, opts: &PlayOptions) -> Playback {
            let out = opts.dsp.dsd_out.unwrap();
            let plan = plan(&track, opts, &dac_rates(), "DAC", true, &crate::channels::default_layout(2)).unwrap();
            assert_eq!(plan.encode, Some(out));
            Playback::new(track, opts, &plan, out.rate.wire_rate() as f64).unwrap()
        }

        /// The 24-bit DoP word a DAC reads back from one sample of `format`.
        fn word(bytes: &[u8], format: OutFormat) -> u32 {
            match format {
                OutFormat::I24Packed => u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]),
                OutFormat::I32 { bits: 32 } => u32::from_le_bytes(bytes.try_into().unwrap()) >> 8,
                OutFormat::I32 { .. } => u32::from_le_bytes(bytes.try_into().unwrap()) & 0xFF_FFFF,
                OutFormat::F32 => ((f32::from_le_bytes(bytes.try_into().unwrap()) as f64 * 8_388_608.0).round() as i32 as u32) & 0xFF_FFFF,
                OutFormat::I16 => unreachable!("DoP never goes out in 16 bits"),
            }
        }

        fn marker(w: u32) -> u8 {
            (w >> 16) as u8
        }

        fn payload(w: u32) -> u16 {
            w as u16
        }

        /// One callback of `frames` stereo frames in `format`: each frame's DoP word per channel.
        fn callback(p: &Playback, frames: usize, format: OutFormat) -> Vec<[u32; 2]> {
            let b = format.bytes();
            let mut data = vec![0u8; frames * 2 * b];
            p.shared().render(&mut [OutBuf { data: &mut data, channels: 2, format }]);
            data.chunks_exact(2 * b).map(|f| [0, 1].map(|c| word(&f[c * b..(c + 1) * b], format))).collect()
        }

        /// Until the ring can serve a callback of 512 frames, or has nothing more coming.
        fn wait(p: &Playback) {
            let rs = p.shared().ring.as_ref().unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            while rs.ring.available() < 512 * 2 && !rs.render_done.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "the render thread stalled");
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        /// Callbacks of 512 frames, each once the ring can serve it, as a device's clock asks.
        fn pull(p: &Playback, frames: usize, format: OutFormat) -> Vec<[u32; 2]> {
            let mut out = Vec::new();
            while out.len() < frames {
                wait(p);
                out.extend(callback(p, 512, format));
            }
            out
        }

        fn pull_all(p: &Playback, format: OutFormat) -> Vec<[u32; 2]> {
            let mut out = Vec::new();
            while !p.is_finished() {
                wait(p);
                out.extend(callback(p, 512, format));
            }
            out
        }

        /// The payloads through the DSD → PCM converter (what a DSD file plays as), per channel
        /// at [`DECODED`].
        fn decode(frames: &[[u32; 2]], rate: DsdRate) -> [Vec<f64>; 2] {
            let runs: Vec<Vec<u8>> = (0..2).map(|c| frames.iter().flat_map(|f| payload(f[c]).to_be_bytes()).collect()).collect();
            let mut conv = Converter::new(rate.hz(), 2).unwrap();
            let mut pcm = Vec::new();
            conv.process(&runs, &mut pcm);
            [0, 1].map(|c| pcm.iter().skip(c).step_by(2).map(|&v| v as f64).collect())
        }

        /// `len` samples of `x` low-passed at 20 kHz (DSD's shaped noise is above), the first
        /// centred on sample `from + 255`.
        fn audio_band(x: &[f64], from: usize, len: usize) -> Vec<f64> {
            let lp = kaiser_lowpass(511, 20_000.0 / DECODED, kaiser_beta(120.0));
            (from..from + len).map(|i| lp.iter().enumerate().map(|(k, g)| g * x[i + k]).sum()).collect()
        }

        /// Amplitude of `freq` in `x` (least-squares sinusoid) and the RMS of everything else.
        fn tone_and_residual(x: &[f64], freq: f64) -> (f64, f64) {
            let w = |i: usize| 2.0 * PI * freq * i as f64 / DECODED;
            let (re, im) = x.iter().enumerate().fold((0.0, 0.0), |(a, b), (i, v)| (a + v * w(i).cos(), b + v * w(i).sin()));
            let (re, im) = (2.0 * re / x.len() as f64, 2.0 * im / x.len() as f64);
            let resid = x.iter().enumerate().map(|(i, v)| (v - re * w(i).cos() - im * w(i).sin()).powi(2)).sum::<f64>() / x.len() as f64;
            ((re * re + im * im).sqrt(), resid.sqrt())
        }

        /// The amplitude of `freq` in `secs` (a whole number of its cycles) of decoded audio
        /// from `at` seconds, and how far down (dB) the rest of the audio band is.
        fn measure(decoded: &[f64], at: f64, secs: f64, freq: f64) -> (f64, f64) {
            let x = audio_band(decoded, (at * DECODED) as usize, (secs * DECODED) as usize);
            let (amp, resid) = tone_and_residual(&x, freq);
            (amp, 20.0 * (amp / resid).log10())
        }

        /// A PCM amplitude once encoded and decoded: PCM full scale is modulation depth `LEVEL`,
        /// which the converter scales by `PCM_GAIN`.
        fn decoded_amp(pcm: f64) -> f64 {
            pcm * LEVEL * PCM_GAIN
        }

        /// The markers alternate on every frame, both channels alike, for the whole life of a
        /// playback: its start, a pause, the resume, a seek, a dropout, a join, the end and
        /// after it. Paused and after the end, the payload is the idle pattern.
        #[test]
        fn the_markers_never_break() {
            let fmt = OutFormat::I32 { bits: 32 };
            let a_frames = 6 * 44_100 + 333;
            let a = tones(44_100, 1000.0, 3000.0, 0.5, 0, a_frames);
            let b = tones(44_100, 1000.0, 3000.0, 0.5, a_frames, 44_100 / 3);
            let p = start(a.clone(), &opts(DSD64_7, true));
            assert!(p.queue_next(b.clone()));
            // No blind switch, and no fade to wait for: no level can change after the modulator.
            assert!(!p.swap(a));
            let rs = p.shared().ring.as_ref().unwrap();
            let mut all = pull(&p, 10_000, fmt);
            p.set_paused(true);
            let paused: Vec<_> = (0..5).flat_map(|_| callback(&p, 512, fmt)).collect();
            assert!(paused.iter().flatten().all(|&w| payload(w) == DOP_IDLE), "paused: the idle pattern");
            all.extend(paused);
            p.fade_out();
            p.set_paused(false);
            all.extend(pull(&p, 5_000, fmt));
            let generation = rs.flush_gen.load(Ordering::Acquire);
            assert!(p.seek(0.5));
            let deadline = Instant::now() + Duration::from_secs(5);
            while rs.flush_gen.load(Ordering::Acquire) == generation {
                assert!(Instant::now() < deadline, "the render thread never took the seek up");
                std::thread::sleep(Duration::from_millis(1));
            }
            all.extend(pull(&p, 4_096, fmt));
            // A device asking far faster than the render thread runs: the ring runs dry.
            let before = p.underruns();
            while p.underruns() == before {
                assert!(Instant::now() < deadline + Duration::from_secs(10), "the ring never ran dry");
                assert!(!p.is_finished(), "ran to the end without a dropout");
                all.extend(callback(&p, 16_384, fmt));
            }
            // Then on to the end, through the join.
            all.extend(pull_all(&p, fmt));
            assert_eq!(p.switches(), 1);
            assert!(Arc::ptr_eq(&p.heard(), &b));
            let after: Vec<_> = (0..4).flat_map(|_| callback(&p, 512, fmt)).collect();
            assert!(after.iter().flatten().all(|&w| payload(w) == DOP_IDLE), "after the end: the idle pattern");
            all.extend(after);
            println!("{} frames, {} dropouts: markers checked on every one", all.len(), p.underruns());
            assert_eq!(marker(all[0][0]), 0x05);
            for (i, f) in all.iter().enumerate() {
                assert_eq!(marker(f[0]), marker(f[1]), "frame {i}: the channels' markers differ");
                assert_eq!(marker(f[0]), if i % 2 == 0 { 0x05 } else { 0xFA }, "frame {i}: the alternation breaks");
            }
        }

        /// 1 kHz on the left and 3 kHz on the right at half scale, from `fs` through the
        /// playback, decoded: each at its own level, the rest of the audio band 90 dB down.
        fn round_trip(fs: u32, out: DsdOut, format: OutFormat) {
            let frames = fs as usize / 2;
            let p = start(tones(fs, 1000.0, 3000.0, 0.5, 0, frames), &opts(out, true));
            // The meters show the PCM level that went into the encoder: the tone's 0.5, read
            // from the bit density over 256-bit windows (a rough estimate, the shaped noise in).
            let wire = out.rate.wire_rate() as usize;
            let mut pulled = pull(&p, wire / 10, format);
            p.take_meter();
            pulled.extend(pull(&p, wire / 5, format));
            let meter = p.take_meter();
            println!("meter on a 0.5 tone: {meter:?}");
            assert!(meter.iter().all(|m| (m / 0.5 - 1.0).abs() <= 0.15), "meter {meter:?}");
            pulled.extend(pull_all(&p, format));
            let decoded = decode(&pulled, out.rate);
            // All the music, then the encoder's delay and 2 ms more of coded silence.
            let wire = out.rate.wire_rate() as f64;
            let words = p.shared().ring.as_ref().unwrap().played.load(Ordering::Relaxed) as usize;
            let music = (frames as f64 * wire / fs as f64).round() as usize;
            let end = Encoder::new(out, 2).latency_frames() + (wire * 0.002).ceil() as usize;
            assert!(words >= music + end, "{words} words for {music} frames of music");
            let want = decoded_amp(0.5);
            for (c, f) in [(0, 1000.0), (1, 3000.0)] {
                let (amp, down) = measure(&decoded[c], 0.1, 0.2, f);
                println!(
                    "{} kHz → DSD{}/{} ({format:?}), ch{c}: amplitude {amp:.6} (want {want:.6}, {:+.3} %), residual {down:.1} dB down",
                    fs as f64 / 1000.0,
                    out.rate.multiple(),
                    out.order.get(),
                    100.0 * (amp / want - 1.0)
                );
                assert!((amp / want - 1.0).abs() <= 0.005, "ch{c}: amplitude {amp}, want {want}");
                assert!(down >= 90.0, "ch{c}: residual only {down:.1} dB down");
            }
            assert_eq!(p.underruns(), 0);
        }

        #[test]
        fn the_words_decode_to_the_source() {
            round_trip(44_100, DSD64_7, OutFormat::I32 { bits: 32 });
            round_trip(44_100, DSD64_5, OutFormat::I24Packed);
            round_trip(48_000, DSD64_7, OutFormat::F32);
            round_trip(176_400, DSD64_5, OutFormat::I32 { bits: 24 });
        }

        /// The same at DSD128 and DSD256:
        /// `cargo test --release -p hifi-engine -- --ignored decode_to_the_source_at_every --nocapture`
        #[test]
        #[ignore]
        fn the_words_decode_to_the_source_at_every_rate() {
            for rate in [DsdRate::Dsd128, DsdRate::Dsd256] {
                for order in ModOrder::ALL {
                    round_trip(44_100, DsdOut { rate, order }, OutFormat::I32 { bits: 32 });
                    round_trip(96_000, DsdOut { rate, order }, OutFormat::I24Packed);
                }
            }
        }

        /// The equaliser runs ahead of the modulator: a +6 dB peak at 1 kHz doubles the decoded
        /// tone, and an edit is heard once what was in the ring has played (about half a second).
        #[test]
        fn the_eq_runs_ahead_of_the_modulator_and_takes_edits() {
            let fmt = OutFormat::I24Packed;
            let eq = |gain: f64| EqPreset { phase: Phase::Minimum, bands: vec![Band { kind: FilterKind::Peak, freq: 1000.0, gain, q: 1.0, ..Default::default() }], ..Default::default() };
            let mut o = opts(DSD64_7, true);
            o.dsp.eq = Some(eq(6.0));
            let p = start(tones(44_100, 1000.0, 1000.0, 0.25, 0, 3 * 44_100), &o);
            let mut all = pull(&p, WIRE, fmt);
            assert!(p.update_eq(&eq(0.0), false));
            all.extend(pull_all(&p, fmt));
            let decoded = decode(&all, DsdRate::Dsd64);
            let flat = decoded_amp(0.25);
            let (boosted, _) = measure(&decoded[0], 0.5, 0.2, 1000.0);
            let want = flat * 10f64.powf(6.0 / 20.0);
            // A second after the edit was made: well after the ring and the EQ's crossfade.
            let (after, down) = measure(&decoded[0], 2.0, 0.2, 1000.0);
            let load = p.load().unwrap();
            println!("+6 dB at 1 kHz: {boosted:.6} (want {want:.6}, {:+.3} %); edited to 0 dB: {after:.6} (want {flat:.6}, {:+.3} %, residual {down:.1} dB down); peak {:?}, load {load:.4}", 100.0 * (boosted / want - 1.0), 100.0 * (after / flat - 1.0), p.output_peak());
            assert!((boosted / want - 1.0).abs() <= 0.005, "boosted {boosted}, want {want}");
            assert!((after / flat - 1.0).abs() <= 0.005 && down >= 90.0, "after the edit {after}, want {flat}");
            assert!(p.output_peak().is_some_and(|pk| (pk - 0.5).abs() < 0.01), "peak {:?}", p.output_peak());
            assert!(load > 0.0 && load < 1.0, "load {load}");
        }

        /// Volume levelling: the track's gain applies ahead of the modulator.
        #[test]
        fn the_track_level_applies_ahead_of_the_modulator() {
            let mut o = opts(DSD64_5, true);
            o.dsp.level = LevelSettings { mode: LevelMode::Track, headroom: false };
            let t = tones(44_100, 1000.0, 3000.0, 0.5, 0, 22_050);
            t.set_level(Some(Level { gain: 0.5, basis: Basis::Track, limited: false }));
            let p = start(t, &o);
            let decoded = decode(&pull_all(&p, OutFormat::F32), DsdRate::Dsd64);
            let want = decoded_amp(0.25);
            for (c, f) in [(0, 1000.0), (1, 3000.0)] {
                let (amp, _) = measure(&decoded[c], 0.1, 0.2, f);
                println!("at a gain of 0.5, ch{c}: {amp:.6} (want {want:.6}, {:+.3} %)", 100.0 * (amp / want - 1.0));
                assert!((amp / want - 1.0).abs() <= 0.005, "ch{c}: {amp}, want {want}");
            }
        }

        /// Two tracks cut from one stereo sine, joined: across the join the decoded signal is as
        /// clean as in the middle of a track (the encoder's state runs on), the heard track
        /// changes on the frame the second begins, and the position follows the wire frames.
        #[test]
        fn a_join_runs_on_through_the_encoder() {
            let fmt = OutFormat::I24Packed;
            // Longer than the ring, so B is lined up before the render thread reaches A's end.
            let a_frames = 44_100 + 777;
            let (a, b) = (tones(44_100, 1000.0, 3000.0, 0.5, 0, a_frames), tones(44_100, 1000.0, 3000.0, 0.5, a_frames, 44_100));
            let p = start(a, &opts(DSD64_7, true));
            assert!(p.queue_next(b.clone()));
            // 44.1 → 176.4 kHz: four wire frames per source frame.
            let (join, total) = (4 * a_frames, 4 * (a_frames + 44_100));
            let mut all = Vec::new();
            while !p.is_finished() {
                wait(&p);
                all.extend(callback(&p, 512, fmt));
                let heard_b = Arc::ptr_eq(&p.heard(), &b);
                assert_eq!(heard_b, all.len() >= join, "at {}", all.len());
                if all.len() <= total {
                    let at = if heard_b { all.len() - join } else { all.len() };
                    assert!((p.position_secs() - at as f64 / WIRE as f64).abs() < 1e-6, "at {}: {}", all.len(), p.position_secs());
                }
            }
            assert_eq!(p.switches(), 1);
            assert_eq!(p.underruns(), 0);
            let decoded = decode(&all, DsdRate::Dsd64);
            let at_join = join as f64 / DECODED;
            for (c, f) in [(0, 1000.0), (1, 3000.0)] {
                let (_, middle) = measure(&decoded[c], 0.3, 0.2, f);
                let (amp, across) = measure(&decoded[c], at_join - 0.1, 0.2, f);
                println!("ch{c}: residual {middle:.1} dB down in the middle of A, {across:.1} dB down across the join; amplitude there {amp:.6}");
                assert!(across >= 90.0 && across >= middle - 3.0, "ch{c}: across the join {across:.1} dB, in the middle {middle:.1} dB");
                assert!((amp / decoded_amp(0.5) - 1.0).abs() <= 0.005);
            }
            let load = p.load().unwrap();
            println!("load {load:.4} ({:.0}× real time)", 1.0 / load);
            assert!(load > 0.0 && load < 1.0, "load {load}");
        }

        /// The decoded rise of a steady level from the start of `frames`: how long (ms) it takes
        /// from 10 % to 90 % of `full`, and where it settles.
        fn rise(frames: &[[u32; 2]], full: f64) -> (f64, f64) {
            let decoded = decode(frames, DsdRate::Dsd64);
            let y = audio_band(&decoded[0], 0, decoded[0].len() - 511);
            let first = |level: f64| y.iter().position(|&v| v >= level * full).expect("never gets there") as f64;
            let settled = y[y.len() - 1_000..].iter().sum::<f64>() / 1_000.0;
            ((first(0.9) - first(0.1)) / DECODED * 1000.0, settled / full)
        }

        /// Seek, let the callback drop what was queued, and pull what comes after.
        fn seek(p: &Playback, secs: f64, fmt: OutFormat) -> Vec<[u32; 2]> {
            let rs = p.shared().ring.as_ref().unwrap();
            let generation = rs.flush_gen.load(Ordering::Acquire);
            assert!(p.seek(secs));
            let deadline = Instant::now() + Duration::from_secs(5);
            while rs.flush_gen.load(Ordering::Acquire) == generation {
                assert!(Instant::now() < deadline, "the render thread never took the seek up");
                std::thread::sleep(Duration::from_millis(1));
            }
            // The flush, then the new place from its first frame.
            let mut out = callback(p, 512, fmt);
            out.extend(pull(p, 8_192, fmt));
            out
        }

        /// With fades on, what a seek lands in the middle of comes up over 10 ms (the half
        /// cosine: 10 → 90 % in 5.9 ms) ahead of the encoder; a seek to the top, a playback
        /// with fades off, starts at full level; one started in the middle fades in.
        #[test]
        fn a_seek_into_the_middle_fades_in_ahead_of_the_encoder() {
            let fmt = OutFormat::F32;
            // A steady level: the fade shows as its own shape.
            let track = tones(44_100, 0.0, 0.0, 0.5, 0, 3 * 44_100);
            let full = decoded_amp(0.5);
            // 10 → 90 % of a half cosine over T: T·(acos(−0.8) − acos(0.8))/π.
            let shape = 10.0 * ((-0.8f64).acos() - 0.8f64.acos()) / PI;
            let p = start(track.clone(), &opts(DSD64_7, true));
            pull(&p, 20_000, fmt);
            let (middle, level) = rise(&seek(&p, 1.0, fmt), full);
            println!("seek to 1 s: 10 → 90 % in {middle:.3} ms (a 10 ms half cosine: {shape:.3}), then {level:.5} of full");
            assert!((middle - shape).abs() < 0.3, "{middle} ms");
            assert!((level - 1.0).abs() < 0.005, "settles at {level}");
            let (top, level) = rise(&seek(&p, 0.0, fmt), full);
            println!("seek to the top: 10 → 90 % in {top:.3} ms");
            assert!(top < 0.3 && (level - 1.0).abs() < 0.005, "{top} ms, settles at {level}");
            drop(p);

            let plain = start(track.clone(), &opts(DSD64_7, false));
            pull(&plain, 20_000, fmt);
            let (off, _) = rise(&seek(&plain, 1.0, fmt), full);
            println!("fades off: 10 → 90 % in {off:.3} ms");
            assert!(off < 0.3, "{off} ms");
            drop(plain);

            let o = PlayOptions { start_at: 1.0, ..opts(DSD64_7, true) };
            let mid = start(track, &o);
            let (picked_up, _) = rise(&pull(&mid, 8_192, fmt), full);
            println!("started at 1 s: 10 → 90 % in {picked_up:.3} ms");
            assert!((picked_up - shape).abs() < 0.3, "{picked_up} ms");
        }

        /// Planned only where the device can take it, and otherwise exactly as if it weren't
        /// asked for; where it is, to the wire rate whatever "upsample to" says, through the
        /// chosen long filter, and never called bit-perfect.
        #[test]
        fn planned_only_where_the_device_takes_it() {
            let speakers = crate::channels::default_layout(2);
            let pcm = tones(44_100, 1000.0, 1000.0, 0.5, 0, 4_410);
            let spec = FilterSpec { kind: crate::dsp::upsample::FilterKind::Linear, taps: 65_536 };
            let physical = StreamFormat { sample_rate: 176_400.0, bits: 24, float: false, channels: 2, non_mixable: true };
            let with = |upsample: Upsample, exclusive: bool| PlayOptions {
                exclusive,
                dsp: DspSettings { upsample, upsample_filter: Some(spec), dsd_out: Some(DSD64_7), ..Default::default() },
                ..Default::default()
            };
            let without = |o: &PlayOptions| PlayOptions { dsp: DspSettings { dsd_out: None, ..o.dsp.clone() }, ..o.clone() };
            let fields = |p: &Plan| (p.target, p.start_frame, p.lossy, p.resampled, p.processing, p.dop, p.encode, p.route.clone());
            let only_44k = vec![RateRange::exact(44_100.0), RateRange::exact(96_000.0)];
            let only_88k = vec![RateRange::exact(88_200.0)];
            let cases = [
                ("shared mode", with(Upsample::Off, false), true, dac_rates()),
                ("no DoP", with(Upsample::Off, true), false, dac_rates()),
                ("no wire rate", with(Upsample::Off, true), true, only_44k),
                // Resampled to twice the source rate, the long filter chosen: still the standard
                // one, as upsampling is off.
                ("no DoP, 88.2 kHz only", with(Upsample::Off, true), false, only_88k),
            ];
            for (name, o, dop_ok, rates) in cases {
                let p = plan(&pcm, &o, &rates, "DAC", dop_ok, &speakers).unwrap();
                assert_eq!(p.encode, None, "{name}");
                assert_eq!(fields(&p), fields(&plan(&pcm, &without(&o), &rates, "DAC", dop_ok, &speakers).unwrap()), "{name}");
                let path = signal_path(&pcm, &o, &p, p.target, Some(physical), OutFormat::I24Packed, o.exclusive, Transport::Usb);
                let plain = signal_path(&pcm, &without(&o), &p, p.target, Some(physical), OutFormat::I24Packed, o.exclusive, Transport::Usb);
                assert_eq!(path.dsd_out, None, "{name}");
                assert_eq!(path.upsample_filter, None, "{name}");
                assert_eq!((path.bit_perfect, path.resampled, path.device_rate), (plain.bit_perfect, plain.resampled, plain.device_rate), "{name}");
            }

            // A DSD track sent straight ignores it.
            let dsd = Track::new_dop(2_822_400, 2, TrackMeta::default(), 1_000);
            dsd.push_dop(&vec![0x69; 4_000]);
            dsd.finish(None);
            let o = with(Upsample::Off, true);
            let p = plan(&dsd, &o, &dac_rates(), "DAC", true, &speakers).unwrap();
            assert!(p.dop && p.encode.is_none() && !p.processing);
            assert_eq!(fields(&p), fields(&plan(&dsd, &without(&o), &dac_rates(), "DAC", true, &speakers).unwrap()));
            let path = signal_path(&dsd, &o, &p, 176_400.0, Some(physical), OutFormat::I24Packed, true, Transport::Usb);
            assert!(path.bit_perfect && path.dsd_out.is_none());
            assert_eq!(wire_dsd_rate(&dsd, &o), Some(2_822_400));

            // Where it can: the wire rate, whatever "upsample to" says.
            assert_eq!(wire_dsd_rate(&pcm, &o), Some(2_822_400));
            assert_eq!(wire_dsd_rate(&pcm, &without(&o)), None);
            for upsample in [Upsample::Off, Upsample::UpTo(96_000.0), Upsample::UpTo(768_000.0)] {
                let o = with(upsample, true);
                let p = plan(&pcm, &o, &dac_rates(), "DAC", true, &speakers).unwrap();
                assert_eq!(p.encode, Some(DSD64_7), "{upsample:?}");
                assert!(p.dop && p.processing && p.resampled, "{upsample:?}");
                assert_eq!(p.target, 176_400.0, "{upsample:?}");
                let path = signal_path(&pcm, &o, &p, 176_400.0, Some(physical), OutFormat::I32 { bits: 32 }, true, Transport::Usb);
                assert_eq!(path.dsd_out, Some(DSD64_7));
                assert!(!path.bit_perfect, "{upsample:?}: encoded PCM is never bit-perfect");
                assert_eq!(path.upsample_filter, Some(spec), "{upsample:?}");
                assert_eq!((path.device_rate, path.resampled), (176_400.0, true));
            }
            // A source at the wire rate itself: encoded, not resampled.
            let at_wire = tones(176_400, 1000.0, 1000.0, 0.5, 0, 1_000);
            let p = plan(&at_wire, &with(Upsample::Off, true), &dac_rates(), "DAC", true, &speakers).unwrap();
            assert!(p.encode.is_some() && p.processing && !p.resampled && p.target == 176_400.0);
            // DSD256 at its own wire rate.
            let o = PlayOptions { dsp: DspSettings { dsd_out: Some(DsdOut { rate: DsdRate::Dsd256, order: ModOrder::Fifth }), ..Default::default() }, ..Default::default() };
            assert_eq!(plan(&pcm, &o, &dac_rates(), "DAC", true, &speakers).unwrap().target, 705_600.0);
            // The long filter runs with DSD output whether or not upsampling is on.
            let filtered = DspSettings { upsample_filter: Some(spec), ..Default::default() };
            assert_eq!(crate::dsp::long_filter(&DspSettings { dsd_out: Some(DSD64_7), ..filtered.clone() }, 44_100, 176_400), Some(spec));
            assert_eq!(crate::dsp::long_filter(&filtered, 44_100, 176_400), None);
        }

        /// The blind test's offline render plays PCM, whatever the settings say about DSD.
        #[test]
        fn rendered_offline_it_stays_pcm() {
            let t = tones(44_100, 1000.0, 3000.0, 0.5, 0, 22_050);
            let base = DspSettings { crossfeed: Some(CrossfeedLevel::DEFAULT), ..Default::default() };
            let plain = render_offline(t.clone(), &base).unwrap();
            let asked = render_offline(t, &DspSettings { dsd_out: Some(DSD64_7), ..base }).unwrap();
            assert_eq!(plain.len(), 2 * 22_050);
            assert!(asked == plain, "the same samples with dsd_out set");
            assert!(asked.iter().all(|v| v.abs() <= 1.0), "samples, not words");
        }
    }

    fn stereo_route() -> Route {
        Route::new(&crate::channels::default_layout(2), &crate::channels::default_layout(2))
    }

    /// Pause, seek and stop fade over 10 ms instead of clicking. A track playing through, and
    /// every sample once a fade is over, is untouched.
    mod fades {
        use super::*;
        use crate::decode::TrackMeta;

        const FS: u32 = 48_000;
        /// 10 ms at 48 kHz.
        const FADE: usize = 480;
        /// The steepest step a 10 ms fade of a 0.5 signal makes between two samples.
        const SMOOTH: f32 = 0.5 * std::f32::consts::PI / 2.0 / FADE as f32 * 1.05;

        /// A steady 0.5 on both channels: a fade shows as its own shape.
        fn steady(frames: usize) -> Arc<Track> {
            Arc::new(Track::from_samples(FS, 2, Some(24), &vec![0.5; frames * 2], TrackMeta::default()))
        }

        /// Every sample distinct: (i + 1) / 2^20.
        fn ramp(frames: usize) -> Arc<Track> {
            let samples: Vec<f32> = (0..frames).flat_map(|i| [value(i), -value(i)]).collect();
            Arc::new(Track::from_samples(FS, 2, Some(24), &samples, TrackMeta::default()))
        }

        fn value(frame: usize) -> f32 {
            (frame + 1) as f32 / 1_048_576.0
        }

        fn playback(track: Arc<Track>, processing: bool, start_frame: usize, fades: bool) -> Playback {
            let plan = Plan { target: FS as f64, start_frame, lossy: None, resampled: false, processing, dop: false, encode: None, route: stereo_route() };
            Playback::new(track, &PlayOptions { fades, ..Default::default() }, &plan, FS as f64).unwrap()
        }

        /// One callback of `frames` stereo F32 frames; returns the left channel.
        fn callback(p: &Playback, frames: usize) -> Vec<f32> {
            let mut data = vec![0u8; frames * 8];
            p.shared().render(&mut [OutBuf { data: &mut data, channels: 2, format: OutFormat::F32 }]);
            data.chunks_exact(8).map(|f| f32::from_le_bytes(f[..4].try_into().unwrap())).collect()
        }

        /// Callbacks of 512 frames from the processed path, each once the ring can serve it
        /// (or has been told to start over), like a device's clock would ask.
        fn pull(p: &Playback, frames: usize) -> Vec<f32> {
            let rs = p.shared().ring.as_ref().unwrap();
            let mut out = Vec::new();
            while out.len() < frames {
                let deadline = Instant::now() + Duration::from_secs(5);
                while rs.ring.available() < 512 * 2 && !rs.render_done.load(Ordering::Acquire) {
                    assert!(Instant::now() < deadline, "the render thread stalled");
                    std::thread::sleep(Duration::from_millis(1));
                }
                out.extend(callback(p, 512));
            }
            out
        }

        fn steepest(x: &[f32]) -> f32 {
            x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max)
        }

        /// A steady `level` on both channels.
        fn steady_at(level: f32, frames: usize) -> Arc<Track> {
            Arc::new(Track::from_samples(FS, 2, Some(24), &vec![level; frames * 2], TrackMeta::default()))
        }

        /// A blind comparison's switch on the direct path: the playing track fades out, the
        /// other fades in from the very frame the first stopped at, and from then on its
        /// samples go out as they are.
        #[test]
        fn a_swap_carries_on_with_the_other_track_at_the_same_frame() {
            let (a, b) = (ramp(FS as usize), steady_at(0.25, FS as usize));
            let p = playback(a.clone(), false, 0, true);
            let first = callback(&p, 1_000);
            assert_eq!(first[999], value(999));
            assert!(p.swap(b.clone()));
            let out = callback(&p, 2_000);
            // The ramp fades out over 10 ms…
            assert!(out[FADE - 1] == 0.0 && out[0] > 0.0);
            // …then the other track fades in, and plays untouched.
            assert!(out[FADE] > 0.0 && out[FADE] < 0.01);
            assert!(out[FADE..2 * FADE].windows(2).all(|w| w[1] >= w[0]));
            assert!(out[2 * FADE..].iter().all(|&v| v == 0.25));
            assert!(Arc::ptr_eq(&p.heard(), &b));
            // No frame was skipped or repeated: the position is what was played.
            assert!((p.position_secs() - 3_000.0 / FS as f64).abs() < 1e-9);
            // Back again: the ramp's own samples, from where the other one stopped.
            assert!(p.swap(a.clone()));
            let back = callback(&p, 2_000);
            assert_eq!(back[1_999], value(3_000 + FADE + 1_999 - FADE));
            assert!(Arc::ptr_eq(&p.heard(), &a));
            // While paused there is nothing to fade: the switch is made at once, silently.
            p.set_paused(true);
            callback(&p, 1_000);
            assert!(p.swap(b.clone()));
            assert!(callback(&p, 100).iter().all(|&v| v == 0.0));
            assert!(Arc::ptr_eq(&p.heard(), &b));
            p.set_paused(false);
            let resumed = callback(&p, 1_000);
            assert!(resumed[0] < 0.01 && resumed[999] == 0.25);
        }

        /// Asked twice before the device wants audio, the last one asked for plays; tracks of
        /// another rate or shape are refused. A swap to the track that already plays fades
        /// just like one to another track: a blind test's switch must sound the same whether
        /// or not X is what was playing.
        #[test]
        fn a_swap_is_refused_where_the_device_would_have_to_switch() {
            let (a, b, c) = (steady_at(0.5, FS as usize), steady_at(0.25, FS as usize), steady_at(0.125, FS as usize));
            let p = playback(a.clone(), false, 0, true);
            callback(&p, 1_000);
            assert!(p.swap(b.clone()) && p.swap(c.clone()));
            callback(&p, 2_000);
            assert!(Arc::ptr_eq(&p.heard(), &c));
            assert!(p.swap(c.clone()));
            let same = callback(&p, 2_000);
            assert!(same[FADE - 1] == 0.0 && same[2 * FADE..].iter().all(|&v| v == 0.125), "the same fade as to another track");
            assert!((p.position_secs() - 5_000.0 / FS as f64).abs() < 1e-9);
            let other_rate = Arc::new(Track::from_samples(96_000, 2, Some(24), &vec![0.5; 2_000], TrackMeta::default()));
            let mono = Arc::new(Track::from_samples(FS, 1, Some(24), &vec![0.5; 2_000], TrackMeta::default()));
            assert!(!p.swap(other_rate) && !p.swap(mono));
            assert!(Arc::ptr_eq(&p.heard(), &c));
        }

        /// The same switch through the processing chain: the render thread carries on with
        /// the other track from the frame being heard, behind a fade.
        #[test]
        fn a_swap_on_the_processed_path_keeps_the_position() {
            let (a, b) = (steady_at(0.5, FS as usize * 4), steady_at(0.25, FS as usize * 4));
            let p = playback(a, true, 0, true);
            let first = pull(&p, 4_096);
            assert!(first.iter().all(|&v| v == 0.5));
            let before = p.position_secs();
            assert!(p.swap(b.clone()));
            // (A device asks for a buffer every few milliseconds; these callbacks come as
            // fast as they can, so give the render thread its moment to take the swap up.)
            let rs = p.shared().ring.as_ref().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while rs.flush_gen.load(Ordering::Acquire) == rs.flush_ack.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "the render thread never took the swap up");
                std::thread::sleep(Duration::from_millis(1));
            }
            let out = pull(&p, 8_192);
            let settled = out.iter().rposition(|&v| v != 0.25).map_or(0, |i| i + 1);
            assert!(settled < 4_096, "the other track is heard within a few buffers ({settled})");
            assert!(out[0] > 0.25 && steepest(&out) < SMOOTH, "a step of {}", steepest(&out));
            assert!(out.iter().any(|&v| v == 0.0), "down to silence in between");
            while !Arc::ptr_eq(&p.heard(), &b) {
                assert!(Instant::now() < deadline, "the other track never became the heard one");
                std::thread::sleep(Duration::from_millis(1));
            }
            // The position moved on by what was heard of the music (not by the silence).
            let moved = p.position_secs() - before;
            assert!(moved > 0.05 && moved < 8_192.0 / FS as f64 + 0.01, "{moved}");
        }

        #[test]
        fn pause_fades_out_and_resume_fades_in() {
            let p = playback(steady(FS as usize), false, 0, true);
            let a = callback(&p, 1_000);
            assert!(a.iter().all(|&v| v == 0.5), "a track from its top starts at full level");
            p.set_paused(true);
            let b = callback(&p, 1_000);
            assert!(b[0] > 0.49 && b[FADE - 1] == 0.0);
            assert!(b[..FADE].windows(2).all(|w| w[1] <= w[0]), "falls all the way");
            assert!(b[FADE..].iter().all(|&v| v == 0.0));
            // What faded out was played: the position counts it.
            assert!((p.position_secs() - (1_000 + FADE) as f64 / FS as f64).abs() < 1e-9);
            assert!(callback(&p, 300).iter().all(|&v| v == 0.0));
            p.set_paused(false);
            let c = callback(&p, 1_000);
            assert!(c[0] > 0.0 && c[0] < 0.01);
            assert!(c[..FADE].windows(2).all(|w| w[1] >= w[0]), "rises all the way");
            assert!(c[FADE - 1..].iter().all(|&v| v == 0.5), "then every sample as it is");
            let all: Vec<f32> = a.into_iter().chain(b).chain(c).collect();
            assert!(steepest(&all) < SMOOTH, "a step of {}", steepest(&all));
        }

        #[test]
        fn a_fade_longer_than_the_buffer_carries_over() {
            let p = playback(steady(FS as usize), false, 0, true);
            callback(&p, 64);
            p.set_paused(true);
            let out: Vec<f32> = (0..10).flat_map(|_| callback(&p, 64)).collect();
            assert!(out[0] > 0.49 && out[FADE - 1] == 0.0 && out[FADE..].iter().all(|&v| v == 0.0));
            assert!(steepest(&out) < SMOOTH);
            // Resumed halfway down: back up from where it was, not from silence.
            p.set_paused(false);
            callback(&p, 64);
            p.set_paused(true);
            let down = callback(&p, 32);
            p.set_paused(false);
            let up = callback(&p, 64);
            assert!(down[31] > 0.0 && (up[0] - down[31]).abs() < SMOOTH);
        }

        #[test]
        fn a_seek_fades_out_then_in_and_lands_on_the_exact_samples() {
            let p = playback(ramp(FS as usize), false, 0, true);
            let a = callback(&p, 1_000);
            assert!(a.iter().enumerate().all(|(i, &v)| v == value(i)));
            assert!(p.seek(0.5));
            let b = callback(&p, 2_000);
            // The old place goes on for 10 ms, falling…
            assert!(b[0] > value(1_000) * 0.99 && b[FADE - 1] == 0.0);
            // …the new one comes up over the next 10…
            let at = FS as usize / 2;
            assert!(b[FADE] > 0.0 && b[FADE] < value(at) * 0.01);
            assert!(b[FADE..2 * FADE].windows(2).all(|w| w[1] > w[0]));
            // …and from there on it is the file, bit for bit.
            assert!((2 * FADE - 1..2_000).all(|i| b[i] == value(at + i - FADE)));
            assert!((p.position_secs() - (at + 2_000 - FADE) as f64 / FS as f64).abs() < 1e-9);
        }

        /// A seek left over for a track that has since been replaced is dropped without a
        /// sound: no fade out and back in at the same place.
        #[test]
        fn a_seek_for_another_track_is_not_heard() {
            let p = playback(ramp(FS as usize), false, 0, true);
            callback(&p, 1_000);
            let other = ramp(100);
            let slots = &p.shared().slots;
            slots.seek_track.store(ptr(&other), Ordering::Release);
            slots.seek_req.store(50, Ordering::Release);
            let out = callback(&p, 1_000);
            assert!(out.iter().enumerate().all(|(i, &v)| v == value(1_000 + i)), "plays straight on");
            assert_eq!(slots.seek_req.load(Ordering::Acquire), NO_SEEK, "and the request is gone");
        }

        #[test]
        fn a_track_picked_up_in_the_middle_fades_in() {
            let p = playback(ramp(FS as usize), false, 10_000, true);
            let a = callback(&p, 1_000);
            assert!(a[0] < value(10_000) * 0.01);
            assert!((FADE - 1..1_000).all(|i| a[i] == value(10_000 + i)));
        }

        /// Started paused (the app restoring where it was): silent and still until resumed,
        /// then the track's first samples exactly — on both paths.
        #[test]
        fn a_playback_started_paused_makes_no_sound_and_starts_exact() {
            for processing in [false, true] {
                let plan = Plan { target: FS as f64, start_frame: 0, lossy: None, resampled: false, processing, dop: false, encode: None, route: stereo_route() };
                let p = Playback::new(ramp(FS as usize), &PlayOptions { start_paused: true, ..Default::default() }, &plan, FS as f64).unwrap();
                for _ in 0..3 {
                    assert!(callback(&p, 512).iter().all(|&v| v == 0.0), "paused: silence");
                }
                assert_eq!(p.position_secs(), 0.0);
                p.fade_out(); // nothing sounds: nothing to wait for
                p.set_paused(false);
                let out = if processing { pull(&p, 2_048) } else { callback(&p, 2_048) };
                assert!(out.iter().enumerate().all(|(i, &v)| v == value(i)), "processing {processing}: from the first sample, as it is");
            }
        }

        /// Back to the top of the track (the "previous" button): the old place fades out, the
        /// track then starts with its own first samples, untouched.
        #[test]
        fn a_seek_to_the_top_starts_the_track_exact() {
            let p = playback(ramp(FS as usize), false, 0, true);
            callback(&p, 5_000);
            assert!(p.seek(0.0));
            let out = callback(&p, 2_000);
            assert_eq!(out[FADE - 1], 0.0);
            assert!((FADE..2_000).all(|i| out[i] == value(i - FADE)));
            // Seeking while paused, before anything sounded since: no fade-out of the old place.
            p.set_paused(true);
            callback(&p, 1_000);
            assert!(p.seek(0.5));
            assert!(callback(&p, 1_000).iter().all(|&v| v == 0.0));
            p.set_paused(false);
            let out = callback(&p, 1_000);
            assert!(out[0] < value(FS as usize / 2) * 0.01, "picked up mid-track: fades in");
            assert_eq!(out[999], value(FS as usize / 2 + 999));
        }

        /// The setting takes hold on what plays, without a restart — and switching it in the
        /// middle of things makes no sound of its own.
        #[test]
        fn fades_can_be_switched_while_playing() {
            for processing in [false, true] {
                let p = playback(steady(FS as usize * 4), processing, 0, true);
                let play = |n: usize| if processing { pull(&p, n) } else { callback(&p, n) };
                assert!(play(1_024).iter().all(|&v| v == 0.5));
                // Off: a pause cuts at once, a resume starts at once.
                p.set_fades(false);
                assert!(play(1_024).iter().all(|&v| v == 0.5), "switching off is not heard");
                p.set_paused(true);
                assert!(callback(&p, 512).iter().all(|&v| v == 0.0));
                // On again while paused: still silent (no tail of what was cut), then a fade in.
                p.set_fades(true);
                assert!(callback(&p, 512).iter().all(|&v| v == 0.0), "processing {processing}");
                p.set_paused(false);
                let up = play(1_024);
                assert!(up[0] < 0.01 && up[FADE - 1..].iter().all(|&v| v == 0.5));
                // Off and on again while it plays: not heard either.
                p.set_fades(false);
                assert!(play(1_024).iter().all(|&v| v == 0.5));
                p.set_fades(true);
                assert!(play(1_024).iter().all(|&v| v == 0.5), "switching on is not heard");
                p.set_paused(true);
                let down = callback(&p, 1_024);
                assert!(down[0] > 0.49 && down[FADE - 1] == 0.0);
            }
        }

        #[test]
        fn without_fades_nothing_is_shaped() {
            let p = playback(ramp(FS as usize), false, 10_000, false);
            assert_eq!(callback(&p, 16)[0], value(10_000));
            assert!(p.seek(0.5));
            assert_eq!(callback(&p, 16)[0], value(FS as usize / 2));
            p.set_paused(true);
            assert!(callback(&p, 16).iter().all(|&v| v == 0.0));
            p.fade_out(); // nothing to wait for
        }

        #[test]
        fn the_processed_path_fades_the_same_way() {
            let p = playback(steady(FS as usize * 4), true, 0, true);
            let mut all = pull(&p, 4_096);
            assert!(all.iter().all(|&v| v == 0.5));
            p.set_paused(true);
            let b = callback(&p, 1_024);
            assert!(b[0] > 0.49 && b[FADE - 1] == 0.0 && b[FADE..].iter().all(|&v| v == 0.0));
            let before = p.position_secs();
            assert!((before - (4_096 + FADE) as f64 / FS as f64).abs() < 1e-9);
            all.extend(b);
            p.set_paused(false);
            let c = pull(&p, 1_024);
            assert!(c[0] < 0.01 && c[FADE - 1..].iter().all(|&v| v == 0.5));
            all.extend(c);
            // A seek: the queued sound fades out before it is thrown away, the new place fades in.
            assert!(p.seek(2.0));
            // (The render thread takes the seek up a moment later.)
            let rs = p.shared().ring.as_ref().unwrap();
            while rs.flush_gen.load(Ordering::Acquire) == rs.flush_ack.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            let d = pull(&p, 8_192);
            assert!(d[0] > 0.49 && d[FADE - 1] == 0.0);
            assert!(d[d.len() - 512..].iter().all(|&v| v == 0.5));
            all.extend(d);
            assert!(steepest(&all) < SMOOTH, "a step of {}", steepest(&all));
            // The position starts over at the seek: the faded-out tail isn't counted into it.
            let pos = p.position_secs();
            assert!((2.0..2.0 + (8_192 - FADE) as f64 / FS as f64 + 1e-9).contains(&pos), "{pos}");
            assert_eq!(p.underruns(), 0);
        }

        #[test]
        fn fade_out_waits_for_the_device_to_get_the_fade() {
            let p = playback(steady(FS as usize * 4), false, 0, true);
            let stop = AtomicBool::new(false);
            let heard = std::thread::scope(|s| {
                let device = s.spawn(|| {
                    let mut out = Vec::new();
                    while !stop.load(Ordering::Acquire) {
                        out.extend(callback(&p, 256));
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    out
                });
                std::thread::sleep(Duration::from_millis(30));
                let started = Instant::now();
                p.fade_out();
                assert!(started.elapsed() < Duration::from_millis(200));
                stop.store(true, Ordering::Release);
                device.join().unwrap()
            });
            assert!(p.is_paused());
            assert_eq!(*heard.last().unwrap(), 0.0);
            assert!(steepest(&heard) < SMOOTH);
        }
    }

    /// Volume levelling: each track at the gain set on it.
    mod levelling {
        use super::*;
        use crate::decode::TrackMeta;
        use crate::dsp::loudness::{Basis, Level, LevelMode, LevelSettings};

        fn ramp(start: usize, frames: usize) -> Arc<Track> {
            let samples: Vec<f32> = (start..start + frames).flat_map(|i| [value(i), -value(i)]).collect();
            Arc::new(Track::from_samples(48_000, 2, Some(24), &samples, TrackMeta::default()))
        }

        fn value(frame: usize) -> f32 {
            (frame + 1) as f32 / 1_048_576.0
        }

        fn at(gain: f64) -> Option<Level> {
            Some(Level { gain, basis: Basis::Track, limited: false })
        }

        fn levelled() -> PlayOptions {
            let level = LevelSettings { mode: LevelMode::Track, headroom: false };
            PlayOptions { dsp: DspSettings { level, ..Default::default() }, ..Default::default() }
        }

        /// Play `a` then `b` joined, through the processed path; returns the left channel.
        fn play(a: Arc<Track>, b: Arc<Track>, opts: &PlayOptions, frames: usize) -> Vec<f32> {
            let plan = Plan { target: 48_000.0, start_frame: 0, lossy: None, resampled: false, processing: true, dop: false, encode: None, route: stereo_route() };
            let p = Playback::new(a, opts, &plan, 48_000.0).unwrap();
            assert!(p.queue_next(b));
            let rs = p.shared().ring.as_ref().unwrap();
            let mut out = Vec::new();
            while out.len() < frames {
                while rs.ring.available() < 512 * 2 && !rs.render_done.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                let mut data = vec![0u8; 512 * 8];
                p.shared().render(&mut [OutBuf { data: &mut data, channels: 2, format: OutFormat::F32 }]);
                out.extend(data.chunks_exact(8).map(|f| f32::from_le_bytes(f[..4].try_into().unwrap())));
            }
            out
        }

        #[test]
        fn each_track_plays_at_its_own_gain_to_the_sample() {
            // A is longer than the ring, so B is lined up before the render thread gets to its end.
            let (a, b) = (ramp(0, 60_000), ramp(60_000, 20_000));
            a.set_level(at(0.5));
            b.set_level(at(0.25));
            let out = play(a, b, &levelled(), 80_000);
            // (Halving and quartering are exact in floating point.)
            assert!((0..60_000).all(|i| out[i] == value(i) * 0.5), "the first track at its gain");
            assert!((60_000..80_000).all(|i| out[i] == value(i) * 0.25), "the next one at its own, from its first sample");
        }

        #[test]
        fn a_gain_left_on_a_track_does_nothing_with_levelling_off() {
            let (a, b) = (ramp(0, 60_000), ramp(60_000, 20_000));
            a.set_level(at(0.5));
            let out = play(a, b, &PlayOptions::default(), 80_000);
            assert!((0..80_000).all(|i| out[i] == value(i)));
        }

        #[test]
        fn levelling_takes_the_processed_path_and_says_so() {
            let rates = [RateRange::exact(48_000.0)];
            let speakers = crate::channels::default_layout(2);
            let t = ramp(0, 48_000);
            let plain = plan(&t, &PlayOptions::default(), &rates, "DAC", true, &speakers).unwrap();
            assert!(!plain.processing);
            // On, with nothing known about the track yet: it still goes through the chain, so
            // the next one can join at its own gain.
            let opts = levelled();
            let pl = plan(&t, &opts, &rates, "DAC", true, &speakers).unwrap();
            assert!(pl.processing);
            t.set_level(at(0.5));
            let physical = StreamFormat { sample_rate: 48_000.0, bits: 24, float: false, channels: 2, non_mixable: true };
            let path = signal_path(&t, &opts, &pl, 48_000.0, Some(physical), OutFormat::I24Packed, true, Transport::Usb);
            assert!(!path.bit_perfect);
            assert_eq!(path.level, at(0.5));
            // The track after it shows its own.
            let next = ramp(0, 100);
            next.set_level(at(0.25));
            assert_eq!(path.for_next(&next).level, at(0.25));
            // With levelling off the path is bit-perfect and shows no gain, whatever the track carries.
            let clean = signal_path(&t, &PlayOptions::default(), &plain, 48_000.0, Some(physical), OutFormat::I24Packed, true, Transport::Usb);
            assert!(clean.bit_perfect && clean.level.is_none());
        }
    }

    /// Multichannel: 5.1 straight onto a 5.1 device, folded onto a stereo one.
    mod multichannel {
        use super::*;
        use crate::channels::default_layout;
        use crate::decode::TrackMeta;

        /// 5.1 where each channel carries its own constant: FL 0.1, FR 0.2, C 0.3, LFE 0.4, BL 0.5, BR 0.6.
        fn five_one(frames: usize) -> Arc<Track> {
            let samples: Vec<f32> = (0..frames).flat_map(|_| [0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6]).collect();
            Arc::new(Track::from_samples(48_000, 6, Some(24), &samples, TrackMeta::default()))
        }

        fn run(device: &[Speaker], processing: bool) -> Vec<f32> {
            let track = five_one(48_000);
            let opts = PlayOptions::default();
            let mut plan = plan(&track, &opts, &[RateRange::exact(48_000.0)], "test", false, device).unwrap();
            plan.processing = processing;
            let p = Playback::new(track, &opts, &plan, 48_000.0).unwrap();
            let ch = device.len();
            let frames = 256;
            let mut data = vec![0u8; frames * ch * 4];
            if let Some(rs) = p.shared().ring.as_ref() {
                while rs.ring.available() < frames * plan.route.program_channels() {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            p.shared().render(&mut [OutBuf { data: &mut data, channels: ch, format: OutFormat::F32 }]);
            // The first frame, every device channel.
            data[..ch * 4].chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect()
        }

        #[test]
        fn five_one_plays_channel_for_channel_on_a_five_one_device() {
            // CoreAudio's 5.1 order: L R Ls Rs C LFE.
            let device = [Speaker::FrontLeft, Speaker::FrontRight, Speaker::SideLeft, Speaker::SideRight, Speaker::FrontCenter, Speaker::Lfe];
            for processing in [false, true] {
                let got = run(&device, processing);
                assert_eq!(got, [0.1, 0.2, 0.5, 0.6, 0.3, 0.4], "processing {processing}");
            }
            let track = five_one(10);
            let opts = PlayOptions::default();
            let p = plan(&track, &opts, &[RateRange::exact(48_000.0)], "test", false, &device).unwrap();
            let path = signal_path(&track, &opts, &p, 48_000.0, None, OutFormat::F32, true, Transport::Usb);
            assert_eq!(path.layout, "5.1");
            assert!(path.downmix.is_none());
        }

        #[test]
        fn five_one_folds_onto_stereo_the_same_either_way() {
            let device = default_layout(2);
            let want_l = (0.1 + 0.3 * std::f32::consts::FRAC_1_SQRT_2 + 0.5 * std::f32::consts::FRAC_1_SQRT_2) / (1.0 + 2.0 * std::f32::consts::FRAC_1_SQRT_2);
            for processing in [false, true] {
                let got = run(&device, processing);
                assert!((got[0] - want_l).abs() < 1e-5, "processing {processing}: {got:?} vs {want_l}");
                assert!(got[1] > got[0], "the right side is louder here");
            }
            let track = five_one(10);
            let opts = PlayOptions::default();
            let p = plan(&track, &opts, &[RateRange::exact(48_000.0)], "test", false, &device).unwrap();
            let path = signal_path(&track, &opts, &p, 48_000.0, None, OutFormat::F32, true, Transport::Usb);
            assert_eq!(path.downmix.as_ref().map(|d| d.to.as_str()), Some("Stereo"));
            assert!(!path.bit_perfect);
        }
    }

    /// A device asking for more than the scratch holds gets all of it, not silence at the end.
    #[test]
    fn a_callback_bigger_than_the_scratch_is_served_whole() {
        let fs = 48_000u32;
        let samples: Vec<f32> = (0..fs as usize * 4).flat_map(|i| [((i % 100) as f32 + 1.0) / 1000.0; 2]).collect();
        let track = Arc::new(Track::from_samples(fs, 2, Some(24), &samples, crate::decode::TrackMeta::default()));
        let opts = PlayOptions { dsp: DspSettings { crossfeed: None, ..Default::default() }, ..Default::default() };
        let plan = Plan { target: fs as f64, start_frame: 0, lossy: None, resampled: false, processing: true, dop: false, encode: None, route: stereo_route() };
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
