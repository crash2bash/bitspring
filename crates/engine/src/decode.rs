//! Decoding into RAM, progressively.
//!
//! Like Audirvana's "preload in memory" mode, every sample ends up decoded in RAM so the
//! realtime callback only ever reads plain memory — no disk, no network, no codec, no locks.
//! Unlike a plain preload, playback can start as soon as the first second is decoded: a
//! decoder thread keeps appending while the player reads behind it. That is what makes
//! network streams (and very long hi-res files) start instantly.

use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use symphonia::core::audio::sample::Sample;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::{MetadataOptions, MetadataRevision, StandardTag, StandardVisualKey};

use crate::cue::Range;

/// Frames per storage chunk. Chunks never move once allocated, so a reader holding a frame
/// index below `ready` can always find its sample — the buffer grows without reallocating.
const CHUNK_FRAMES: usize = 1 << 16;

/// Longest track we make room for (24 h) — caps what a forged header can make us allocate.
const MAX_TRACK_SECS: usize = 24 * 3600;

/// Highest PCM sample rate we accept (8 × 192 kHz); beyond it a header is lying.
pub(crate) const MAX_PCM_RATE: u32 = 1_536_000;

/// A decoded (or still decoding) track in memory.
///
/// Samples are `f32`, interleaved, normalised to [-1.0, 1.0). For sources up to 24 bits this
/// is lossless: an f32 mantissa holds 24 bits, and symphonia converts `i24 -> f32` by dividing
/// by 2^23 (an exact power-of-two scale), so the float -> int conversion on the way to the DAC
/// reproduces the original integers bit for bit.
///
/// One writer (the decoder) appends; readers read frames below
/// [`ready_frames`](Self::ready_frames). The writer fills a region first and only then
/// publishes it with a release store of `ready`, so readers never see a half-written sample.
///
/// A local file is held in a window (see [`Window`]): the decoder stays a few minutes ahead of
/// the one reader that plays it, and frees what lies well behind. The reader announces where
/// it reads with [`claim`](Self::claim) before every read; only frames at or after the
/// announced position are ever touched, and the writer never frees a chunk at or after it.
pub struct Track {
    pub sample_rate: u32,
    pub channels: usize,
    /// Bit depth of the source as stored in the file (16, 24, ...), when the codec reports it.
    pub bits_per_sample: Option<u32>,
    /// Lossy codec (MP3, AAC, Vorbis, Opus): decoded peaks may exceed full scale.
    pub lossy: bool,
    pub meta: TrackMeta,
    /// Storage, CHUNK_FRAMES frames each: f32 samples (PCM) or 2 bytes per channel (DoP).
    /// Allocated as the decoder gets there, freed (null again) once behind the window.
    chunks: Box<[AtomicPtr<u8>]>,
    /// First frame held (frames below it are freed, or were never decoded: see `origin`).
    base: AtomicUsize,
    /// Where decoding started (0, or the position a file was opened at).
    origin: usize,
    /// Where the reader reads (see `claim`).
    playhead: AtomicUsize,
    window: Option<Window>,
    ready: AtomicUsize,
    /// Frames the container says the track has (0 = unknown); a guess until `complete`.
    expected: AtomicUsize,
    complete: AtomicBool,
    error: Mutex<Option<String>>,
    /// Largest |sample| decoded so far, as f32 bits (lossy decodes can exceed 1.0).
    peak: AtomicU32,
    /// The source is DSD (see [`crate::dsd`]): its bit rate, and whether this track carries the
    /// raw bitstream for DoP or a PCM conversion of it.
    pub dsd: Option<DsdSource>,
    /// Carries raw DSD for DoP: 2 bytes per channel per frame, oldest bit in the MSB.
    dop: bool,
}

/// How much of a local file is kept decoded, in frames.
#[derive(Debug, Clone, Copy)]
struct Window {
    /// The decoder pauses this far ahead of the reader.
    ahead: usize,
    /// What is kept behind the reader (an instant seek back).
    behind: usize,
}

/// Memory for one track's window. Most CD-quality tracks fit whole (about 12 minutes); a
/// 24/192 file keeps about 2 minutes ahead and 40 s behind; DSD256 as DoP about 70 s ahead.
pub const WINDOW_BYTES: usize = 256 << 20;

/// How far beyond what is decoded a seek may land and still be waited for (the decoder runs
/// many times faster than real time); further, the file is opened anew there.
const SEEK_AHEAD_SECS: usize = 10;

/// Where a track's DSD came from and how it is carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DsdSource {
    /// 1-bit samples per second per channel (2 822 400 for DSD64).
    pub rate: u32,
    /// true: the bitstream itself, sent to the DAC packed as DoP. false: converted to PCM.
    pub dop: bool,
}

impl std::fmt::Debug for Track {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Track")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("bits_per_sample", &self.bits_per_sample)
            .field("ready", &self.ready_frames())
            .field("complete", &self.is_complete())
            .finish()
    }
}

impl Drop for Track {
    fn drop(&mut self) {
        for c in self.chunks.iter() {
            // SAFETY: nobody else holds the track any more.
            unsafe { self.free_chunk(c) };
        }
    }
}

/// Tags and cover art, as far as the file has them.
#[derive(Debug, Clone, Default)]
pub struct TrackMeta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub year: Option<String>,
    pub track_number: Option<u64>,
    pub disc_number: Option<u64>,
    /// (MIME type, image bytes) — front cover if tagged as such, else the first picture.
    pub cover: Option<(String, Vec<u8>)>,
}

pub(crate) fn read_meta(format: &mut dyn FormatReader) -> TrackMeta {
    let mut log = format.metadata();
    match log.skip_to_latest() {
        Some(rev) => meta_from_revision(rev),
        None => TrackMeta::default(),
    }
}

pub(crate) fn meta_from_revision(rev: &MetadataRevision) -> TrackMeta {
    let mut m = TrackMeta::default();
    for tag in &rev.media.tags {
        match &tag.std {
            Some(StandardTag::TrackTitle(v)) => m.title.get_or_insert_with(|| v.to_string()),
            Some(StandardTag::Artist(v)) => m.artist.get_or_insert_with(|| v.to_string()),
            Some(StandardTag::Album(v)) => m.album.get_or_insert_with(|| v.to_string()),
            Some(StandardTag::AlbumArtist(v)) => m.album_artist.get_or_insert_with(|| v.to_string()),
            Some(StandardTag::Genre(v)) => m.genre.get_or_insert_with(|| v.to_string()),
            Some(StandardTag::ReleaseDate(v)) | Some(StandardTag::RecordingDate(v)) => {
                m.year.get_or_insert_with(|| v.chars().take(4).collect())
            }
            Some(StandardTag::ReleaseYear(y)) | Some(StandardTag::RecordingYear(y)) => {
                m.year.get_or_insert_with(|| y.to_string())
            }
            Some(StandardTag::TrackNumber(n)) => {
                m.track_number.get_or_insert(*n);
                continue;
            }
            Some(StandardTag::DiscNumber(n)) => {
                m.disc_number.get_or_insert(*n);
                continue;
            }
            _ => continue,
        };
    }
    let visuals = &rev.media.visuals;
    m.cover = visuals
        .iter()
        .find(|v| v.usage == Some(StandardVisualKey::FrontCover))
        .or(visuals.first())
        .map(|v| (v.media_type.clone().unwrap_or_else(|| "image/jpeg".into()), v.data.to_vec()));
    m
}

impl Track {
    pub(crate) fn new(sample_rate: u32, channels: usize, bits: Option<u32>, lossy: bool, meta: TrackMeta, expected: usize) -> Self {
        // Room for 1.5x what the container announces, and at least an hour — the chunk table
        // is only pointers (8 bytes per 64k frames), the samples themselves are allocated lazily.
        // The container's figure is untrusted: never size the table for more than a day of audio.
        let expected = expected.min(sample_rate as usize * MAX_TRACK_SECS);
        let max_frames = (expected + expected / 2).max(sample_rate as usize * 3600);
        let n = max_frames.div_ceil(CHUNK_FRAMES) + 1;
        Self {
            sample_rate,
            channels: channels.max(1),
            bits_per_sample: bits,
            lossy,
            meta,
            chunks: (0..n).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect(),
            base: AtomicUsize::new(0),
            origin: 0,
            playhead: AtomicUsize::new(0),
            window: None,
            ready: AtomicUsize::new(0),
            expected: AtomicUsize::new(expected),
            complete: AtomicBool::new(false),
            error: Mutex::new(None),
            peak: AtomicU32::new(0),
            dsd: None,
            dop: false,
        }
    }

    /// A DoP track: `frames` DoP frames (16 DSD bits per channel each) at `dsd_rate / 16`.
    pub(crate) fn new_dop(dsd_rate: u32, channels: usize, meta: TrackMeta, frames: usize) -> Self {
        let channels = channels.max(1);
        Self {
            sample_rate: dsd_rate / 16,
            channels,
            bits_per_sample: Some(1),
            lossy: false,
            meta,
            chunks: (0..frames.div_ceil(CHUNK_FRAMES) + 1).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect(),
            base: AtomicUsize::new(0),
            origin: 0,
            playhead: AtomicUsize::new(0),
            window: None,
            ready: AtomicUsize::new(0),
            expected: AtomicUsize::new(frames),
            complete: AtomicBool::new(false),
            error: Mutex::new(None),
            peak: AtomicU32::new(0),
            dsd: Some(DsdSource { rate: dsd_rate, dop: true }),
            dop: true,
        }
    }

    /// Decoding starts at `frame` (a file opened at a position): nothing below it is held.
    pub(crate) fn starting_at(mut self, frame: usize) -> Self {
        let frame = frame.min((self.chunks.len().saturating_sub(1)) * CHUNK_FRAMES);
        self.origin = frame;
        *self.base.get_mut() = frame;
        *self.ready.get_mut() = frame;
        *self.playhead.get_mut() = frame;
        self
    }

    /// Keep only a window of about `bytes` decoded (local files; a stream can't be re-read).
    pub(crate) fn windowed(mut self, bytes: usize) -> Self {
        let frames = (bytes / self.frame_bytes()).max(4 * CHUNK_FRAMES);
        self.window = Some(Window { ahead: frames / 4 * 3, behind: frames / 4 });
        self
    }

    fn frame_bytes(&self) -> usize {
        self.channels * if self.dop { 2 } else { 4 }
    }

    /// Allocate chunk `ci` if it isn't yet. Writer only.
    fn chunk_for_writing(&self, ci: usize) -> Option<*mut u8> {
        let slot = self.chunks.get(ci)?;
        let mut p = slot.load(Ordering::Relaxed);
        if p.is_null() {
            p = if self.dop {
                Box::into_raw(vec![0u8; CHUNK_FRAMES * self.channels * 2].into_boxed_slice()) as *mut u8
            } else {
                Box::into_raw(vec![0f32; CHUNK_FRAMES * self.channels].into_boxed_slice()) as *mut u8
            };
            slot.store(p, Ordering::Release);
        }
        Some(p)
    }

    /// # Safety
    /// No reader may touch the chunk any more (it is behind the window, or the track is gone).
    unsafe fn free_chunk(&self, slot: &AtomicPtr<u8>) {
        let p = slot.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if p.is_null() {
            return;
        }
        // SAFETY: allocated in `chunk_for_writing` with exactly this type and length.
        unsafe {
            if self.dop {
                drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(p, CHUNK_FRAMES * self.channels * 2)));
            } else {
                drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(p as *mut f32, CHUNK_FRAMES * self.channels)));
            }
        }
    }

    /// Carries the raw DSD bitstream (play only as DoP, never through processing).
    #[inline]
    pub fn is_dop(&self) -> bool {
        self.dop
    }

    /// Append whole DoP frames (`channels * 2` bytes each). Only the reader thread may call this.
    pub(crate) fn push_dop(&self, mut data: &[u8]) -> bool {
        let fb = self.channels * 2;
        let mut frame = self.ready.load(Ordering::Relaxed);
        while data.len() >= fb {
            let (ci, off) = (frame / CHUNK_FRAMES, frame % CHUNK_FRAMES);
            let Some(p) = self.chunk_for_writing(ci) else { return false };
            let n = (CHUNK_FRAMES - off).min(data.len() / fb);
            // SAFETY: frames >= `ready` belong to the writer until the release store.
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), p.add(off * fb), n * fb) };
            data = &data[n * fb..];
            frame += n;
            self.ready.store(frame, Ordering::Release);
        }
        true
    }

    /// The 16 DSD bits of one channel in one DoP frame, oldest bit in the MSB.
    /// `frame` must be claimed and below `ready_frames()`. Realtime-safe.
    #[inline(always)]
    pub fn dop_payload(&self, frame: usize, channel: usize) -> u16 {
        let p = self.chunks[frame / CHUNK_FRAMES].load(Ordering::Acquire);
        if p.is_null() {
            return crate::dsd::DOP_IDLE;
        }
        let i = ((frame % CHUNK_FRAMES) * self.channels + channel) * 2;
        // SAFETY: a claimed frame below `ready` is written, published and not freed.
        unsafe { u16::from_be_bytes([*p.add(i), *p.add(i + 1)]) }
    }

    /// A finished track from samples already in memory (test signals, tests).
    pub fn from_samples(sample_rate: u32, channels: usize, bits: Option<u32>, samples: &[f32], meta: TrackMeta) -> Self {
        let t = Self::new(sample_rate, channels, bits, false, meta, samples.len() / channels.max(1));
        t.push(samples);
        t.finish(None);
        t
    }

    /// Append interleaved samples. Only the single decoder thread may call this.
    pub(crate) fn push(&self, mut data: &[f32]) -> bool {
        let ch = self.channels;
        let peak = data.iter().fold(0f32, |m, v| m.max(v.abs()));
        self.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
        let mut frame = self.ready.load(Ordering::Relaxed);
        while data.len() >= ch {
            let (ci, off) = (frame / CHUNK_FRAMES, frame % CHUNK_FRAMES);
            let Some(p) = self.chunk_for_writing(ci) else {
                log::warn!("track longer than its sample table, truncating");
                return false;
            };
            let p = p as *mut f32;
            let n = (CHUNK_FRAMES - off).min(data.len() / ch);
            // SAFETY: frames >= `ready` are owned by the writer; readers never touch them
            // until the release store below publishes them.
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), p.add(off * ch), n * ch) };
            data = &data[n * ch..];
            frame += n;
            self.ready.store(frame, Ordering::Release);
        }
        true
    }

    pub(crate) fn finish(&self, error: Option<String>) {
        if let Some(e) = error {
            // Never panics: this also runs while a decoding thread unwinds (FinishOnPanic).
            *self.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
        }
        self.complete.store(true, Ordering::Release);
    }

    /// Frames decoded so far — everything below this index is readable.
    #[inline]
    pub fn ready_frames(&self) -> usize {
        self.ready.load(Ordering::Acquire)
    }

    /// Decoding has ended (successfully or not): `ready_frames` is final.
    #[inline]
    pub fn is_complete(&self) -> bool {
        self.complete.load(Ordering::Acquire)
    }

    /// Largest |sample| decoded so far (1.0 = full scale; above = the decode clips).
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak.load(Ordering::Relaxed))
    }

    /// Why decoding stopped early, if it did.
    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Total length in frames: exact once complete, the container's figure before that.
    pub fn frames(&self) -> usize {
        let ready = self.ready_frames();
        if self.is_complete() { ready } else { ready.max(self.expected.load(Ordering::Relaxed)) }
    }

    pub fn duration_secs(&self) -> f64 {
        self.frames() as f64 / self.sample_rate as f64
    }

    /// One sample. `frame` must be claimed and below `ready_frames()`. Realtime-safe: two
    /// loads, no locks.
    #[inline(always)]
    pub fn sample(&self, frame: usize, channel: usize) -> f32 {
        let p = self.chunks[frame / CHUNK_FRAMES].load(Ordering::Acquire) as *const f32;
        if p.is_null() {
            return 0.0; // never for a claimed frame; a slip costs silence, not memory safety
        }
        // SAFETY: the chunk was published (pointer stored) before `ready` moved past `frame`,
        // and it isn't freed while the frame is claimed.
        unsafe { *p.add((frame % CHUNK_FRAMES) * self.channels + channel) }
    }

    /// The longest contiguous run of decoded frames starting at `start`, capped at `max` frames
    /// (it ends at a chunk boundary or at `ready_frames`). Empty if nothing is ready there yet.
    pub fn slice(&self, start: usize, max: usize) -> &[f32] {
        let ready = self.ready_frames();
        if start >= ready || self.is_dop() {
            return &[];
        }
        let off = start % CHUNK_FRAMES;
        let n = (CHUNK_FRAMES - off).min(ready - start).min(max);
        let p = self.chunks[start / CHUNK_FRAMES].load(Ordering::Acquire) as *const f32;
        if p.is_null() {
            return &[];
        }
        // SAFETY: as in `sample`; the run stays inside one chunk and below `ready`.
        unsafe { std::slice::from_raw_parts(p.add(off * self.channels), n * self.channels) }
    }

    /// Copy every decoded sample out (tests and tools — playback never needs this).
    pub fn to_vec(&self) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.ready_frames() * self.channels);
        let mut f = self.base.load(Ordering::Acquire);
        loop {
            let s = self.slice(f, usize::MAX);
            if s.is_empty() {
                break out;
            }
            f += s.len() / self.channels;
            out.extend_from_slice(s);
        }
    }

    /// The reader is about to read from `frame` on (and only from there on). Returns the frame
    /// it may read from: `frame`, or the start of what is held if `frame` was already freed
    /// (a seek that raced the window moving on). Realtime-safe.
    ///
    /// The reader stores its position and then looks at `base`; the writer raises `base` and
    /// then looks at the position (both SeqCst), so one of them always sees the other: the
    /// writer never frees what the reader is about to read.
    #[inline]
    pub fn claim(&self, frame: usize) -> usize {
        if self.window.is_none() {
            return frame;
        }
        self.playhead.store(frame, Ordering::SeqCst);
        let base = self.base.load(Ordering::SeqCst);
        if frame < base {
            self.playhead.store(base, Ordering::SeqCst);
            return base;
        }
        frame
    }

    /// First frame still held in memory.
    pub fn held_from(&self) -> usize {
        self.base.load(Ordering::Acquire)
    }

    /// Whether playback can move to `frame` in this track as it is (else the file has to be
    /// opened anew there): it is held, or decoding will get there in a moment.
    pub fn can_seek_to(&self, frame: usize) -> bool {
        if self.window.is_none() {
            return true;
        }
        let base = self.base.load(Ordering::Acquire);
        // Just above `base` the window may move past it any moment: keep a chunk's margin.
        let held = frame >= base && (base == self.origin || frame >= base + CHUNK_FRAMES);
        let ready = self.ready_frames();
        held && (frame <= ready + SEEK_AHEAD_SECS * self.sample_rate as usize || self.is_complete() && frame <= ready)
    }

    /// The decoder has decoded as far ahead of the reader as the window allows.
    fn window_full(&self) -> bool {
        self.window.is_some_and(|w| self.ready_frames() >= self.playhead.load(Ordering::Relaxed) + w.ahead)
    }

    /// Writer side, before decoding more: free what is well behind the reader, then wait while
    /// the window ahead is full. False when `alive` says to stop.
    pub(crate) fn make_room(&self, alive: impl Fn() -> bool) -> bool {
        let Some(w) = self.window else { return alive() };
        loop {
            self.free_behind(w);
            if !self.window_full() {
                return alive();
            }
            if !alive() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn free_behind(&self, w: Window) {
        let base = self.base.load(Ordering::Relaxed);
        let want = self.playhead.load(Ordering::SeqCst).saturating_sub(w.behind) / CHUNK_FRAMES * CHUNK_FRAMES;
        if want <= base {
            return;
        }
        self.base.store(want, Ordering::SeqCst);
        // The reader may have moved back before seeing the new base: never free from there on.
        let reader = self.playhead.load(Ordering::SeqCst) / CHUNK_FRAMES * CHUNK_FRAMES;
        let upto = if reader < want {
            let upto = reader.max(base);
            self.base.store(upto, Ordering::SeqCst);
            upto
        } else {
            want
        };
        for slot in &self.chunks[base / CHUNK_FRAMES..upto / CHUNK_FRAMES] {
            // SAFETY: below `base`, which the reader has seen or will see before reading (claim).
            unsafe { self.free_chunk(slot) };
        }
    }

    /// Bytes of decoded audio held now (tests, diagnostics).
    pub fn held_bytes(&self) -> usize {
        let per_chunk = CHUNK_FRAMES * self.frame_bytes();
        self.chunks.iter().filter(|c| !c.load(Ordering::Relaxed).is_null()).count() * per_chunk
    }

    /// Block until `frames` are decoded (or decoding ends, or `timeout` passes).
    pub fn wait_for(&self, frames: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.ready_frames() >= frames || self.is_complete() || self.window_full() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Everything needed to decode, set up before the first sample — so format errors surface
/// to the caller instead of a background thread.
struct Opened {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    track: Track,
    trim: Trim,
}

/// Encoder delay and padding to cut, so an album of AAC files plays gapless: an AAC encoder
/// adds ~2112 frames of silence at the start ("priming") and pads the end to a whole frame.
/// MP3 (LAME/Xing tag) and Vorbis/Opus are trimmed by symphonia itself.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Trim {
    /// Decoded frames to drop at the start.
    pub skip: usize,
    /// Frames to keep after those (None: all).
    pub keep: Option<usize>,
}

impl Trim {
    /// From iTunes' `iTunSMPB` tag (iTunes, Apple Music, XLD, qaac, dBpoweramp):
    /// " 00000000 <delay> <padding> <original length> ..." in hex.
    fn from_itunsmpb(v: &str) -> Option<Self> {
        let f: Vec<u64> = v.split_whitespace().take(4).map(|x| u64::from_str_radix(x, 16)).collect::<Result<_, _>>().ok()?;
        let (&delay, &length) = (f.get(1)?, f.get(3)?);
        Some(Self { skip: delay as usize, keep: (length > 0).then_some(length as usize) })
    }

    /// Plausible for a track at `rate`: a delay under a second.
    fn sane(self, rate: u32) -> Option<Self> {
        (self.skip < rate as usize && self.keep != Some(0)).then_some(self)
    }
}

/// An MP4 audio track's single edit (ffmpeg, Nero): where the music starts, in the track's
/// time scale, and how long it lasts, in the movie's.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mp4Edit {
    start: u64,
    media_scale: u64,
    duration: u64,
    movie_scale: u64,
}

impl Mp4Edit {
    fn at(self, rate: u32) -> Option<Trim> {
        let skip = (self.start as u128 * rate as u128 / self.media_scale as u128) as usize;
        let keep = (self.duration as u128 * rate as u128 / self.movie_scale as u128) as usize;
        Trim { skip, keep: Some(keep) }.sane(rate)
    }
}

/// The edit list of an MP4 file's audio track, if it has exactly one edit into the media.
fn mp4_edit(path: &Path) -> Option<Mp4Edit> {
    use std::io::{Read, Seek, SeekFrom};
    let be32 = |b: &[u8], i: usize| b.get(i..i + 4).map(|x| u32::from_be_bytes(x.try_into().unwrap()));
    let be64 = |b: &[u8], i: usize| b.get(i..i + 8).map(|x| u64::from_be_bytes(x.try_into().unwrap()));
    // Child atoms of `b`: (type, body).
    fn atoms(b: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
        let mut at = 0;
        std::iter::from_fn(move || {
            let size = u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize;
            let kind = b.get(at + 4..at + 8)?;
            let (head, size) = match size {
                1 => (16, u64::from_be_bytes(b.get(at + 8..at + 16)?.try_into().ok()?) as usize),
                0 => (8, b.len() - at),
                n => (8, n),
            };
            let body = b.get(at + head..at.checked_add(size)?)?;
            at += size;
            Some((kind, body))
        })
    }
    fn child<'a>(b: &'a [u8], kind: &[u8]) -> Option<&'a [u8]> {
        atoms(b).find(|(k, _)| *k == kind).map(|(_, v)| v)
    }
    // Find `moov` among the top-level atoms (it may sit after the audio data).
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut pos = 0u64;
    // Every read is checked: this runs on whatever file the user adds, damaged or forged.
    let moov = loop {
        if pos.checked_add(8)? > len {
            return None;
        }
        f.seek(SeekFrom::Start(pos)).ok()?;
        let mut h = [0u8; 16];
        f.read_exact(&mut h[..8]).ok()?;
        let (mut size, head) = (u32::from_be_bytes(h[..4].try_into().unwrap()) as u64, 8u64);
        let head = if size == 1 {
            f.read_exact(&mut h[8..]).ok()?;
            size = u64::from_be_bytes(h[8..].try_into().unwrap());
            16
        } else {
            head
        };
        if size == 0 {
            size = len - pos;
        }
        // An atom must at least hold its header and end inside the file, so each step moves on.
        if size < head || pos.checked_add(size)? > len {
            return None;
        }
        if &h[4..8] == b"moov" {
            if size > 64 << 20 {
                return None;
            }
            let mut b = vec![0u8; (size - head) as usize];
            f.read_exact(&mut b).ok()?;
            break b;
        }
        pos += size;
    };
    let mvhd = child(&moov, b"mvhd")?;
    let movie_scale = be32(mvhd, if *mvhd.first()? == 1 { 20 } else { 12 })? as u64;
    for (kind, trak) in atoms(&moov) {
        if kind != b"trak" {
            continue;
        }
        let mdia = child(trak, b"mdia")?;
        if child(mdia, b"hdlr").and_then(|h| h.get(8..12)) != Some(b"soun") {
            continue;
        }
        let mdhd = child(mdia, b"mdhd")?;
        let media_scale = be32(mdhd, if *mdhd.first()? == 1 { 20 } else { 12 })? as u64;
        let elst = child(child(trak, b"edts")?, b"elst")?;
        let v1 = *elst.first()? == 1;
        // Real files have one or two edits; a forged count must not make us allocate.
        let n = (be32(elst, 4)? as usize).min(elst.len() / 12);
        let step = if v1 { 20 } else { 12 };
        let edits: Vec<(u64, i64)> = (0..n)
            .map(|i| {
                let at = 8 + i * step;
                Some(if v1 {
                    (be64(elst, at)?, be64(elst, at + 8)? as i64)
                } else {
                    (be32(elst, at)? as u64, be32(elst, at + 4)? as i32 as i64)
                })
            })
            .collect::<Option<_>>()?;
        // One edit into the media (empty edits, media time −1, would add silence: leave
        // those files alone).
        let [(duration, start)] = edits[..] else { return None };
        if start < 0 || movie_scale == 0 || media_scale == 0 {
            return None;
        }
        return Some(Mp4Edit { start: start as u64, media_scale, duration, movie_scale });
    }
    None
}

fn open(source: Box<dyn MediaSource>, hint: &Hint, expected_frames: Option<usize>, edit: Option<Mp4Edit>, range: Option<Range>) -> Result<Opened> {
    let mss = MediaSourceStream::new(source, Default::default());
    let mut format = symphonia::default::get_probe()
        .probe(hint, mss, FormatOptions::default(), MetadataOptions::default())
        .context("unsupported or unrecognised format")?;

    let smpb = format.metadata().skip_to_latest().and_then(|rev| {
        rev.media.tags.iter().find(|t| t.raw.key.ends_with("iTunSMPB")).and_then(|t| match &t.raw.value {
            symphonia::core::meta::RawValue::String(s) => Trim::from_itunsmpb(s),
            _ => None,
        })
    });
    let meta = read_meta(&mut *format);
    let t = format.default_track(TrackType::Audio).ok_or_else(|| anyhow!("no audio track"))?;
    let track_id = t.id;
    let num_frames = t.num_frames.map(|n| n as usize);
    let params = t
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| anyhow!("track has no audio codec parameters"))?
        .clone();
    let decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .context("no decoder for this codec")?;

    let sample_rate = params.sample_rate.ok_or_else(|| anyhow!("unknown sample rate"))?;
    if sample_rate == 0 || sample_rate > MAX_PCM_RATE {
        bail!("implausible sample rate {sample_rate} Hz");
    }
    let channels = params.channels.as_ref().map(|c| c.count()).ok_or_else(|| anyhow!("unknown channel count"))?;
    if channels == 0 || channels > 32 {
        bail!("implausible channel count {channels}");
    }
    let lossy = crate::library::codec_is_lossy(params.codec);
    // AAC carries its delay in a tag or the edit list; symphonia doesn't apply either.
    let trim = if params.codec == symphonia::core::codecs::audio::well_known::CODEC_ID_AAC && t_delay_unknown(&*format, track_id) {
        smpb.and_then(|t| t.sane(sample_rate)).or_else(|| edit.and_then(|e| e.at(sample_rate))).unwrap_or_default()
    } else {
        Trim::default()
    };
    let trim = match range {
        Some(r) => ranged(trim, r, sample_rate, num_frames),
        None => trim,
    };
    let expected = trim.keep.or(num_frames).or(expected_frames).unwrap_or(0);
    let track = Track::new(sample_rate, channels, params.bits_per_sample, lossy, meta, expected);
    Ok(Opened { format, decoder, track_id, track, trim })
}

/// One CUE track of an image file: its part of the file, on top of what the codec trims.
fn ranged(trim: Trim, range: Range, rate: u32, total: Option<usize>) -> Trim {
    let start = range.start_at(rate);
    let keep = range.len_at(rate).or_else(|| trim.keep.or(total.map(|t| t.saturating_sub(trim.skip))).map(|k| k.saturating_sub(start)));
    Trim { skip: trim.skip + start, keep }
}

/// The demuxer stated no delay for this track (else the decoder trims it already).
fn t_delay_unknown(format: &dyn FormatReader, track_id: u32) -> bool {
    format.tracks().iter().find(|t| t.id == track_id).is_none_or(|t| t.delay.is_none() && t.padding.is_none())
}

/// Ends `track` with an error if the decoding thread panics (a decoder bug on a malformed
/// file). Without it the track never completes: playback reaches the last decoded frame and
/// waits in "buffering" forever, and the queue never moves on.
pub(crate) struct FinishOnPanic<'a>(pub(crate) &'a Track);

impl Drop for FinishOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.finish(Some("decode: the decoder crashed on this file".into()));
        }
    }
}

/// Move the demuxer to `secs` (in the trimmed timeline). Returns the frame decoding resumes
/// at and the trim that now applies (the frames before it to drop, and what is left to keep).
/// None when the format can't seek: then decoding starts from the beginning.
///
/// A lossy decoder needs the frame before to get its first output right (MDCT overlap), so it
/// starts a little earlier and that pre-roll is dropped.
fn seek_decoder(format: &mut dyn FormatReader, decoder: &mut dyn AudioDecoder, track_id: u32, rate: u32, lossy: bool, trim: Trim, secs: f64) -> Option<(usize, Trim)> {
    use symphonia::core::formats::{SeekMode, SeekTo};
    use symphonia::core::units::Time;
    const PRE_ROLL: usize = 4096;
    let start = (secs * rate as f64) as usize;
    // Track timestamps count the encoder delay too (when we trim it ourselves).
    let wanted = start + trim.skip;
    let from = wanted.saturating_sub(if lossy { PRE_ROLL } else { 0 });
    let time = Time::try_from_secs_f64(from as f64 / rate as f64)?;
    let tb = format.tracks().iter().find(|t| t.id == track_id).and_then(|t| t.time_base);
    let to_frames = |ts: i64| -> i64 {
        match tb {
            Some(tb) => (ts as i128 * tb.numer.get() as i128 * rate as i128 / tb.denom.get() as i128) as i64,
            None => ts,
        }
    };
    let seeked = match format.seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(track_id) }) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("can't seek this file, decoding from the start: {e}");
            return None;
        }
    };
    decoder.reset();
    let actual = to_frames(seeked.actual_ts.get()).max(0) as usize;
    if actual > wanted {
        // Landed past the point (shouldn't): start where it landed.
        let start = actual.saturating_sub(trim.skip);
        return Some((start, Trim { skip: 0, keep: trim.keep.map(|k| k.saturating_sub(start)) }));
    }
    Some((start, Trim { skip: wanted - actual, keep: trim.keep.map(|k| k.saturating_sub(start)) }))
}

/// Decode packets into `track` until the end, an error, or `alive()` says stop.
fn run(format: &mut dyn FormatReader, decoder: &mut dyn AudioDecoder, track_id: u32, track: &Track, trim: Trim, alive: impl Fn() -> bool) {
    let mut scratch: Vec<f32> = Vec::new();
    let ch = track.channels;
    let (mut skip, mut left) = (trim.skip, trim.keep.unwrap_or(usize::MAX));
    let error = loop {
        // Local files: wait while the window ahead is full (and free what's well behind).
        if !track.make_room(&alive) {
            break None;
        }
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break None,
            // A truncated tail shouldn't throw away everything decoded so far.
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break None,
            Err(e) => break Some(format!("demux: {e}")),
        };
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                scratch.resize(buf.samples_interleaved(), f32::MID);
                buf.copy_to_slice_interleaved(&mut scratch);
                let mut data = &scratch[..];
                let s = skip.min(data.len() / ch);
                skip -= s;
                data = &data[s * ch..];
                let k = (data.len() / ch).min(left);
                left -= k;
                if !data.is_empty() && !track.push(&data[..k * ch]) {
                    break None;
                }
                if left == 0 {
                    break None; // the rest is the encoder's padding
                }
            }
            // A single corrupt frame: skip it, as every player does.
            Err(SymError::DecodeError(msg)) => log::warn!("decode error, skipping packet: {msg}"),
            Err(e) => break Some(format!("decode: {e}")),
        }
    };
    if let Some(e) = &error {
        log::warn!("{e}");
    }
    track.finish(error);
}

/// Open any byte source and start decoding it in the background. Returns as soon as the
/// headers are parsed; samples keep arriving in the returned track. The decoder stops by
/// itself once every `Arc` to the track is dropped (switching tracks cancels a download).
///
/// `expected_secs` sizes the buffer when the container doesn't state its length (streams).
pub fn open_source(source: Box<dyn MediaSource>, hint: Hint, expected_secs: Option<f64>) -> Result<Arc<Track>> {
    open_stream(source, hint, expected_secs, TrackMeta::default())
}

/// [`open_source`] for a stream whose tags come from elsewhere (a streaming service's
/// catalogue): fields set in `meta` win over what the stream itself carries.
pub fn open_stream(source: Box<dyn MediaSource>, hint: Hint, expected_secs: Option<f64>, meta: TrackMeta) -> Result<Arc<Track>> {
    open_stream_trimmed(source, hint, expected_secs, meta, Local::No)
}

/// How a source is read: a stream once and whole; a local file in a window, from any point.
#[derive(Clone)]
enum Local {
    No,
    /// `path`: the file, for a cover.jpg-style picture next to it when it has none inside.
    File { path: std::path::PathBuf, edit: Option<Mp4Edit>, start_secs: f64, range: Option<Range> },
}

fn open_stream_trimmed(source: Box<dyn MediaSource>, hint: Hint, expected_secs: Option<f64>, meta: TrackMeta, local: Local) -> Result<Arc<Track>> {
    let (edit, range, path) = match &local {
        Local::File { edit, range, path, .. } => (*edit, *range, Some(path.clone())),
        Local::No => (None, None, None),
    };
    let Opened { mut format, mut decoder, track_id, mut track, mut trim } = open(source, &hint, None, edit, range)?;
    if let Local::File { start_secs, .. } = local {
        // Seek to the start: one asked for, or a CUE track's place in its image.
        if (start_secs > 0.0 || trim.skip >= track.sample_rate as usize)
            && let Some((start, rest)) = seek_decoder(&mut *format, &mut *decoder, track_id, track.sample_rate, track.lossy, trim, start_secs)
        {
            trim = rest;
            track = track.starting_at(start);
        }
        track = track.windowed(WINDOW_BYTES);
    }
    let own = std::mem::take(&mut track.meta);
    track.meta = TrackMeta {
        title: meta.title.or(own.title),
        artist: meta.artist.or(own.artist),
        album: meta.album.or(own.album),
        album_artist: meta.album_artist.or(own.album_artist),
        genre: meta.genre.or(own.genre),
        year: meta.year.or(own.year),
        track_number: meta.track_number.or(own.track_number),
        disc_number: meta.disc_number.or(own.disc_number),
        cover: meta.cover.or(own.cover).or_else(|| path.as_deref().and_then(folder_cover)),
    };
    if track.expected.load(Ordering::Relaxed) == 0 {
        if let Some(s) = expected_secs {
            track.expected.store((s * track.sample_rate as f64) as usize, Ordering::Relaxed);
        }
    }
    let track = Arc::new(track);
    let mine = track.clone();
    std::thread::Builder::new().name("hifi-decode".into()).spawn(move || {
        // Once ours is the only reference left, nobody will play this track: stop decoding
        // (and stop downloading, for a stream).
        let alive = || Arc::strong_count(&mine) > 1;
        let _guard = FinishOnPanic(&mine);
        run(&mut *format, &mut *decoder, track_id, &mine, trim, alive);
    })?;
    Ok(track)
}

/// The picture rippers leave next to the tracks (cover.jpg and the like), as cover art: for
/// files without art inside, so network players and lock screens have something to show.
pub(crate) fn folder_cover(track_path: &Path) -> Option<(String, Vec<u8>)> {
    let f = crate::library::folder_image(track_path)?;
    let bytes = std::fs::read(&f).ok().filter(|b| b.len() <= 16 << 20)?;
    let png = f.extension().is_some_and(|e| e.eq_ignore_ascii_case("png"));
    Some((if png { "image/png" } else { "image/jpeg" }.to_string(), bytes))
}

/// Open a file and decode it progressively (see [`open_source`]).
pub fn open_track(path: &Path) -> Result<Arc<Track>> {
    open_track_at(path, 0.0)
}

/// [`open_track`], decoding from `start_secs` on (a seek far from what is held).
pub fn open_track_at(path: &Path, start_secs: f64) -> Result<Arc<Track>> {
    open_range_at(path, None, start_secs, TrackMeta::default())
}

/// [`open_track_at`] for part of a file (a CUE track of an image): `start_secs` counts from
/// the start of that part, and decoding stops at its end. Tags set in `meta` (the sheet's)
/// win over the file's own.
pub fn open_range_at(path: &Path, range: Option<Range>, start_secs: f64, meta: TrackMeta) -> Result<Arc<Track>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    open_stream_trimmed(Box::new(file), hint_for(path), None, meta, Local::File { path: path.to_path_buf(), edit: file_edit(path), start_secs, range })
}

/// The edit list of a local MP4/M4A file (looked for only in those).
fn file_edit(path: &Path) -> Option<Mp4Edit> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if !["m4a", "mp4", "m4b"].contains(&ext.as_str()) {
        return None;
    }
    mp4_edit(path)
}

fn hint_for(path: &Path) -> Hint {
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    hint
}

/// Decode a whole file before returning (CLI tools, tests).
pub fn load_track(path: &Path) -> Result<Track> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let Opened { mut format, mut decoder, track_id, track, trim } = open(Box::new(file), &hint_for(path), None, file_edit(path), None)?;
    run(&mut *format, &mut *decoder, track_id, &track, trim, || true);
    match track.error() {
        Some(e) if track.ready_frames() == 0 => Err(anyhow!(e)),
        _ => Ok(track),
    }
}

/// A local file decoded in a window of `bytes` (tests elsewhere in the crate).
#[cfg(test)]
pub(crate) fn open_windowed(path: &Path, bytes: usize) -> Arc<Track> {
    let file = File::open(path).unwrap();
    let Opened { mut format, mut decoder, track_id, track, trim } = open(Box::new(file), &hint_for(path), None, None, None).unwrap();
    let t = Arc::new(track.windowed(bytes));
    let mine = t.clone();
    std::thread::spawn(move || run(&mut *format, &mut *decoder, track_id, &mine, trim, || Arc::strong_count(&mine) > 1));
    t
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A 16-bit stereo WAV whose every frame is distinct: left = frame % 30000, right = −left.
    pub(crate) fn ramp_wav(path: &Path, rate: u32, frames: usize) {
        let data: Vec<u8> = (0..frames).flat_map(|i| {
            let v = (i % 30_000) as i16;
            [v.to_le_bytes(), (-v).to_le_bytes()].concat()
        }).collect();
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&2u16.to_le_bytes());
        w.extend_from_slice(&rate.to_le_bytes());
        w.extend_from_slice(&(rate * 4).to_le_bytes());
        w.extend_from_slice(&4u16.to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data.len() as u32).to_le_bytes());
        w.extend_from_slice(&data);
        std::fs::write(path, w).unwrap();
    }

    fn value(frame: usize) -> f32 {
        (frame % 30_000) as f32 / 32_768.0
    }

    #[test]
    fn a_file_opened_in_the_middle_starts_exactly_there() {
        let path = std::env::temp_dir().join(format!("hifi-mid-{}.wav", std::process::id()));
        ramp_wav(&path, 44_100, 44_100 * 3);
        let t = open_track_at(&path, 1.5).unwrap();
        assert!(t.wait_for(usize::MAX, Duration::from_secs(10)));
        let at = 66_150;
        assert_eq!(t.held_from(), at);
        assert_eq!(t.frames(), 44_100 * 3);
        for f in [at, at + 1, at + 12_345, t.frames() - 1] {
            assert_eq!(t.sample(f, 0), value(f), "frame {f}");
            assert_eq!(t.sample(f, 1), -value(f));
        }
        // Before the start nothing is held: a seek there needs the file opened anew.
        assert!(!t.can_seek_to(at - 10_000));
        assert!(t.can_seek_to(at + 1_000));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_cue_track_is_exactly_its_part_of_the_image() {
        let path = std::env::temp_dir().join(format!("hifi-cue-{}.wav", std::process::id()));
        let rate = 44_100;
        ramp_wav(&path, rate, rate as usize * 5);
        // Seconds 2 to 3.
        let r = Range { start: 150, end: Some(225) };
        let t = open_range_at(&path, Some(r), 0.0, TrackMeta::default()).unwrap();
        assert!(t.wait_for(usize::MAX, Duration::from_secs(10)));
        assert_eq!(t.frames(), 44_100);
        assert!(t.is_complete());
        for f in [0, 1, 20_000, 44_099] {
            assert_eq!(t.sample(f, 0), value(2 * 44_100 + f), "frame {f}");
        }
        // Started half a second in (a resume, or a seek far ahead).
        let t = open_range_at(&path, Some(r), 0.5, TrackMeta::default()).unwrap();
        assert!(t.wait_for(usize::MAX, Duration::from_secs(10)));
        assert_eq!((t.held_from(), t.frames()), (22_050, 44_100));
        assert_eq!(t.sample(22_050, 0), value(2 * 44_100 + 22_050));
        // The last track: from its start to the end of the file.
        let t = open_range_at(&path, Some(Range { start: 300, end: None }), 0.0, TrackMeta::default()).unwrap();
        assert!(t.wait_for(usize::MAX, Duration::from_secs(10)));
        assert_eq!(t.frames(), 44_100);
        assert_eq!(t.sample(0, 0), value(4 * 44_100));
        let _ = std::fs::remove_file(&path);
    }

    /// The decoder stays a window ahead of the reader and frees what is well behind it, so a
    /// long file never sits whole in memory — and every frame the reader reads is right.
    #[test]
    fn a_long_file_is_held_in_a_window() {
        let path = std::env::temp_dir().join(format!("hifi-window-{}.wav", std::process::id()));
        let rate = 48_000;
        ramp_wav(&path, rate, rate as usize * 120); // 2 minutes = 46 MB as f32
        let file = File::open(&path).unwrap();
        // A small window for the test: 8 MB (≈ 22 s of 48 kHz stereo).
        let Opened { mut format, mut decoder, track_id, track, trim } = open(Box::new(file), &hint_for(&path), None, None, None).unwrap();
        let t = Arc::new(track.windowed(8 << 20));
        let mine = t.clone();
        let decoding = std::thread::spawn(move || run(&mut *format, &mut *decoder, track_id, &mine, trim, || true));
        let mut most = 0;
        let mut frame = 0;
        let step = 4_800; // 0.1 s
        while frame + step < rate as usize * 120 {
            let at = t.claim(frame);
            assert_eq!(at, frame);
            while t.ready_frames() < frame + step {
                std::thread::sleep(Duration::from_millis(1));
            }
            for f in [frame, frame + step - 1] {
                assert_eq!(t.sample(f, 0), value(f), "frame {f}");
            }
            most = most.max(t.held_bytes());
            frame += step * 5; // play faster than real time
        }
        decoding.join().unwrap();
        assert!(most <= (8 << 20) + 3 * CHUNK_FRAMES * 8, "held {} MB", most >> 20);
        assert!(t.held_from() > 0, "old audio freed");
        // Far behind: gone, so a seek there reopens the file; near the reader: fine.
        assert!(!t.can_seek_to(0));
        assert!(t.can_seek_to(frame - 1_000));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn itunes_gapless_tag_gives_delay_and_length() {
        let t = Trim::from_itunsmpb(" 00000000 00000840 0000026C 0000000000035D54 00000000 00000000").unwrap();
        assert_eq!(t, Trim { skip: 2112, keep: Some(220_500) });
        assert_eq!(Trim::from_itunsmpb("garbage"), None);
        // A length of 0 (unknown): only the start is cut.
        assert_eq!(Trim::from_itunsmpb(" 0 840 0 0").unwrap().keep, None);
    }

    /// An MP4 file as ffmpeg writes it, reduced to the atoms the edit list lives in.
    #[test]
    fn mp4_edit_list_is_found_after_the_audio_data() {
        fn atom(kind: &[u8], body: &[u8]) -> Vec<u8> {
            let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            v.extend_from_slice(kind);
            v.extend_from_slice(body);
            v
        }
        let mut mvhd = vec![0u8; 100];
        mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes()); // movie: ms
        let mut mdhd = vec![0u8; 24];
        mdhd[12..16].copy_from_slice(&44_100u32.to_be_bytes());
        let mut hdlr = vec![0u8; 24];
        hdlr[8..12].copy_from_slice(b"soun");
        let mut elst = vec![0u8, 0, 0, 0];
        elst.extend_from_slice(&1u32.to_be_bytes());
        elst.extend_from_slice(&5000u32.to_be_bytes()); // 5 s
        elst.extend_from_slice(&1024i32.to_be_bytes()); // music starts at sample 1024
        elst.extend_from_slice(&[0, 1, 0, 0]);
        let mdia = atom(b"mdia", &[atom(b"mdhd", &mdhd), atom(b"hdlr", &hdlr)].concat());
        let trak = atom(b"trak", &[atom(b"tkhd", &[0; 84]), atom(b"edts", &atom(b"elst", &elst)), mdia].concat());
        let moov = atom(b"moov", &[atom(b"mvhd", &mvhd), trak].concat());
        let file = [atom(b"ftyp", b"M4A \0\0\0\0"), atom(b"mdat", &[0; 5000]), moov].concat();
        let path = std::env::temp_dir().join(format!("hifi-elst-{}.m4a", std::process::id()));
        std::fs::write(&path, file).unwrap();
        let edit = file_edit(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(edit.at(44_100), Some(Trim { skip: 1024, keep: Some(220_500) }));
    }

    /// Damaged or forged headers end the search instead of panicking or looping.
    #[test]
    fn hostile_mp4_headers_are_refused() {
        let path = std::env::temp_dir().join(format!("hifi-bad-{}.m4a", std::process::id()));
        let huge = [&1u32.to_be_bytes()[..], b"free", &u64::MAX.to_be_bytes()].concat();
        let tiny = [&3u32.to_be_bytes()[..], b"free"].concat();
        let empty_mvhd = [&16u32.to_be_bytes()[..], b"moov", &8u32.to_be_bytes(), b"mvhd"].concat();
        for bytes in [huge, tiny, empty_mvhd, vec![0; 3]] {
            std::fs::write(&path, &bytes).unwrap();
            assert!(file_edit(&path).is_none());
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn samples_survive_chunk_boundaries() {
        let n = CHUNK_FRAMES * 2 + 123;
        let data: Vec<f32> = (0..n * 2).map(|i| i as f32).collect();
        let t = Track::new(48_000, 2, Some(24), false, TrackMeta::default(), 0);
        // Odd-sized pushes, like decoder packets.
        for part in data.chunks(2 * 1151) {
            t.push(part);
        }
        t.finish(None);
        assert_eq!(t.frames(), n);
        assert_eq!(t.sample(CHUNK_FRAMES - 1, 1), ((CHUNK_FRAMES - 1) * 2 + 1) as f32);
        assert_eq!(t.sample(CHUNK_FRAMES, 0), (CHUNK_FRAMES * 2) as f32);
        // A slice never crosses a chunk.
        assert_eq!(t.slice(CHUNK_FRAMES - 10, 100).len(), 10 * 2);
        assert_eq!(t.to_vec(), data);
    }

    #[test]
    fn a_crashing_decoder_still_ends_the_track() {
        let t = Arc::new(Track::new(48_000, 2, Some(24), false, TrackMeta::default(), 0));
        let mine = t.clone();
        let joined = std::thread::spawn(move || {
            let _guard = FinishOnPanic(&mine);
            mine.push(&[0.0; 64]);
            panic!("decoder bug");
        })
        .join();
        assert!(joined.is_err());
        assert!(t.is_complete());
        assert_eq!(t.frames(), 32);
        assert!(t.error().is_some_and(|e| e.contains("crashed")));
    }

    #[test]
    fn progressive_decode_matches_full_decode() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/library");
        let Some(file) = std::fs::read_dir(&dir).ok().and_then(|mut d| {
            d.find_map(|e| {
                let p = e.ok()?.path();
                (p.is_dir()).then(|| std::fs::read_dir(&p).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "flac")))?
            })
        }) else {
            eprintln!("no test FLAC in {}, skipping", dir.display());
            return;
        };
        let full = load_track(&file).unwrap();
        let prog = open_track(&file).unwrap();
        assert!(prog.wait_for(usize::MAX, Duration::from_secs(20)));
        assert!(prog.is_complete());
        assert_eq!(prog.frames(), full.frames());
        assert!(prog.to_vec() == full.to_vec(), "samples differ");
    }
}
