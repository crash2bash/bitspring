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
/// One writer (the decoder) appends; any number of readers read frames below
/// [`ready_frames`](Self::ready_frames). The writer fills a region first and only then
/// publishes it with a release store of `ready`, so readers never see a half-written sample.
pub struct Track {
    pub sample_rate: u32,
    pub channels: usize,
    /// Bit depth of the source as stored in the file (16, 24, ...), when the codec reports it.
    pub bits_per_sample: Option<u32>,
    /// Lossy codec (MP3, AAC, Vorbis, Opus): decoded peaks may exceed full scale.
    pub lossy: bool,
    pub meta: TrackMeta,
    chunks: Box<[AtomicPtr<f32>]>,
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
    /// DoP tracks only: raw DSD, 2 bytes per channel per frame, oldest bit in the MSB.
    /// Allocated once at full size (pages are only touched as the reader fills them).
    dop: AtomicPtr<u8>,
    dop_len: usize,
}

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
        let len = CHUNK_FRAMES * self.channels;
        for c in self.chunks.iter() {
            let p = c.load(Ordering::Relaxed);
            if !p.is_null() {
                // SAFETY: allocated in `push` as a boxed slice of exactly `len` floats.
                drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(p, len)) });
            }
        }
        let p = self.dop.load(Ordering::Relaxed);
        if !p.is_null() {
            // SAFETY: allocated in `new_dop` as a boxed slice of exactly `dop_len` bytes.
            drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(p, self.dop_len)) });
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
            ready: AtomicUsize::new(0),
            expected: AtomicUsize::new(expected),
            complete: AtomicBool::new(false),
            error: Mutex::new(None),
            peak: AtomicU32::new(0),
            dsd: None,
            dop: AtomicPtr::new(std::ptr::null_mut()),
            dop_len: 0,
        }
    }

    /// A DoP track: `frames` DoP frames (16 DSD bits per channel each) at `dsd_rate / 16`.
    pub(crate) fn new_dop(dsd_rate: u32, channels: usize, meta: TrackMeta, frames: usize) -> Self {
        let channels = channels.max(1);
        let dop_len = frames * channels * 2;
        // Zeroed, so the pages are only committed as the reader fills them.
        let buf = Box::into_raw(vec![0u8; dop_len].into_boxed_slice()) as *mut u8;
        Self {
            sample_rate: dsd_rate / 16,
            channels,
            bits_per_sample: Some(1),
            lossy: false,
            meta,
            chunks: Box::new([]),
            ready: AtomicUsize::new(0),
            expected: AtomicUsize::new(frames),
            complete: AtomicBool::new(false),
            error: Mutex::new(None),
            peak: AtomicU32::new(0),
            dsd: Some(DsdSource { rate: dsd_rate, dop: true }),
            dop: AtomicPtr::new(buf),
            dop_len,
        }
    }

    /// Carries the raw DSD bitstream (play only as DoP, never through processing).
    #[inline]
    pub fn is_dop(&self) -> bool {
        self.dop_len > 0
    }

    /// Append whole DoP frames (`channels * 2` bytes each). Only the reader thread may call this.
    pub(crate) fn push_dop(&self, data: &[u8]) -> bool {
        let frame_bytes = self.channels * 2;
        let ready = self.ready.load(Ordering::Relaxed);
        let start = ready * frame_bytes;
        let n = (data.len() / frame_bytes).min((self.dop_len - start) / frame_bytes);
        // SAFETY: bytes at and past `ready` belong to the writer until the release store.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.dop.load(Ordering::Relaxed).add(start), n * frame_bytes) };
        self.ready.store(ready + n, Ordering::Release);
        n * frame_bytes == data.len() - data.len() % frame_bytes
    }

    /// The 16 DSD bits of one channel in one DoP frame, oldest bit in the MSB.
    /// `frame` must be below `ready_frames()`. Realtime-safe.
    #[inline(always)]
    pub fn dop_payload(&self, frame: usize, channel: usize) -> u16 {
        let i = (frame * self.channels + channel) * 2;
        debug_assert!(i + 1 < self.dop_len);
        // SAFETY: below `ready` (acquired by the caller), so written and published.
        unsafe {
            let p = self.dop.load(Ordering::Relaxed).add(i);
            u16::from_be_bytes([*p, *p.add(1)])
        }
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
            let Some(slot) = self.chunks.get(ci) else {
                log::warn!("track longer than its sample table, truncating");
                return false;
            };
            let mut p = slot.load(Ordering::Relaxed);
            if p.is_null() {
                p = Box::into_raw(vec![0f32; CHUNK_FRAMES * ch].into_boxed_slice()) as *mut f32;
                slot.store(p, Ordering::Relaxed);
            }
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

    /// One sample. `frame` must be below `ready_frames()`. Realtime-safe: two loads, no locks.
    #[inline(always)]
    pub fn sample(&self, frame: usize, channel: usize) -> f32 {
        let p = self.chunks[frame / CHUNK_FRAMES].load(Ordering::Relaxed);
        // SAFETY: the chunk was published (pointer stored) before `ready` moved past `frame`,
        // and the caller read `ready` with acquire ordering.
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
        let p = self.chunks[start / CHUNK_FRAMES].load(Ordering::Relaxed);
        // SAFETY: as in `sample`; the run stays inside one chunk and below `ready`.
        unsafe { std::slice::from_raw_parts(p.add(off * self.channels), n * self.channels) }
    }

    /// Copy every decoded sample out (tests and tools — playback never needs this).
    pub fn to_vec(&self) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.ready_frames() * self.channels);
        let mut f = 0;
        loop {
            let s = self.slice(f, usize::MAX);
            if s.is_empty() {
                break out;
            }
            f += s.len() / self.channels;
            out.extend_from_slice(s);
        }
    }

    /// Block until `frames` are decoded (or decoding ends, or `timeout` passes).
    pub fn wait_for(&self, frames: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.ready_frames() >= frames || self.is_complete() {
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

fn open(source: Box<dyn MediaSource>, hint: &Hint, expected_frames: Option<usize>, edit: Option<Mp4Edit>) -> Result<Opened> {
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
    let expected = trim.keep.or(num_frames).or(expected_frames).unwrap_or(0);
    let track = Track::new(sample_rate, channels, params.bits_per_sample, lossy, meta, expected);
    Ok(Opened { format, decoder, track_id, track, trim })
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

/// Decode packets into `track` until the end, an error, or `alive()` says stop.
fn run(format: &mut dyn FormatReader, decoder: &mut dyn AudioDecoder, track_id: u32, track: &Track, trim: Trim, alive: impl Fn() -> bool) {
    let mut scratch: Vec<f32> = Vec::new();
    let ch = track.channels;
    let (mut skip, mut left) = (trim.skip, trim.keep.unwrap_or(usize::MAX));
    let error = loop {
        if !alive() {
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
    open_stream_trimmed(source, hint, expected_secs, meta, None)
}

fn open_stream_trimmed(source: Box<dyn MediaSource>, hint: Hint, expected_secs: Option<f64>, meta: TrackMeta, edit: Option<Mp4Edit>) -> Result<Arc<Track>> {
    let Opened { mut format, mut decoder, track_id, mut track, trim } = open(source, &hint, None, edit)?;
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
        cover: meta.cover.or(own.cover),
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

/// Open a file and decode it progressively (see [`open_source`]).
pub fn open_track(path: &Path) -> Result<Arc<Track>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    open_stream_trimmed(Box::new(file), hint_for(path), None, TrackMeta::default(), file_edit(path))
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
    let Opened { mut format, mut decoder, track_id, track, trim } = open(Box::new(file), &hint_for(path), None, file_edit(path))?;
    run(&mut *format, &mut *decoder, track_id, &track, trim, || true);
    match track.error() {
        Some(e) if track.ready_frames() == 0 => Err(anyhow!(e)),
        _ => Ok(track),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

