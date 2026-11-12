//! Lossless formats the decoder library doesn't cover: Monkey's Audio (.ape), WavPack (.wv)
//! and True Audio (.tta). Each is decoded frame by frame in a thread of its own into a
//! [`Track`], like everything else, so playback, gapless joins and seeking work the same.
//!
//! The decoders are pure Rust (ape-decoder, wavicle, oxideav-tta), each checked against the
//! reference decoders by its authors and here against ffmpeg (tests below and in
//! `examples/`): the samples come out exactly as they went in.

use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};

use crate::decode::{Track, TrackMeta, WINDOW_BYTES};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Ape,
    WavPack,
    Tta,
}

impl Kind {
    pub fn of(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "ape" => Self::Ape,
            "wv" => Self::WavPack,
            "tta" => Self::Tta,
            _ => return None,
        })
    }

    /// As the user knows it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Ape => "APE",
            Self::WavPack => "WavPack",
            Self::Tta => "TTA",
        }
    }
}

pub fn is_lossless_extra(path: &Path) -> bool {
    Kind::of(path).is_some()
}

/// What a file holds, from its header and tags.
#[derive(Debug, Clone)]
pub struct Info {
    pub kind: Kind,
    pub sample_rate: u32,
    pub channels: usize,
    pub bits: u32,
    /// 32-bit float samples (WavPack can carry them), not integers.
    pub float: bool,
    /// Frames (samples per channel).
    pub frames: u64,
    pub meta: TrackMeta,
}

impl Info {
    pub fn duration_secs(&self) -> f64 {
        self.frames as f64 / self.sample_rate.max(1) as f64
    }
}

/// A decoder that hands out blocks of interleaved integer samples from a starting frame.
trait Source: Send {
    /// Start at `frame`; returns how many frames of the first block to drop to land on it.
    fn seek(&mut self, frame: u64) -> Result<usize>;
    /// The next block, or None at the end.
    fn next(&mut self) -> Result<Option<Vec<i32>>>;
}

pub fn probe(path: &Path) -> Result<Info> {
    let kind = Kind::of(path).ok_or_else(|| anyhow!("not an APE, WavPack or TTA file"))?;
    let (info, _) = open_source(path, kind)?;
    Ok(info)
}

fn open_source(path: &Path, kind: Kind) -> Result<(Info, Box<dyn Source>)> {
    let mut meta = read_tags(path).unwrap_or_default();
    if meta.cover.is_none() {
        meta.cover = crate::decode::folder_cover(path);
    }
    match kind {
        Kind::Ape => {
            let file = BufReader::new(File::open(path).with_context(|| format!("open {}", path.display()))?);
            let dec = ape_decoder::ApeDecoder::new(file).map_err(|e| anyhow!("APE: {e}"))?;
            let i = dec.info().clone();
            if i.is_floating_point {
                bail!("APE: floating-point files aren't supported");
            }
            let info = Info { kind, sample_rate: i.sample_rate, channels: i.channels as usize, bits: i.bits_per_sample as u32, float: false, frames: i.total_samples, meta };
            Ok((info, Box::new(Ape { dec, next: 0, bytes: i.bytes_per_sample as usize, unsigned8: i.bits_per_sample == 8 && !i.is_signed_8bit })))
        }
        Kind::WavPack => {
            let bytes = std::fs::read(path).with_context(|| format!("open {}", path.display()))?;
            let w = WavPack::new(bytes)?;
            let info = Info { kind, sample_rate: w.rate, channels: w.channels, bits: w.bits, float: w.float, frames: w.frames, meta };
            Ok((info, Box::new(w)))
        }
        Kind::Tta => {
            let bytes = std::fs::read(path).with_context(|| format!("open {}", path.display()))?;
            let t = Tta::new(bytes)?;
            let h = t.header();
            let info = Info { kind, sample_rate: h.sample_rate, channels: h.channels as usize, bits: h.bits_per_sample as u32, float: false, frames: h.total_samples as u64, meta };
            Ok((info, Box::new(t)))
        }
    }
}

/// Open for playback from `start_secs`; decoding runs on in the background.
pub fn open_at(path: &Path, start_secs: f64) -> Result<Arc<Track>> {
    open_range_at(path, None, start_secs, TrackMeta::default())
}

/// [`open_at`] for one track of an image (a CUE sheet's): `start_secs` counts from the start
/// of that part, decoding stops at its end, and the sheet's tags (`meta`) win over the file's.
pub fn open_range_at(path: &Path, range: Option<crate::cue::Range>, start_secs: f64, meta: TrackMeta) -> Result<Arc<Track>> {
    let kind = Kind::of(path).ok_or_else(|| anyhow!("not an APE, WavPack or TTA file"))?;
    let (info, mut source) = open_source(path, kind)?;
    if info.channels == 0 || info.sample_rate == 0 || !(8..=32).contains(&info.bits) {
        bail!("{}: implausible format ({} Hz, {} ch, {} bit)", kind.name(), info.sample_rate, info.channels, info.bits);
    }
    // The part of the file that is the track, in frames.
    let rate = info.sample_rate;
    let first = range.map_or(0, |r| r.start_at(rate) as u64).min(info.frames);
    let end = range.and_then(|r| r.len_at(rate)).map_or(info.frames, |len| (first + len as u64).min(info.frames));
    let length = end - first;
    let start = ((start_secs.max(0.0) * rate as f64) as u64).min(length);
    let mut skip = source.seek(first + start)?;
    let mut left = (length - start) as usize;
    let file_meta = info.meta.clone();
    let meta = TrackMeta {
        title: meta.title.or(file_meta.title),
        artist: meta.artist.or(file_meta.artist),
        album: meta.album.or(file_meta.album),
        album_artist: meta.album_artist.or(file_meta.album_artist),
        genre: meta.genre.or(file_meta.genre),
        year: meta.year.or(file_meta.year),
        track_number: meta.track_number.or(file_meta.track_number),
        disc_number: meta.disc_number.or(file_meta.disc_number),
        cover: meta.cover.or(file_meta.cover),
    };
    let track = Track::new(rate, info.channels, Some(info.bits), false, meta, length as usize);
    let track = Arc::new(track.starting_at(start as usize).windowed(WINDOW_BYTES));
    let mine = track.clone();
    let scale = 1.0 / (1u64 << (info.bits - 1)) as f32;
    let float = info.float;
    let ch = info.channels;
    std::thread::Builder::new().name("hifi-lossless".into()).spawn(move || {
        let _guard = crate::decode::FinishOnPanic(&mine);
        let mut pcm: Vec<f32> = Vec::new();
        let result = (|| -> Result<()> {
            while left > 0
                && let Some(block) = source.next()?
            {
                if !mine.make_room(|| Arc::strong_count(&mine) > 1) {
                    return Ok(());
                }
                let frames = block.len() / ch;
                let s = skip.min(frames);
                skip -= s;
                let n = (frames - s).min(left);
                left -= n;
                let part = &block[s * ch..(s + n) * ch];
                pcm.clear();
                if float {
                    pcm.extend(part.iter().map(|&v| f32::from_bits(v as u32)));
                } else {
                    pcm.extend(part.iter().map(|&v| v as f32 * scale));
                }
                if !mine.push(&pcm) {
                    return Ok(());
                }
            }
            Ok(())
        })();
        mine.finish(result.err().map(|e| format!("{e:#}")));
    })?;
    Ok(track)
}

// ---------------------------------------------------------------------------------------------
// Monkey's Audio
// ---------------------------------------------------------------------------------------------

struct Ape {
    dec: ape_decoder::ApeDecoder<BufReader<File>>,
    next: u32,
    bytes: usize,
    unsigned8: bool,
}

impl Source for Ape {
    fn seek(&mut self, frame: u64) -> Result<usize> {
        if frame == 0 {
            return Ok(0);
        }
        let p = self.dec.seek(frame).map_err(|e| anyhow!("APE: {e}"))?;
        self.next = p.frame_index;
        Ok(p.skip_samples as usize)
    }

    fn next(&mut self) -> Result<Option<Vec<i32>>> {
        if self.next >= self.dec.total_frames() {
            return Ok(None);
        }
        let raw = self.dec.decode_frame(self.next).map_err(|e| anyhow!("APE frame {}: {e}", self.next))?;
        self.next += 1;
        let b = self.bytes;
        Ok(Some(
            raw.chunks_exact(b)
                .map(|s| match b {
                    1 if self.unsigned8 => s[0] as i32 - 128,
                    1 => s[0] as i8 as i32,
                    2 => i16::from_le_bytes([s[0], s[1]]) as i32,
                    3 => ((s[2] as i32) << 24 | (s[1] as i32) << 16 | (s[0] as i32) << 8) >> 8,
                    _ => i32::from_le_bytes([s[0], s[1], s[2], s[3]]),
                })
                .collect(),
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// WavPack
// ---------------------------------------------------------------------------------------------

/// The file in memory, cut into its blocks (each decodes on its own).
struct WavPack {
    bytes: Vec<u8>,
    /// (byte range, first frame, frames) of each block that carries audio.
    blocks: Vec<(std::ops::Range<usize>, u64, u32)>,
    next: usize,
    rate: u32,
    channels: usize,
    bits: u32,
    float: bool,
    frames: u64,
}

impl WavPack {
    fn new(bytes: Vec<u8>) -> Result<Self> {
        let mut blocks = Vec::new();
        let mut at = 0;
        let mut total = None;
        while at + wavicle::block::HEADER_LEN <= bytes.len() {
            if &bytes[at..at + 4] != b"wvpk" {
                // Tags (APEv2, ID3v1) at the end, or garbage: look for the next block.
                match bytes[at + 1..].windows(4).position(|w| w == b"wvpk") {
                    Some(p) => at += 1 + p,
                    None => break,
                }
                continue;
            }
            let h = wavicle::BlockHeader::parse(&bytes[at..]).map_err(|e| anyhow!("WavPack: {e}"))?;
            let len = h.block_len();
            if at + len > bytes.len() {
                break;
            }
            if h.block_index == 0 && total.is_none() {
                total = h.total_samples;
            }
            if h.block_samples > 0 {
                blocks.push((at..at + len, h.block_index, h.block_samples));
            }
            at += len;
        }
        let first = blocks.first().ok_or_else(|| anyhow!("WavPack: no audio"))?;
        // The format, from decoding the first block.
        let d = wavicle::decode_stream(&bytes[first.0.clone()]).map_err(|e| anyhow!("WavPack: {e}"))?;
        let frames = total.unwrap_or_else(|| blocks.iter().map(|b| b.1 + b.2 as u64).max().unwrap_or(0));
        // The bits actually used: the header gives the container width (16/24/32).
        Ok(Self { rate: d.sample_rate, channels: d.channels as usize, bits: d.bits_per_sample, float: d.is_float, frames, bytes, blocks, next: 0 })
    }
}

impl Source for WavPack {
    fn seek(&mut self, frame: u64) -> Result<usize> {
        let i = self.blocks.iter().rposition(|b| b.1 <= frame).unwrap_or(0);
        self.next = i;
        Ok((frame - self.blocks[i].1.min(frame)) as usize)
    }

    fn next(&mut self) -> Result<Option<Vec<i32>>> {
        let Some((range, ..)) = self.blocks.get(self.next).cloned() else { return Ok(None) };
        self.next += 1;
        let d = wavicle::decode_stream(&self.bytes[range]).map_err(|e| anyhow!("WavPack: {e}"))?;
        Ok(Some(d.samples))
    }
}

// ---------------------------------------------------------------------------------------------
// True Audio
// ---------------------------------------------------------------------------------------------

/// The file in memory; the decoder (which borrows it) is made for each frame.
struct Tta {
    bytes: Vec<u8>,
    next: usize,
    count: usize,
}

impl Tta {
    fn new(bytes: Vec<u8>) -> Result<Self> {
        let count = oxideav_tta::Decoder::new(&bytes).map_err(|e| anyhow!("TTA: {e}"))?.frames.len();
        Ok(Self { bytes, next: 0, count })
    }

    fn header(&self) -> oxideav_tta::StreamHeader {
        oxideav_tta::Decoder::new(&self.bytes).expect("parsed in new").header
    }
}

impl Source for Tta {
    fn seek(&mut self, frame: u64) -> Result<usize> {
        if frame == 0 {
            return Ok(0);
        }
        let d = oxideav_tta::Decoder::new(&self.bytes).map_err(|e| anyhow!("TTA: {e}"))?;
        let p = d.seek_to_sample(frame).map_err(|e| anyhow!("TTA: {e}"))?;
        self.next = p.frame_index;
        Ok(p.sample_offset_in_frame as usize)
    }

    fn next(&mut self) -> Result<Option<Vec<i32>>> {
        if self.next >= self.count {
            return Ok(None);
        }
        let d = oxideav_tta::Decoder::new(&self.bytes).map_err(|e| anyhow!("TTA: {e}"))?;
        let samples = d.decode_frame_at(self.next).map_err(|e| anyhow!("TTA frame {}: {e}", self.next))?;
        self.next += 1;
        Ok(Some(samples))
    }
}

// ---------------------------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------------------------

/// APEv2 tags (all three formats use them), else ID3v2 (some TTA files).
fn read_tags(path: &Path) -> Result<TrackMeta> {
    let mut f = File::open(path)?;
    let mut m = TrackMeta::default();
    if let Some(tag) = ape_decoder::read_tag(&mut f).ok().flatten() {
        let text = |k: &str| tag.get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        m.title = text("Title");
        m.artist = text("Artist");
        m.album = text("Album");
        m.album_artist = text("Album Artist").or_else(|| text("AlbumArtist"));
        m.genre = text("Genre");
        m.year = text("Year").or_else(|| text("Date")).map(|y| y.chars().take(4).collect());
        m.track_number = text("Track").and_then(|t| t.split('/').next()?.trim().parse().ok());
        m.disc_number = text("Disc").and_then(|t| t.split('/').next()?.trim().parse().ok());
        if let Some(field) = tag.field("Cover Art (front)") {
            // "<description>\0<image bytes>"
            if let Some(nul) = field.value.iter().position(|&b| b == 0) {
                let img = field.value[nul + 1..].to_vec();
                let mime = if img.starts_with(b"\x89PNG") { "image/png" } else { "image/jpeg" };
                if !img.is_empty() {
                    m.cover = Some((mime.to_string(), img));
                }
            }
        }
    }
    if m.title.is_none() {
        f.seek(SeekFrom::Start(0))?;
        if let Some(id3) = ape_decoder::read_id3v2(&mut f).ok().flatten() {
            let clean = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            m.title = clean(id3.title());
            m.artist = m.artist.take().or_else(|| clean(id3.artist()));
            m.album = m.album.take().or_else(|| clean(id3.album()));
            m.genre = m.genre.take().or_else(|| clean(id3.genre()));
            m.year = m.year.take().or_else(|| clean(id3.year()).map(|y| y.chars().take(4).collect()));
            m.track_number = m.track_number.or_else(|| clean(id3.track()).and_then(|t| t.split('/').next()?.trim().parse().ok()));
        }
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the library makes of each sample file (tags, format). Ignored by default.
    #[test]
    #[ignore]
    fn library_reads_the_samples() {
        let dir = std::env::var("BITSPRING_LOSSLESS_SAMPLES").expect("BITSPRING_LOSSLESS_SAMPLES");
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let (t, cover) = crate::library::scan_file(&p).unwrap();
            eprintln!("{} | {} | {:?} | {:?} | {:?} | {} Hz {:?} bit | {:.1} s | cover {}", t.format, t.title, t.artist, t.album, t.track_number, t.sample_rate, t.bits, t.duration, cover.is_some());
        }
    }

    /// Decode every file in $BITSPRING_LOSSLESS_SAMPLES with us and with ffmpeg: the samples
    /// must match exactly (whole file, and from a point in the middle). Ignored by default.
    #[test]
    #[ignore]
    fn matches_ffmpeg() {
        let dir = std::env::var("BITSPRING_LOSSLESS_SAMPLES").expect("BITSPRING_LOSSLESS_SAMPLES");
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if Kind::of(&p).is_none() || p.file_name().unwrap().to_string_lossy().starts_with('.') {
                continue;
            }
            let info = probe(&p).unwrap();
            let (fmt, codec) = if info.float { ("f32le", "pcm_f32le") } else { ("s32le", "pcm_s32le") };
            let out = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&p)
                .args(["-f", fmt, "-acodec", codec, "-"])
                .output()
                .unwrap();
            let want: Vec<i32> = out
                .stdout
                .chunks_exact(4)
                .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .map(|v| if info.float { v } else { v >> (32 - info.bits) })
                .collect();
            for start in [0.0, 1.5] {
                let t = open_at(&p, start).unwrap();
                let from = (start * info.sample_rate as f64) as usize;
                // The first minute (a long file is held in a window, not whole).
                let n = (want.len() / info.channels - from).min(60 * info.sample_rate as usize);
                assert!(t.wait_for(from + n, std::time::Duration::from_secs(120)), "{}", p.display());
                assert_eq!(t.error(), None, "{}", p.display());
                assert_eq!(info.frames as usize, want.len() / info.channels, "{}: length", p.display());
                let full = (1u64 << (info.bits - 1)) as f64;
                let float = info.float;
                let got: Vec<i32> = (from..from + n)
                    .flat_map(|f| (0..info.channels).map(move |c| (f, c)))
                    .map(|(f, c)| if float { t.sample(f, c).to_bits() as i32 } else { (t.sample(f, c) as f64 * full).round() as i32 })
                    .collect();
                let want = &want[from * info.channels..(from + n) * info.channels];
                if got != want {
                    let i = got.iter().zip(want).position(|(a, b)| a != b).unwrap_or(0);
                    eprintln!("first diff at {i}: ours {:?} ffmpeg {:?}", got[i..i + 4].iter().map(|v| f32::from_bits(*v as u32)).collect::<Vec<_>>(), want[i..i + 4].iter().map(|v| f32::from_bits(*v as u32)).collect::<Vec<_>>());
                }
                assert_eq!(got.len(), want.len(), "{} from {start}", p.display());
                assert!(got == want, "{} from {start}: samples differ", p.display());
            }
            eprintln!("ok {} ({} Hz, {} bit, {} ch, {:.1} s)", p.display(), info.sample_rate, info.bits, info.channels, info.duration_secs());
        }
    }
}

#[cfg(test)]
mod images {
    /// A folder with an image and a CUE sheet ($BITSPRING_CUE_FOLDER): the library lists the
    /// sheet's tracks, and track 2 plays exactly that part of the image (vs ffmpeg).
    #[test]
    #[ignore]
    fn cue_tracks_of_an_image() {
        let dir = std::path::PathBuf::from(std::env::var("BITSPRING_CUE_FOLDER").expect("BITSPRING_CUE_FOLDER"));
        let r = crate::library::scan(std::slice::from_ref(&dir), &[], |_, _| {}, |_, _| {});
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        for t in &r.tracks {
            eprintln!("{} | {} | {:?} | {:.1} s", t.path.rsplit('/').next().unwrap(), t.title, t.artist, t.duration);
        }
        let two = r.tracks.iter().find(|t| t.track_number == Some(2) && t.path.contains("#cue:")).expect("a track 2");
        let (file, Some(range)) = crate::cue::split(&two.path) else { panic!("not a CUE track") };
        let info = super::probe(std::path::Path::new(file)).unwrap();
        let start = range.start_at(info.sample_rate) as f64 / info.sample_rate as f64;
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i", file, "-ss", &format!("{start:.6}"), "-t", "10", "-f", "f32le", "-acodec", "pcm_f32le", "-"])
            .output()
            .unwrap();
        let want: Vec<f32> = out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        let t = crate::open_file(std::path::Path::new(&two.path), crate::dsd::DsdMode::Pcm).unwrap();
        let n = want.len() / info.channels;
        assert!(t.wait_for(n, std::time::Duration::from_secs(60)));
        let got: Vec<f32> = (0..n).flat_map(|f| (0..info.channels).map(move |c| (f, c))).map(|(f, c)| t.sample(f, c)).collect();
        let off = got.iter().zip(&want).filter(|(a, b)| (*a - *b).abs() > 1e-6).count();
        assert_eq!(off, 0, "{} of {} samples differ", off, got.len());
        eprintln!("track 2 '{}': first 10 s identical to ffmpeg", two.title);
    }
}
