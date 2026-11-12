//! Is the file what it says it is? A check of the audio itself, on the device, the way people
//! check by eye in a spectrum viewer:
//!
//! - **Upsampled.** A 96 or 192 kHz file made from a CD (or a 48 kHz master) has nothing above
//!   the old file's limit: its spectrum stops at a wall near 20–24 kHz.
//! - **Made from MP3 or AAC.** Lossy encoders throw away everything above a fixed frequency
//!   (16 kHz at 128 kbit/s, about 19 kHz at the best settings); a lossless file made from one
//!   keeps that wall.
//!   A cheaper resampler leaves no wall but images: the music below that limit reflected above
//!   it, which real music never is.
//! - **Padded.** A 24-bit file whose lowest 8 bits are always zero holds 16-bit audio.
//!
//! The wall is what tells: a real recording may also have little treble (an old tape, a quiet
//! piece), but then the level falls gradually, not by tens of dB within a kilohertz.
//!
//! Also measured: the peak level and how often the signal sits at full scale (clipping).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use serde::{Deserialize, Serialize};

use crate::Track;

/// What the check concludes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Verdict {
    /// Nothing suggests the file is anything but what it says.
    Genuine,
    /// A high-rate file made from a CD-quality one (44.1 or 48 kHz).
    Upsampled,
    /// A lossless file made from an MP3/AAC/Vorbis file.
    FromLossy,
    /// A 24-bit file that holds `bits`-bit audio.
    Padded { bits: u32 },
    /// The file itself is lossy: there is nothing to unmask (its bandwidth is still reported).
    Lossy,
    /// DSD: its noise shaping fills the top of the spectrum by design.
    Dsd,
    /// Too short or too quiet to tell.
    Inconclusive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub verdict: Verdict,
    pub sample_rate: u32,
    /// Bit depth the file declares, and the one its samples actually use (24-bit files only).
    pub bits: Option<u32>,
    pub effective_bits: Option<u32>,
    /// Where the music ends, in Hz (None: it reaches the top of the file's range). Above it
    /// the spectrum stays near its floor.
    pub cutoff_hz: Option<u32>,
    /// The music stops at a wall there (rather than fading out).
    pub wall: bool,
    /// Average spectrum, dB, `SPECTRUM_POINTS` points evenly from 0 Hz to half the sample rate.
    pub spectrum: Vec<f32>,
    /// Highest sample level, dBFS.
    pub peak_db: f32,
    /// Places where the signal stays at full scale for 3 samples or more.
    pub clipped: u32,
    /// Seconds of audio that went into the spectrum (silence is left out).
    pub analysed_secs: f32,
}

pub const SPECTRUM_POINTS: usize = 400;

/// Goes up whenever the rules change, so results saved by an older version are checked again.
pub const RULES_VERSION: u32 = 2;

/// A band this close to the floor holds no music…
const NEAR_FLOOR_DB: f32 = 15.0;
/// …nor does one this far below the music's own level (a resampler's faint images).
const BELOW_MUSIC_DB: f32 = 60.0;
/// A drop of this many dB within a few kHz is a wall, not a natural fade.
const WALL_DB: f32 = 25.0;
/// Blocks quieter than this (dBFS RMS) are left out: silence says nothing.
const SILENCE_DB: f32 = -70.0;

/// Accumulates the spectrum and sample statistics of a stream of interleaved frames.
pub struct Analyzer {
    rate: u32,
    channels: usize,
    bits: Option<u32>,
    n: usize,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Frames waiting for a whole block.
    pending: Vec<f32>,
    power: Vec<f64>,
    blocks: usize,
    buf: Vec<Complex<f32>>,
    peak: f32,
    /// Per channel: how many samples in a row have been at full scale.
    run: Vec<u32>,
    clipped: u32,
    /// 24-bit sources: samples seen, and samples with something in their lowest 8 bits.
    samples: u64,
    low_bits: u64,
}

impl Analyzer {
    pub fn new(rate: u32, channels: usize, bits: Option<u32>) -> Self {
        // About 5 Hz per bin at 44.1 kHz, the same resolution at higher rates.
        let n = (rate as usize / 8).next_power_of_two().clamp(4096, 65536);
        let fft = FftPlanner::new().plan_fft_forward(n);
        let window = (0..n).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos()).collect();
        Self {
            rate,
            channels: channels.max(1),
            bits,
            n,
            fft,
            window,
            pending: Vec::with_capacity(n * channels),
            power: vec![0.0; n / 2 + 1],
            blocks: 0,
            buf: vec![Complex::default(); n],
            peak: 0.0,
            run: vec![0; channels.max(1)],
            clipped: 0,
            samples: 0,
            low_bits: 0,
        }
    }

    /// Feed interleaved samples (whole frames).
    pub fn push(&mut self, mut data: &[f32]) {
        let ch = self.channels;
        for frame in data.chunks_exact(ch) {
            for (c, &x) in frame.iter().enumerate() {
                let a = x.abs();
                self.peak = self.peak.max(a);
                if a >= 32_767.0 / 32_768.0 {
                    self.run[c] += 1;
                    if self.run[c] == 3 {
                        self.clipped += 1;
                    }
                } else {
                    self.run[c] = 0;
                }
                if self.bits == Some(24) {
                    self.samples += 1;
                    if ((x * 8_388_608.0) as i32) & 0xFF != 0 {
                        self.low_bits += 1;
                    }
                }
            }
        }
        while !data.is_empty() {
            let want = self.n * ch - self.pending.len();
            let take = want.min(data.len() / ch * ch);
            self.pending.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.pending.len() == self.n * ch {
                self.block();
                self.pending.clear();
            }
            if take == 0 {
                break;
            }
        }
    }

    fn block(&mut self) {
        let (n, ch) = (self.n, self.channels);
        let rms = (self.pending.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>() / self.pending.len() as f64).sqrt();
        if 20.0 * rms.max(1e-12).log10() < SILENCE_DB as f64 {
            return;
        }
        for c in 0..ch {
            for i in 0..n {
                self.buf[i] = Complex::new(self.pending[i * ch + c] * self.window[i], 0.0);
            }
            self.fft.process(&mut self.buf);
            for (p, v) in self.power.iter_mut().zip(&self.buf) {
                *p += v.norm_sqr() as f64;
            }
        }
        self.blocks += 1;
    }

    pub fn finish(self) -> Report {
        let nyquist = self.rate as f32 / 2.0;
        let hz_per_bin = self.rate as f32 / self.n as f32;
        // Mean power per bin, in dB relative to a full-scale sine.
        let norm = (self.blocks.max(1) * self.channels) as f64 * (self.n as f64 * 0.25).powi(2);
        let db: Vec<f32> = self.power.iter().map(|&p| (10.0 * (p / norm).max(1e-20).log10()) as f32).collect();
        // Smoothed over about 150 Hz: single tones and bin noise don't count, bands do.
        let half = ((150.0 / hz_per_bin) as usize / 2).max(1);
        let smooth: Vec<f32> = (0..db.len())
            .map(|i| {
                let (a, b) = (i.saturating_sub(half), (i + half + 1).min(db.len()));
                db[a..b].iter().sum::<f32>() / (b - a) as f32
            })
            .collect();
        let bin = |hz: f32| ((hz / hz_per_bin) as usize).min(smooth.len() - 1);
        let mean = |a: f32, b: f32| {
            let (a, b) = (bin(a.max(0.0)), bin(b.min(nyquist)));
            (b > a).then(|| smooth[a..b].iter().sum::<f32>() / (b - a) as f32)
        };

        // The floor: what the quietest stretches above 1 kHz sit at (dither, or digital silence).
        let lo = bin(1000.0);
        let mut upper: Vec<f32> = smooth[lo..].to_vec();
        upper.sort_by(f32::total_cmp);
        let floor = upper.get(upper.len() / 20).copied().unwrap_or(-200.0);
        let music = mean(200.0, 4000.0).unwrap_or(-60.0);
        let quiet_below = (floor + NEAR_FLOOR_DB).max(music - BELOW_MUSIC_DB);
        // Where the music ends: the lowest frequency above which (nearly) everything is quiet.
        // A few loud bins up there (a pilot tone, a stray click) don't count: up to 1 % of
        // what lies above, at most 300 Hz worth.
        let mut loud_above = vec![0usize; smooth.len() + 1];
        for i in (0..smooth.len()).rev() {
            loud_above[i] = loud_above[i + 1] + usize::from(smooth[i] >= quiet_below);
        }
        let spare = ((300.0 / hz_per_bin) as usize).max(1);
        let end = (lo..smooth.len()).find(|&i| loud_above[i] <= ((smooth.len() - i) / 100).min(spare)).unwrap_or(smooth.len());
        let cutoff = end as f32 * hz_per_bin;
        // Where the file's own range ends anyway (its anti-alias filter).
        let full_band = cutoff >= nyquist * 0.93;
        let cutoff_hz = (!full_band).then(|| cutoff.round() as u32);
        // A wall: tens of dB lost within a few kHz, where the music stops.
        let wall = !full_band
            && match (mean(cutoff - 4000.0, cutoff - 1500.0), mean(cutoff + 500.0, cutoff + 3000.0)) {
                (Some(below), Some(above)) => below - above >= WALL_DB,
                _ => false,
            };

        let effective_bits = (self.bits == Some(24) && self.samples > 0).then(|| {
            // A handful of stray values (an edit, a fade done in 24 bits) don't make it 24-bit.
            if self.low_bits * 100_000 < self.samples { 16 } else { 24 }
        });

        // The spectrum for the screen (power averaged per point); the mirror test reads it too.
        let per = self.power.len() as f32 / SPECTRUM_POINTS as f32;
        let spectrum: Vec<f32> = (0..SPECTRUM_POINTS)
            .map(|k| {
                let (a, b) = ((k as f32 * per) as usize, (((k + 1) as f32 * per) as usize).max((k as f32 * per) as usize + 1).min(db.len()));
                let p = self.power[a..b].iter().sum::<f64>() / (b - a) as f64 / norm;
                ((10.0 * p.max(1e-20).log10()) as f32 * 10.0).round() / 10.0
            })
            .collect();

        let analysed_secs = (self.blocks * self.n) as f32 / self.rate as f32;
        // A high-rate file with a wall at a CD's or 48 kHz master's limit (a soft resampler
        // filter, like ffmpeg's, lets a few kHz through above it)…
        let walled = wall && self.rate > 48_000 && cutoff_hz.is_some_and(|c| c <= 28_000);
        // …or, from a cheaper resampler, no wall but a mirror image of the music above it.
        let images = if self.rate > 48_000 { mirror_image(&spectrum, self.rate) } else { None };
        let verdict = if analysed_secs < 3.0 {
            Verdict::Inconclusive
        } else {
            match cutoff_hz {
                // A lossy encoder's lowpass, whatever the file's rate (16 kHz at 128 kbit/s,
                // 17–19 at the usual VBR settings).
                Some(c) if wall && c < 19_000 => Verdict::FromLossy,
                _ if walled || images.is_some() => Verdict::Upsampled,
                // The low byte is empty whatever the spectrum looks like.
                _ if effective_bits == Some(16) => Verdict::Padded { bits: 16 },
                // Either an MP3 at its best settings (lowpass at 19.5–20 kHz) or a mastering
                // filter, which real CDs have there too: no way to tell from the spectrum alone.
                Some(c) if wall && c < 20_400 && self.rate <= 48_000 => Verdict::Inconclusive,
                // Little above the CD range, but no wall either: an old or dull recording, or
                // a 48 kHz master upsampled with a gentle filter.
                Some(c) if self.rate > 48_000 && c <= 24_500 => Verdict::Inconclusive,
                _ => Verdict::Genuine,
            }
        };
        // Above a mirror image's centre there is no music, only its reflection.
        let cutoff_hz = match images {
            Some(old) if !walled && verdict == Verdict::Upsampled => Some(old / 2),
            _ => cutoff_hz,
        };

        Report {
            verdict,
            sample_rate: self.rate,
            bits: self.bits,
            effective_bits,
            cutoff_hz,
            wall,
            spectrum,
            peak_db: 20.0 * self.peak.max(1e-10).log10(),
            clipped: self.clipped,
            analysed_secs,
        }
    }
}

/// Upsampling with a cheap or middling filter leaves images: the music below the old file's
/// limit (22.05 or 24 kHz) reflected above it, quieter but with the same shape. Real music has
/// no reason to be symmetric around exactly that frequency. Compares the 8 kHz on either side,
/// each with its overall slope taken out, and returns the old rate if they match.
fn mirror_image(spectrum: &[f32], rate: u32) -> Option<u32> {
    let nyquist = rate as f32 / 2.0;
    let step = nyquist / spectrum.len() as f32;
    if nyquist < 30_000.0 {
        return None;
    }
    let width = (8000.0 / step) as usize;
    [44_100u32, 48_000].into_iter().find(|&old| {
        let centre = old as f32 / 2.0 / step;
        let side = |sign: f32| detrend((2..width).map(|x| spectrum[(centre + sign * x as f32) as usize]).collect());
        let (below, above) = (side(-1.0), side(1.0));
        let (sb, sa) = (deviation(&below), deviation(&above));
        // Both sides need some shape to compare (a flat floor matches anything), and an image
        // has about as much as the original.
        sb >= 0.8 && sa >= 0.8 && (0.6..=1.6).contains(&(sb / sa)) && correlation(&below, &above) >= 0.75
    })
}

/// `v` minus its least-squares line.
fn detrend(v: Vec<f32>) -> Vec<f32> {
    let n = v.len() as f32;
    let (mx, my) = ((n - 1.0) / 2.0, v.iter().sum::<f32>() / n);
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for (x, y) in v.iter().enumerate() {
        sxy += (x as f32 - mx) * (y - my);
        sxx += (x as f32 - mx).powi(2);
    }
    let k = sxy / sxx;
    v.iter().enumerate().map(|(x, y)| y - (my + k * (x as f32 - mx))).collect()
}

fn deviation(v: &[f32]) -> f32 {
    let m = v.iter().sum::<f32>() / v.len() as f32;
    (v.iter().map(|x| (x - m).powi(2)).sum::<f32>() / v.len() as f32).sqrt()
}

fn correlation(a: &[f32], b: &[f32]) -> f32 {
    let (ma, mb) = (a.iter().sum::<f32>() / a.len() as f32, b.iter().sum::<f32>() / b.len() as f32);
    let (mut ab, mut aa, mut bb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        ab += (x - ma) * (y - mb);
        aa += (x - ma).powi(2);
        bb += (y - mb).powi(2);
    }
    if aa > 0.0 && bb > 0.0 { ab / (aa * bb).sqrt() } else { 0.0 }
}

/// Check one file (or CUE track) of the library. Decodes it all, off the realtime path, at
/// whatever speed the disk and CPU allow: a few seconds for a long hi-res track.
pub fn analyze(path: &Path) -> Result<Report> {
    let p = path.to_string_lossy();
    let file = crate::cue::split(&p).0;
    if crate::dsd::is_dsd_path(Path::new(file)) {
        let t = crate::open_file(path, crate::dsd::DsdMode::Pcm)?;
        return Ok(Report {
            verdict: Verdict::Dsd,
            sample_rate: t.dsd.map_or(t.sample_rate, |d| d.rate),
            bits: Some(1),
            effective_bits: None,
            cutoff_hz: None,
            wall: false,
            spectrum: Vec::new(),
            peak_db: 0.0,
            clipped: 0,
            analysed_secs: 0.0,
        });
    }
    let track = crate::open_file(path, crate::dsd::DsdMode::Pcm)?;
    let mut a = Analyzer::new(track.sample_rate, track.channels, track.bits_per_sample);
    read_all(&track, |s| a.push(s))?;
    let mut report = a.finish();
    if track.lossy {
        report.verdict = Verdict::Lossy;
    }
    Ok(report)
}

/// Every decoded sample of `track`, in order, as the decoder delivers them.
fn read_all(track: &Track, mut each: impl FnMut(&[f32])) -> Result<()> {
    let mut pos = 0;
    loop {
        pos = track.claim(pos);
        let s = track.slice(pos, 1 << 16);
        if s.is_empty() {
            if track.is_complete() && pos >= track.ready_frames() {
                break;
            }
            if !track.wait_for(pos + 1, Duration::from_secs(30)) {
                bail!("decoding stalled");
            }
            continue;
        }
        each(s);
        pos += s.len() / track.channels;
    }
    match track.error() {
        Some(e) if pos == 0 => bail!(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `secs` of stereo noise at `rate`, flat up to `top` Hz and nothing above, plus a faint
    /// broadband floor (`floor_db`, as dither or a resampler's rounding leaves).
    fn band_noise(rate: u32, secs: f32, top: f32, floor_db: f32) -> Vec<f32> {
        shaped_noise(rate, secs, |hz| if hz <= top { 1.0 } else { 0.0 }, floor_db)
    }

    /// Like `band_noise`, with the level of each frequency given by `gain`.
    fn shaped_noise(rate: u32, secs: f32, gain: impl Fn(f32) -> f32, floor_db: f32) -> Vec<f32> {
        let n = ((rate as f32 * secs) as usize).next_power_of_two();
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f32 / (1u64 << 53) as f32
        };
        let ifft = FftPlanner::new().plan_fft_inverse(n);
        let mut out = vec![0.0f32; n * 2];
        for c in 0..2 {
            let mut spec = vec![Complex::default(); n];
            for (k, s) in spec.iter_mut().enumerate().take(n / 2).skip(1) {
                let hz = k as f32 * rate as f32 / n as f32;
                *s = Complex::from_polar(gain(hz), rnd() * std::f32::consts::TAU);
            }
            ifft.process(&mut spec);
            let peak = spec.iter().map(|v| v.re.abs()).fold(0.0, f32::max);
            let floor = 10f32.powf(floor_db / 20.0);
            for i in 0..n {
                out[i * 2 + c] = spec[i].re / peak * 0.5 + (rnd() - 0.5) * 2.0 * floor;
            }
        }
        out
    }

    fn check(rate: u32, bits: Option<u32>, samples: &[f32]) -> Report {
        let mut a = Analyzer::new(rate, 2, bits);
        a.push(samples);
        a.finish()
    }

    #[test]
    fn full_band_files_are_genuine() {
        let r = check(44_100, Some(16), &band_noise(44_100, 8.0, 21_800.0, -96.0));
        assert_eq!(r.verdict, Verdict::Genuine, "{r:?}");
        let r = check(96_000, Some(24), &band_noise(96_000, 8.0, 44_000.0, -140.0));
        assert_eq!(r.verdict, Verdict::Genuine, "cutoff {:?}", r.cutoff_hz);
        assert_eq!(r.effective_bits, Some(24));
    }

    #[test]
    fn a_cd_in_a_96k_file_is_upsampled() {
        let r = check(96_000, Some(24), &band_noise(96_000, 8.0, 20_500.0, -140.0));
        assert_eq!(r.verdict, Verdict::Upsampled, "{:?}", (r.cutoff_hz, r.wall));
        let r = check(192_000, Some(24), &band_noise(192_000, 8.0, 23_500.0, -140.0));
        assert_eq!(r.verdict, Verdict::Upsampled);
    }

    /// Music-like treble: falling with frequency, and uneven from one 500 Hz band to the next.
    fn uneven(hz: f32) -> f32 {
        let band = (hz / 500.0) as u64;
        let wobble = ((band.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) % 1000) as f32 / 1000.0;
        10f32.powf(-(hz / 1000.0) * 0.15 - wobble * 0.6)
    }

    #[test]
    fn a_cheap_resampler_leaves_a_mirror_image() {
        // 44.1 kHz music to 96 kHz by straight-line interpolation: no wall, but images.
        let cd = shaped_noise(44_100, 8.0, |hz| if hz <= 20_000.0 { uneven(hz) } else { 0.0 }, -120.0);
        let frames = cd.len() / 2;
        let out_frames = frames * 96_000 / 44_100 - 1;
        let mut up = Vec::with_capacity(out_frames * 2);
        for i in 0..out_frames {
            let t = i as f64 * 44_100.0 / 96_000.0;
            let (j, f) = (t as usize, (t - t.floor()) as f32);
            for c in 0..2 {
                up.push(cd[j * 2 + c] * (1.0 - f) + cd[(j + 1).min(frames - 1) * 2 + c] * f);
            }
        }
        let r = check(96_000, Some(24), &up);
        assert_eq!(r.verdict, Verdict::Upsampled, "{:?}", (r.cutoff_hz, r.wall));
        assert_eq!(mirror_image(&r.spectrum, 96_000), Some(44_100));
        assert_eq!((r.cutoff_hz, r.wall), (Some(22_050), false));
    }

    #[test]
    fn real_treble_is_not_a_mirror() {
        let r = check(96_000, Some(24), &shaped_noise(96_000, 8.0, uneven, -140.0));
        assert_eq!(mirror_image(&r.spectrum, 96_000), None);
        assert!(!SUSPECT_FOR_TESTS.contains(&r.verdict), "{:?}", (r.verdict, r.cutoff_hz, r.wall));
    }

    const SUSPECT_FOR_TESTS: [Verdict; 3] = [Verdict::Upsampled, Verdict::FromLossy, Verdict::Padded { bits: 16 }];

    #[test]
    fn an_mp3_lowpass_gives_it_away() {
        let r = check(44_100, Some(16), &band_noise(44_100, 8.0, 16_000.0, -96.0));
        assert_eq!(r.verdict, Verdict::FromLossy, "{:?}", (r.cutoff_hz, r.wall));
        assert!(r.cutoff_hz.is_some_and(|c| (15_500..16_500).contains(&c)));
    }

    #[test]
    fn a_gentle_fade_is_not_a_wall() {
        // Treble that fades out over 10 kHz, as on an old recording: no verdict of fakery.
        let mut s = band_noise(44_100, 8.0, 21_800.0, -96.0);
        // A one-pole lowpass at 2 kHz: -6 dB per octave.
        let k = (-2.0 * std::f32::consts::PI * 2000.0 / 44_100.0).exp();
        let (mut l, mut r) = (0.0, 0.0);
        for f in s.chunks_exact_mut(2) {
            l = l * k + f[0] * (1.0 - k);
            r = r * k + f[1] * (1.0 - k);
            f[0] = l;
            f[1] = r;
        }
        let rep = check(44_100, Some(16), &s);
        assert_ne!(rep.verdict, Verdict::FromLossy, "{:?}", (rep.cutoff_hz, rep.wall));
    }

    #[test]
    fn sixteen_bits_in_a_24_bit_file() {
        let s: Vec<f32> = band_noise(96_000, 8.0, 44_000.0, -90.0).iter().map(|x| (x * 32_768.0).round() / 32_768.0).collect();
        let r = check(96_000, Some(24), &s);
        assert_eq!(r.verdict, Verdict::Padded { bits: 16 });
    }

    #[test]
    fn clipping_is_counted() {
        let mut s = band_noise(44_100, 4.0, 20_000.0, -96.0);
        for x in &mut s[1000..1020] {
            *x = 1.0;
        }
        let r = check(44_100, Some(16), &s);
        assert_eq!(r.clipped, 2, "one run on each channel");
        assert!(r.peak_db.abs() < 0.01);
        assert_eq!(r.spectrum.len(), SPECTRUM_POINTS);
    }

    #[test]
    fn too_short_to_tell() {
        let r = check(44_100, Some(16), &band_noise(44_100, 1.0, 16_000.0, -96.0));
        assert_eq!(r.verdict, Verdict::Inconclusive);
    }
}
