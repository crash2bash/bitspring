//! DSD: DSF and DSDIFF (.dff) files.
//!
//! Two ways to play them:
//! - **DoP** (DSD over PCM): the 1-bit stream goes to the DAC untouched, 16 bits per channel
//!   packed into each 24-bit PCM word under an alternating 0x05/0xFA marker byte. The DAC sees
//!   the markers and switches to DSD. Needs a DoP-capable DAC (FiiO Q7 and most USB DSD DACs)
//!   running at `dsd_rate / 16` (176.4 kHz for DSD64) with at least 24 integer bits.
//! - **PCM conversion** for everything else (and whenever EQ or crossfeed must run): the
//!   bitstream is low-pass filtered and decimated to 176.4 kHz (192 kHz for the 48k family).
//!
//! The DoP track keeps the raw bits in RAM (1x the file size); the IOProc adds the markers on
//! the fly, so they stay continuous through seeks and pauses.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, MetadataReader};
use symphonia::default::meta::Id3v2Reader;

use crate::decode::{DsdSource, Track, TrackMeta, meta_from_revision};

/// 1-bit sample rate of DSD64 (64 × 44.1 kHz).
pub const DSD64: u32 = 2_822_400;

/// Highest DSD rate we accept (DSD1024); beyond it a header is lying.
const MAX_DSD_RATE: u32 = DSD64 * 16;
/// Largest DSF block per channel we accept (the spec fixes it at 4096).
const MAX_DSF_BLOCK: usize = 1 << 16;
/// Largest tag we read into memory.
const MAX_TAG: u64 = 64 << 20;

/// Offset of the chunk after one at `body` with `size` bytes (chunks are padded to even sizes),
/// or None when a forged size would overflow — so a chunk walk always moves forward and ends.
fn next_chunk(body: u64, size: u64) -> Option<u64> {
    body.checked_add(size)?.checked_add(size & 1)
}

/// DoP idle pattern: the DSD equivalent of digital silence (equal ones and zeros).
pub const DOP_IDLE: u16 = 0x6969;

/// Level mapping for PCM conversion: the SACD spec's largest allowed level (70 % modulation)
/// lands exactly at 0 dBFS, so no legal DSD can clip after conversion.
pub const PCM_GAIN: f64 = 1.0 / 0.7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsdMode {
    Dop,
    Pcm,
}

pub fn is_dsd_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("dsf") || e.eq_ignore_ascii_case("dff"))
        || p.to_str().is_some_and(|s| crate::sacd::split(s).1.is_some())
}

/// "DSD64", "DSD128", … (by multiple of 44.1 kHz; the 48k family gets its own multiple).
pub fn dsd_name(rate: u32) -> String {
    let base = if rate % 44_100 == 0 { 44_100 } else { 48_000 };
    format!("DSD{}", rate / base)
}

/// PCM rate a conversion produces.
pub fn pcm_rate(dsd_rate: u32) -> u32 {
    if dsd_rate % 44_100 == 0 { 176_400 } else { 192_000 }
}

#[derive(Debug, Clone, Copy)]
enum Layout {
    /// Per-channel blocks of `block` bytes, channel after channel.
    Dsf { block: usize, lsb_first: bool },
    /// One byte per channel, interleaved.
    Dff,
    /// A track of a SACD image (see crate::sacd).
    Sacd,
}

/// What the header says, before any audio is read.
#[derive(Debug, Clone)]
pub struct DsdHeader {
    /// 1-bit samples per second per channel.
    pub rate: u32,
    pub channels: usize,
    /// 1-bit samples per channel.
    pub samples: u64,
    pub meta: TrackMeta,
    /// "DSF" or "DFF".
    pub container: &'static str,
    layout: Layout,
    data_start: u64,
    data_len: u64,
}

impl DsdHeader {
    pub fn duration_secs(&self) -> f64 {
        self.samples as f64 / self.rate as f64
    }
}

fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}
fn u64le(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

fn read_exact_at(f: &mut File, pos: u64, n: usize) -> Result<Vec<u8>> {
    f.seek(SeekFrom::Start(pos))?;
    let mut v = vec![0; n];
    f.read_exact(&mut v)?;
    Ok(v)
}

fn id3_meta(tag: &[u8]) -> TrackMeta {
    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(tag.to_vec())), Default::default());
    match Id3v2Reader::try_new(mss, MetadataOptions::default()).and_then(|mut r| r.read_all()) {
        Ok(buf) => meta_from_revision(&buf.revision),
        Err(e) => {
            log::warn!("unreadable ID3v2 tag: {e}");
            TrackMeta::default()
        }
    }
}

/// Parse a DSF or DFF header (and its tags).
pub fn probe(path: &Path) -> Result<DsdHeader> {
    if let Some(s) = path.to_str().filter(|s| crate::sacd::split(s).1.is_some()) {
        let t = crate::sacd::probe(s)?;
        return Ok(DsdHeader { rate: t.rate, channels: t.channels, samples: t.samples, meta: t.meta, container: "SACD", layout: Layout::Sacd, data_start: 0, data_len: 0 });
    }
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let file_len = f.metadata()?.len();
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)?;
    match &magic {
        b"DSD " => probe_dsf(&mut f, file_len),
        b"FRM8" => probe_dff(&mut f, file_len),
        _ => bail!("not a DSF or DSDIFF file"),
    }
}

fn probe_dsf(f: &mut File, file_len: u64) -> Result<DsdHeader> {
    let head = read_exact_at(f, 0, 28 + 52 + 12)?;
    let meta_ptr = u64le(&head[20..]);
    let fmt = &head[28..80];
    if &fmt[..4] != b"fmt " {
        bail!("DSF: missing fmt chunk");
    }
    if u32le(&fmt[16..]) != 0 {
        bail!("DSF: unsupported format id");
    }
    let channels = u32le(&fmt[24..]) as usize;
    let rate = u32le(&fmt[28..]);
    let bits = u32le(&fmt[32..]);
    let samples = u64le(&fmt[36..]);
    let block = u32le(&fmt[44..]) as usize;
    let data = &head[80..];
    if &data[..4] != b"data" {
        bail!("DSF: missing data chunk");
    }
    if !(1..=8).contains(&channels)
        || !(DSD64 / 2..=MAX_DSD_RATE).contains(&rate)
        || !(1..=MAX_DSF_BLOCK).contains(&block)
        || !matches!(bits, 1 | 8)
    {
        bail!("DSF: implausible header ({channels} ch, {rate} Hz, {bits} bit, block {block})");
    }
    let data_len = u64le(&data[4..]).saturating_sub(12).min(file_len.saturating_sub(92));
    // Never trust the sample count beyond what the data chunk can hold.
    let samples = samples.min(data_len / channels as u64 * 8);
    let meta = if meta_ptr > 0 && meta_ptr < file_len && file_len - meta_ptr <= MAX_TAG {
        read_exact_at(f, meta_ptr, (file_len - meta_ptr) as usize).map(|t| id3_meta(&t)).unwrap_or_default()
    } else {
        TrackMeta::default()
    };
    Ok(DsdHeader {
        rate,
        channels,
        samples,
        meta,
        container: "DSF",
        layout: Layout::Dsf { block, lsb_first: bits == 1 },
        data_start: 92,
        data_len,
    })
}

fn probe_dff(f: &mut File, file_len: u64) -> Result<DsdHeader> {
    let mut hdr = [0u8; 12];
    f.seek(SeekFrom::Start(4))?;
    f.read_exact(&mut hdr)?;
    if &hdr[8..12] != b"DSD " {
        bail!("DSDIFF: not a DSD form");
    }
    let (mut rate, mut channels, mut data) = (0u32, 0usize, None);
    let mut meta = TrackMeta::default();
    // Top-level chunks after FRM8 header (16 bytes).
    let mut pos = 16u64;
    while pos + 12 <= file_len {
        let h = read_exact_at(f, pos, 12)?;
        let id: [u8; 4] = h[..4].try_into().unwrap();
        let size = u64::from_be_bytes(h[4..12].try_into().unwrap());
        let body = pos + 12;
        match &id {
            b"PROP" => {
                let end = body.saturating_add(size).min(file_len);
                let mut p = body + 4; // skip "SND "
                while p + 12 <= end {
                    let sh = read_exact_at(f, p, 12)?;
                    let sid: [u8; 4] = sh[..4].try_into().unwrap();
                    let ssize = u64::from_be_bytes(sh[4..12].try_into().unwrap());
                    match &sid {
                        b"FS  " => rate = u32::from_be_bytes(read_exact_at(f, p + 12, 4)?.try_into().unwrap()),
                        b"CHNL" => channels = u16::from_be_bytes(read_exact_at(f, p + 12, 2)?.try_into().unwrap()) as usize,
                        b"CMPR" => {
                            if &read_exact_at(f, p + 12, 4)?[..] != b"DSD " {
                                bail!("DSDIFF: DST-compressed files are not supported");
                            }
                        }
                        _ => {}
                    }
                    match next_chunk(p + 12, ssize) {
                        Some(n) => p = n,
                        None => break,
                    }
                }
            }
            b"DSD " => data = Some((body, size.min(file_len - body))),
            b"DST " => bail!("DSDIFF: DST-compressed files are not supported"),
            b"ID3 " if size <= MAX_TAG => meta = id3_meta(&read_exact_at(f, body, size as usize)?),
            b"DIIN" if meta.title.is_none() => meta = diin_meta(f, body, body.saturating_add(size).min(file_len))?,
            _ => {}
        }
        match next_chunk(body, size) {
            Some(n) => pos = n,
            None => break,
        }
    }
    let (data_start, data_len) = data.ok_or_else(|| anyhow!("DSDIFF: no DSD sound data"))?;
    if !(1..=8).contains(&channels) || !(DSD64 / 2..=MAX_DSD_RATE).contains(&rate) {
        bail!("DSDIFF: implausible header ({channels} ch, {rate} Hz)");
    }
    Ok(DsdHeader {
        rate,
        channels,
        samples: data_len / channels as u64 * 8,
        meta,
        container: "DFF",
        layout: Layout::Dff,
        data_start,
        data_len,
    })
}

/// DSDIFF "edited master" info: title (DITI) and artist (DIAR).
fn diin_meta(f: &mut File, start: u64, end: u64) -> Result<TrackMeta> {
    let mut m = TrackMeta::default();
    let mut p = start;
    while p + 12 <= end {
        let h = read_exact_at(f, p, 12)?;
        let size = u64::from_be_bytes(h[4..12].try_into().unwrap());
        if (&h[..4] == b"DITI" || &h[..4] == b"DIAR") && size >= 4 && size < 4096 {
            let b = read_exact_at(f, p + 12, size as usize)?;
            let n = (u32::from_be_bytes(b[..4].try_into().unwrap()) as usize).min(b.len() - 4);
            let text = String::from_utf8_lossy(&b[4..4 + n]).trim().to_string();
            if &h[..4] == b"DITI" { m.title = Some(text) } else { m.artist = Some(text) }
        }
        match next_chunk(p + 12, size) {
            Some(n) => p = n,
            None => break,
        }
    }
    Ok(m)
}

/// Bytes per channel `read_bitstream` can start at: whole DSF blocks (4096 bytes per channel
/// in every file seen); the same step for DFF.
fn start_step(h: &DsdHeader) -> u64 {
    match h.layout {
        Layout::Dsf { block, .. } => block as u64,
        Layout::Dff => 4096,
        Layout::Sacd => crate::sacd::FRAME_BYTES,
    }
}

/// Read the sound data and hand it on as per-channel byte runs (MSB = oldest bit), in order,
/// from byte `from` of each channel (a multiple of `start_step`). `sink` returns false to stop
/// early.
fn read_bitstream(path: &Path, h: &DsdHeader, from: u64, mut sink: impl FnMut(&[Vec<u8>]) -> bool) -> Result<()> {
    if matches!(h.layout, Layout::Sacd) {
        return crate::sacd::read_bitstream(&path.to_string_lossy(), from, sink);
    }
    let mut f = File::open(path)?;
    let ch = h.channels;
    let per_channel = h.samples.div_ceil(8);
    let from = from.min(per_channel);
    // DSF: blocks of every channel in turn; DFF: bytes of every channel in turn.
    f.seek(SeekFrom::Start(h.data_start + from * ch as u64))?;
    let mut done = from; // bytes per channel delivered
    let mut out: Vec<Vec<u8>> = vec![Vec::new(); ch];
    match h.layout {
        Layout::Dsf { block, lsb_first } => {
            let mut group = vec![0u8; block * ch];
            while done < per_channel {
                if read_full(&mut f, &mut group)? < group.len() {
                    break; // truncated file: play what is there
                }
                let take = (per_channel - done).min(block as u64) as usize;
                for (c, o) in out.iter_mut().enumerate() {
                    o.clear();
                    let src = &group[c * block..c * block + take];
                    if lsb_first {
                        o.extend(src.iter().map(|b| b.reverse_bits()));
                    } else {
                        o.extend_from_slice(src);
                    }
                }
                done += take as u64;
                if !sink(&out) {
                    break;
                }
            }
        }
        Layout::Dff => {
            let mut buf = vec![0u8; 16 * 1024 * ch];
            let mut left = (h.data_len - h.data_len % ch as u64).saturating_sub(from * ch as u64);
            while left > 0 {
                let want = (buf.len() as u64).min(left) as usize;
                let n = read_full(&mut f, &mut buf[..want])?;
                let n = n - n % ch;
                if n == 0 {
                    break;
                }
                for (c, o) in out.iter_mut().enumerate() {
                    o.clear();
                    o.extend(buf[..n].iter().skip(c).step_by(ch));
                }
                left -= n as u64;
                if !sink(&out) {
                    break;
                }
            }
        }
        Layout::Sacd => unreachable!("read above"),
    }
    Ok(())
}

fn read_full(f: &mut File, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Open a DSD file and start reading it in the background (like [`crate::open_track`]).
pub fn open_dsd(path: &Path, mode: DsdMode) -> Result<Arc<Track>> {
    open_dsd_at(path, mode, 0.0)
}

/// [`open_dsd`], from `start_secs` on.
pub fn open_dsd_at(path: &Path, mode: DsdMode, start_secs: f64) -> Result<Arc<Track>> {
    let mut h = probe(path)?;
    if h.meta.cover.is_none() {
        h.meta.cover = crate::decode::folder_cover(path);
    }
    let track = match mode {
        DsdMode::Dop => Track::new_dop(h.rate, h.channels, h.meta.clone(), (h.samples / 16) as usize),
        DsdMode::Pcm => {
            let rate = pcm_rate(h.rate);
            let mut t = Track::new(rate, h.channels, None, false, h.meta.clone(), (h.duration_secs() * rate as f64) as usize);
            t.dsd = Some(DsdSource { rate: h.rate, dop: false });
            t
        }
    };
    // Bytes per channel per output frame: 2 for DoP (16 bits), the decimation for PCM.
    let per_frame = (h.rate / 8 / track.sample_rate) as u64;
    let target = (start_secs.max(0.0) * track.sample_rate as f64) as u64;
    // Start a block early: the converter's filters settle on it, and its output is dropped.
    let step = start_step(&h);
    let warm = if mode == DsdMode::Pcm { step } else { 0 };
    let from = (target * per_frame).saturating_sub(warm) / step * step;
    let first = from / per_frame; // the frame the bytes at `from` become
    let mut skip = (target - first.min(target)) as usize;
    let track = Arc::new(track.starting_at(target as usize).windowed(crate::decode::WINDOW_BYTES));
    let mine = track.clone();
    let path = path.to_path_buf();
    let mut converter = match mode {
        DsdMode::Pcm => Some(Converter::new(h.rate, h.channels)?),
        DsdMode::Dop => None,
    };
    std::thread::Builder::new().name("hifi-dsd".into()).spawn(move || {
        let _guard = crate::decode::FinishOnPanic(&mine);
        let ch = h.channels;
        let mut pending: Vec<Vec<u8>> = vec![Vec::new(); ch];
        let mut frame_buf: Vec<u8> = Vec::new();
        let mut pcm: Vec<f32> = Vec::new();
        let result = read_bitstream(&path, &h, from, |runs| {
            // Wait while the window ahead is full; stop once nobody will play it any more.
            if !mine.make_room(|| Arc::strong_count(&mine) > 1) {
                return false;
            }
            match &mut converter {
                None => {
                    // DoP frames: 2 bytes per channel. Keep an odd trailing byte for next time.
                    for (p, r) in pending.iter_mut().zip(runs) {
                        p.extend_from_slice(r);
                    }
                    let frames = pending[0].len() / 2;
                    frame_buf.clear();
                    for i in 0..frames {
                        for p in &pending {
                            frame_buf.extend_from_slice(&p[2 * i..2 * i + 2]);
                        }
                    }
                    for p in &mut pending {
                        p.drain(..frames * 2);
                    }
                    let fb = ch * 2;
                    let s = skip.min(frame_buf.len() / fb);
                    skip -= s;
                    mine.push_dop(&frame_buf[s * fb..])
                }
                Some(conv) => {
                    conv.process(runs, &mut pcm);
                    let s = skip.min(pcm.len() / ch);
                    skip -= s;
                    mine.push(&pcm[s * ch..])
                }
            }
        });
        mine.finish(result.err().map(|e| format!("read: {e}")));
    })?;
    Ok(track)
}

// ---------------------------------------------------------------------------------------------
// DSD → PCM
// ---------------------------------------------------------------------------------------------

/// Stage 1 FIR length at the DSD rate (12 bytes of history → 12 table lookups per output).
const S1_TAPS: usize = 96;
const S1_BYTES: usize = S1_TAPS / 8;

/// Two-stage decimator: DSD → rate/8 with a byte-lookup FIR, then → 176.4/192 kHz with a
/// polyphase FIR whose passband is flat to 30 kHz and which is 100 dB down from 60 kHz —
/// the region where DSD's shaped noise starts to climb.
pub struct Converter {
    lut: Vec<[f32; 256]>,
    taps2: Vec<f64>,
    decim2: usize,
    chans: Vec<ChannelState>,
    gain: f64,
}

struct ChannelState {
    hist: [u8; 16],
    hpos: usize,
    /// Stage-1 output history, stored twice so a contiguous window always exists.
    ring: Vec<f64>,
    rpos: usize,
    phase: usize,
    out: Vec<f64>,
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    while term > 1e-12 * sum {
        term *= (x / (2.0 * k)).powi(2);
        sum += term;
        k += 1.0;
    }
    sum
}

/// Kaiser-windowed sinc low-pass, `cutoff` as a fraction of the sample rate, unity DC gain.
fn kaiser_lowpass(n: usize, cutoff: f64, beta: f64) -> Vec<f64> {
    let m = (n - 1) as f64;
    let mut h: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 - m / 2.0;
            let sinc = if t == 0.0 { 2.0 * cutoff } else { (2.0 * std::f64::consts::PI * cutoff * t).sin() / (std::f64::consts::PI * t) };
            let r = 2.0 * i as f64 / m - 1.0;
            sinc * bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / bessel_i0(beta)
        })
        .collect();
    let sum: f64 = h.iter().sum();
    h.iter_mut().for_each(|v| *v /= sum);
    h
}

fn kaiser_beta(atten_db: f64) -> f64 {
    if atten_db > 50.0 { 0.1102 * (atten_db - 8.7) } else { 0.5842 * (atten_db - 21.0).powf(0.4) + 0.07886 * (atten_db - 21.0) }
}

impl Converter {
    pub fn new(dsd_rate: u32, channels: usize) -> Result<Self> {
        let fs1 = dsd_rate as f64 / 8.0;
        let out = pcm_rate(dsd_rate) as f64;
        let decim2 = (fs1 / out).round() as usize;
        if decim2 < 1 || (fs1 / decim2 as f64 - out).abs() > 0.5 {
            bail!("unsupported DSD rate {dsd_rate} Hz");
        }
        // Stage 1: protect 0–60 kHz from aliasing at fs1 (stopband from fs1 − 60 kHz).
        let s1 = kaiser_lowpass(S1_TAPS, (fs1 / 2.0) / dsd_rate as f64, kaiser_beta(120.0));
        let lut = (0..S1_BYTES)
            .map(|k| {
                let mut t = [0f32; 256];
                for (v, slot) in t.iter_mut().enumerate() {
                    // Byte k back from the newest; bit b (LSB = 0) is sample n − 8k − b.
                    *slot = (0..8).map(|b| s1[8 * k + b] * if (v >> b) & 1 == 1 { 1.0 } else { -1.0 }).sum::<f64>() as f32;
                }
                t
            })
            .collect();
        // Stage 2: −6 dB at 45 kHz, 30 kHz transition, 100 dB stopband.
        let (atten, width) = (100.0, 30_000.0 / fs1);
        let n2 = (((atten - 8.0) / (2.285 * 2.0 * std::f64::consts::PI * width)).ceil() as usize + 1) | 1;
        let taps2 = if decim2 == 1 { vec![1.0] } else { kaiser_lowpass(n2, 45_000.0 / fs1, kaiser_beta(atten)) };
        let len = taps2.len();
        let chans = (0..channels)
            .map(|_| ChannelState { hist: [0x69; 16], hpos: 0, ring: vec![0.0; 2 * len], rpos: 0, phase: 0, out: Vec::new() })
            .collect();
        Ok(Self { lut, taps2, decim2, chans, gain: PCM_GAIN })
    }

    /// Convert one run of bytes per channel (equal lengths) into interleaved PCM in `out`.
    pub fn process(&mut self, runs: &[Vec<u8>], out: &mut Vec<f32>) {
        let len = self.taps2.len();
        for (st, bytes) in self.chans.iter_mut().zip(runs) {
            st.out.clear();
            for &b in bytes {
                st.hpos = (st.hpos + 1) & 15;
                st.hist[st.hpos] = b;
                let mut y1 = 0f32;
                for (k, table) in self.lut.iter().enumerate() {
                    y1 += table[st.hist[(st.hpos.wrapping_sub(k)) & 15] as usize];
                }
                st.ring[st.rpos] = y1 as f64;
                st.ring[st.rpos + len] = y1 as f64;
                st.rpos = (st.rpos + 1) % len;
                st.phase += 1;
                if st.phase == self.decim2 {
                    st.phase = 0;
                    let window = &st.ring[st.rpos..st.rpos + len];
                    let y: f64 = window.iter().zip(&self.taps2).map(|(x, g)| x * g).sum();
                    st.out.push(y * self.gain);
                }
            }
        }
        let frames = self.chans.iter().map(|c| c.out.len()).min().unwrap_or(0);
        out.clear();
        for i in 0..frames {
            for c in &self.chans {
                out.push(c.out[i] as f32);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Test signal writer (tests and the `make_dsd` example)
// ---------------------------------------------------------------------------------------------

/// Second-order sigma-delta modulator: `signal(t)` (−1..1, per channel) → bits (MSB first).
pub fn modulate(rate: u32, channels: usize, secs: f64, signal: impl Fn(usize, f64) -> f64) -> Vec<Vec<u8>> {
    let n = (secs * rate as f64) as usize / 8 * 8;
    (0..channels)
        .map(|c| {
            let (mut i1, mut i2, mut y) = (0f64, 0f64, 0f64);
            let mut bytes = vec![0u8; n / 8];
            for i in 0..n {
                let x = signal(c, i as f64 / rate as f64);
                i1 += x - y;
                i2 += i1 - y;
                let bit = i2 >= 0.0;
                y = if bit { 1.0 } else { -1.0 };
                if bit {
                    bytes[i / 8] |= 0x80 >> (i % 8);
                }
            }
            bytes
        })
        .collect()
}

/// Write per-channel MSB-first bitstreams as a DSF file (LSB-first blocks of 4096, per spec).
pub fn write_dsf(path: &Path, rate: u32, channels: &[Vec<u8>]) -> std::io::Result<()> {
    const BLOCK: usize = 4096;
    let ch = channels.len();
    let bytes = channels[0].len();
    let blocks = bytes.div_ceil(BLOCK);
    let data_len = (blocks * BLOCK * ch) as u64;
    let total = 28 + 52 + 12 + data_len;
    let mut v = Vec::with_capacity(total as usize);
    v.extend_from_slice(b"DSD ");
    v.extend_from_slice(&28u64.to_le_bytes());
    v.extend_from_slice(&total.to_le_bytes());
    v.extend_from_slice(&0u64.to_le_bytes());
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&52u64.to_le_bytes());
    for x in [1u32, 0, if ch == 2 { 2 } else { 1 }, ch as u32, rate, 1] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.extend_from_slice(&(bytes as u64 * 8).to_le_bytes());
    v.extend_from_slice(&(BLOCK as u32).to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(12 + data_len).to_le_bytes());
    for b in 0..blocks {
        for c in channels {
            let mut block = [0u8; BLOCK];
            let src = &c[b * BLOCK..((b + 1) * BLOCK).min(bytes)];
            for (d, s) in block.iter_mut().zip(src) {
                *d = s.reverse_bits();
            }
            v.extend_from_slice(&block);
        }
    }
    std::fs::write(path, v)
}

/// Write per-channel MSB-first bitstreams as an uncompressed DSDIFF file.
pub fn write_dff(path: &Path, rate: u32, channels: &[Vec<u8>]) -> std::io::Result<()> {
    let ch = channels.len();
    let n = channels[0].len();
    let chunk = |id: &[u8; 4], body: &[u8]| {
        let mut c = id.to_vec();
        c.extend_from_slice(&(body.len() as u64).to_be_bytes());
        c.extend_from_slice(body);
        if body.len() % 2 == 1 {
            c.push(0);
        }
        c
    };
    let mut chnl = (ch as u16).to_be_bytes().to_vec();
    for i in 0..ch {
        chnl.extend_from_slice(match (ch, i) {
            (2, 0) => b"SLFT",
            (2, 1) => b"SRGT",
            _ => b"C000",
        });
    }
    let mut cmpr = b"DSD ".to_vec();
    cmpr.push(14);
    cmpr.extend_from_slice(b"not compressed");
    let mut prop = b"SND ".to_vec();
    prop.extend(chunk(b"FS  ", &rate.to_be_bytes()));
    prop.extend(chunk(b"CHNL", &chnl));
    prop.extend(chunk(b"CMPR", &cmpr));
    let mut data = Vec::with_capacity(n * ch);
    for i in 0..n {
        for c in channels {
            data.push(c[i]);
        }
    }
    let mut form = b"DSD ".to_vec();
    form.extend(chunk(b"FVER", &0x0105_0000u32.to_be_bytes()));
    form.extend(chunk(b"PROP", &prop));
    form.extend(chunk(b"DSD ", &data));
    std::fs::write(path, chunk(b"FRM8", &form))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tone(c: usize, t: f64) -> f64 {
        let f = if c == 0 { 1000.0 } else { 3000.0 };
        0.5 * (2.0 * std::f64::consts::PI * f * t).sin()
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("hifi-dsd-test-{}-{name}", std::process::id()))
    }

    /// Amplitude of `freq` in `x` (single-bin DFT) — and the RMS of everything else.
    fn tone_and_residual(x: &[f64], rate: f64, freq: f64) -> (f64, f64) {
        let (mut re, mut im) = (0.0, 0.0);
        for (i, v) in x.iter().enumerate() {
            let w = 2.0 * std::f64::consts::PI * freq * i as f64 / rate;
            re += v * w.cos();
            im += v * w.sin();
        }
        let (re, im) = (2.0 * re / x.len() as f64, 2.0 * im / x.len() as f64);
        let resid: f64 = x
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let w = 2.0 * std::f64::consts::PI * freq * i as f64 / rate;
                (v - re * w.cos() - im * w.sin()).powi(2)
            })
            .sum::<f64>()
            / x.len() as f64;
        ((re * re + im * im).sqrt(), resid.sqrt())
    }

    #[test]
    fn dsf_and_dff_round_trip_to_pcm() {
        let bits = modulate(DSD64, 2, 0.25, tone);
        for (name, write) in [("t.dsf", write_dsf as fn(&Path, u32, &[Vec<u8>]) -> _), ("t.dff", write_dff)] {
            let p = tmp(name);
            write(&p, DSD64, &bits).unwrap();
            let h = probe(&p).unwrap();
            assert_eq!((h.rate, h.channels), (DSD64, 2), "{name}");
            assert!((h.duration_secs() - 0.25).abs() < 0.002, "{name}: {}", h.duration_secs());

            let t = open_dsd(&p, DsdMode::Pcm).unwrap();
            assert!(t.wait_for(usize::MAX, Duration::from_secs(10)));
            assert_eq!(t.sample_rate, 176_400);
            let pcm = t.to_vec();
            for (c, f) in [(0, 1000.0), (1, 3000.0)] {
                // Skip the filters' start-up, measure an exact number of cycles.
                // In-band (≤ 20 kHz) only: the modulator's shaped noise above is expected. The test
                // modulator is 2nd order, so ~75 dB is its ceiling (ffmpeg decodes it to the same).
                let ch: Vec<f64> = pcm.iter().skip(c).step_by(2).map(|&v| v as f64).collect();
                let lp = kaiser_lowpass(511, 20_000.0 / 176_400.0, kaiser_beta(120.0));
                let x: Vec<f64> = (4410..4410 + 35_280).map(|i| lp.iter().enumerate().map(|(k, g)| g * ch[i + k]).sum()).collect();
                let (amp, resid) = tone_and_residual(&x, 176_400.0, f);
                let want = 0.5 * PCM_GAIN;
                assert!((amp - want).abs() < 0.01, "{name} ch{c}: amplitude {amp}, want {want}");
                let snr = 20.0 * (amp / resid).log10();
                assert!(snr > 70.0, "{name} ch{c}: SNR {snr:.1} dB");
            }
            std::fs::remove_file(&p).ok();
        }
    }

    #[test]
    fn opened_in_the_middle_it_plays_what_the_whole_file_has_there() {
        let rate = DSD64;
        let bits = modulate(rate, 2, 1.0, tone);
        for (name, write) in [("mid.dsf", write_dsf as fn(&std::path::Path, u32, &[Vec<u8>]) -> std::io::Result<()>), ("mid.dff", write_dff)] {
            let p = tmp(name);
            write(&p, rate, &bits).unwrap();
            // DoP: the very same bits from the frame asked for.
            let whole = open_dsd(&p, DsdMode::Dop).unwrap();
            let part = open_dsd_at(&p, DsdMode::Dop, 0.4).unwrap();
            assert!(whole.wait_for(usize::MAX, Duration::from_secs(10)) && part.wait_for(usize::MAX, Duration::from_secs(10)));
            let at = (0.4 * part.sample_rate as f64) as usize;
            assert_eq!(part.held_from(), at);
            assert_eq!(part.frames(), whole.frames());
            for f in [at, at + 1, at + 5_000, whole.frames() - 1] {
                for c in 0..2 {
                    assert_eq!(part.dop_payload(f, c), whole.dop_payload(f, c), "{name} DoP frame {f}");
                }
            }
            // PCM: once the filters have settled on the block before, sample for sample.
            let whole = open_dsd(&p, DsdMode::Pcm).unwrap();
            let part = open_dsd_at(&p, DsdMode::Pcm, 0.4).unwrap();
            assert!(whole.wait_for(usize::MAX, Duration::from_secs(10)) && part.wait_for(usize::MAX, Duration::from_secs(10)));
            let at = (0.4 * part.sample_rate as f64) as usize;
            assert_eq!(part.held_from(), at);
            for f in (at..whole.frames()).step_by(997) {
                for c in 0..2 {
                    assert_eq!(part.sample(f, c), whole.sample(f, c), "{name} PCM frame {f}");
                }
            }
            std::fs::remove_file(&p).ok();
        }
    }

    #[test]
    fn dop_track_carries_the_exact_bits() {
        let bits = modulate(DSD64 * 2, 2, 0.1, tone);
        let p = tmp("dop.dsf");
        write_dsf(&p, DSD64 * 2, &bits).unwrap();
        let t = open_dsd(&p, DsdMode::Dop).unwrap();
        assert!(t.wait_for(usize::MAX, Duration::from_secs(10)));
        assert!(t.is_dop());
        assert_eq!(t.sample_rate, 352_800);
        assert_eq!(t.frames(), bits[0].len() / 2);
        for f in [0, 1, 777, t.frames() - 1] {
            for c in 0..2 {
                assert_eq!(t.dop_payload(f, c), u16::from_be_bytes([bits[c][2 * f], bits[c][2 * f + 1]]));
            }
        }
        std::fs::remove_file(&p).ok();
    }

    /// Forged headers must neither hang the parser nor make it allocate what the file can't hold.
    #[test]
    fn forged_headers_are_contained() {
        let bits = modulate(DSD64, 2, 0.01, tone);
        // DSF claiming 2^63 samples: clamped to what the data chunk holds.
        let p = tmp("forged.dsf");
        write_dsf(&p, DSD64, &bits).unwrap();
        let mut v = std::fs::read(&p).unwrap();
        v[64..72].copy_from_slice(&(u64::MAX / 2).to_le_bytes());
        std::fs::write(&p, &v).unwrap();
        let h = probe(&p).unwrap();
        assert!(h.samples <= (v.len() as u64) * 8, "samples {} not clamped", h.samples);
        // A gigantic block size is refused outright.
        v[72..76].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&p, &v).unwrap();
        assert!(probe(&p).is_err());

        // DFF whose chunk sizes would overflow the offset arithmetic: the walk must end.
        let p2 = tmp("forged.dff");
        write_dff(&p2, DSD64, &bits).unwrap();
        let mut v = std::fs::read(&p2).unwrap();
        let fver = v.windows(4).position(|w| w == b"FVER").unwrap();
        v[fver + 4..fver + 12].copy_from_slice(&u64::MAX.to_be_bytes());
        std::fs::write(&p2, &v).unwrap();
        assert!(probe(&p2).is_err()); // no sound data reachable, but no hang either
        for p in [p, p2] {
            std::fs::remove_file(p).ok();
        }
    }

    #[test]
    fn names_and_rates() {
        assert_eq!(dsd_name(DSD64), "DSD64");
        assert_eq!(dsd_name(DSD64 * 4), "DSD256");
        assert_eq!(dsd_name(3_072_000), "DSD64");
        assert_eq!(pcm_rate(3_072_000), 192_000);
        for r in [DSD64, DSD64 * 2, DSD64 * 4, DSD64 * 8, 3_072_000, 6_144_000] {
            Converter::new(r, 2).unwrap();
        }
    }
}
