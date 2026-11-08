//! Enhancers for lossy sources (MP3, AAC, Vorbis, Opus).
//!
//! What a perceptual codec threw away is gone; nothing here pretends otherwise. Two things are
//! still worth fixing, and both switch on only for lossy tracks — lossless files keep the
//! bit-perfect path:
//!
//! * **Headroom.** The codec rebuilds the waveform from quantised frequency bands, so the
//!   decoded peaks overshoot the original — commonly 1–3 dB past full scale on loud masters.
//!   Those overs get hard-clipped at the output, an audible crackle. We measure the actual
//!   decoded peak of the whole track and lower the level by exactly that much, nothing more.
//! * **High-frequency restoration.** Encoders low-pass the signal (≈16 kHz at 128 kbps, ≈19–20
//!   kHz at 320) — the "dull top" of MP3. We find that cutoff in the spectrum and synthesise
//!   the missing octave from the one below it, the way codec SBR or Sony's DSEE do: an even
//!   (full-wave) nonlinearity maps each component f in [fc/2, fc] to 2f in [fc, 2fc], and the
//!   result is filtered to the empty band only and level-matched to the band below.
//!   Plausible air, not recovered information — so it is a separate switch.

use std::f64::consts::PI;

use crate::decode::Track;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LossySettings {
    pub headroom: bool,
    pub restore_highs: bool,
}

impl LossySettings {
    pub fn any(&self) -> bool {
        self.headroom || self.restore_highs
    }
}

/// What the analysis decided for one lossy track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LossyPlan {
    /// Linear gain applied first (≤ 1; 1 = the track has no overs).
    pub gain: f64,
    /// The gain comes from measuring the whole track (vs. a safe default while still decoding).
    pub gain_measured: bool,
    /// Codec low-pass found in the spectrum (Hz) — restoration fills from here up.
    pub restore_from_hz: Option<f64>,
}

impl LossyPlan {
    pub fn gain_db(&self) -> f64 {
        20.0 * self.gain.log10()
    }
}

/// Peak ceiling after the headroom gain: −0.3 dBFS leaves room for the resampler's and DACs'
/// own intersample overshoot.
const CEILING: f64 = 0.966;
/// Used when a stream is still downloading and the true peak isn't known yet.
const DEFAULT_HEADROOM_DB: f64 = -3.0;

/// Decide what to do for `track`. None = nothing needed (lossless, settings off, or a lossy
/// track that neither clips nor has a cutoff) — the player then keeps the untouched path.
pub fn plan(track: &Track, settings: &LossySettings) -> Option<LossyPlan> {
    if !track.lossy || !settings.any() {
        return None;
    }
    let restore_from_hz = if settings.restore_highs { detect_cutoff(track) } else { None };
    // Restored highs add a little energy on top: aim a bit lower then.
    let ceiling = if restore_from_hz.is_some() { CEILING * 0.94 } else { CEILING };
    let (gain, gain_measured) = if !settings.headroom {
        (1.0, true)
    } else if track.is_complete() {
        let peak = track.peak() as f64;
        (if peak > ceiling { ceiling / peak } else { 1.0 }, true)
    } else {
        (10f64.powf(DEFAULT_HEADROOM_DB / 20.0), false)
    };
    if gain >= 1.0 && restore_from_hz.is_none() {
        return None;
    }
    Some(LossyPlan { gain, gain_measured, restore_from_hz })
}

// ---------------------------------------------------------------------------------------------
// Cutoff detection
// ---------------------------------------------------------------------------------------------

const FFT: usize = 4096;

/// The encoder's low-pass, if the spectrum shows one: a cliff of 25+ dB between 9 kHz and
/// just under Nyquist. Averages up to 64 windows spread over what is decoded so far.
pub fn detect_cutoff(track: &Track) -> Option<f64> {
    let fs = track.sample_rate as f64;
    let frames = track.ready_frames();
    if frames < FFT * 4 {
        return None;
    }
    let window: Vec<f64> = (0..FFT).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f64 / FFT as f64).cos()).collect();
    let mut power = vec![0.0f64; FFT / 2];
    let (mut re, mut im) = (vec![0.0; FFT], vec![0.0; FFT]);
    let (start, end) = (frames / 20, frames - frames / 20 - FFT);
    let count = 64.min((end - start) / FFT).max(1);
    let mut used = 0;
    for w in 0..count {
        let at = start + (end - start) * w / count;
        let mut energy = 0.0;
        for i in 0..FFT {
            let f = at + i;
            let mono = (0..track.channels).map(|c| track.sample(f, c) as f64).sum::<f64>() / track.channels as f64;
            energy += mono * mono;
            re[i] = mono * window[i];
            im[i] = 0.0;
        }
        if energy / (FFT as f64) < 1e-7 {
            continue; // silence tells nothing about the codec
        }
        fft(&mut re, &mut im);
        for (k, p) in power.iter_mut().enumerate() {
            *p += re[k] * re[k] + im[k] * im[k];
        }
        used += 1;
    }
    if used < 4 {
        return None;
    }
    cutoff_in_spectrum(&power, fs)
}

/// Find the cliff in an averaged power spectrum (bins 0..N/2 of an N-point FFT at `fs`).
fn cutoff_in_spectrum(power: &[f64], fs: f64) -> Option<f64> {
    let bin_hz = fs / (2.0 * power.len() as f64);
    let db: Vec<f64> = power.iter().map(|p| 10.0 * (p + 1e-30).log10()).collect();
    let bins = |hz: f64| (hz / bin_hz).round() as usize;
    let mean = |a: usize, b: usize| db[a..b].iter().sum::<f64>() / (b - a) as f64;
    let (w, gap) = (bins(600.0).max(2), bins(150.0).max(1));
    let lo = bins(9_000.0);
    let hi = bins((fs / 2.0) * 0.97).min(db.len() - w - gap);
    let (mut best, mut at) = (0.0, 0);
    for k in lo.max(w)..hi {
        let drop = mean(k - w, k) - mean(k + gap, k + gap + w);
        if drop > best {
            (best, at) = (drop, k);
        }
    }
    let hz = at as f64 * bin_hz;
    // Content running to ~20 kHz (or close to Nyquist) has nothing audible left to restore.
    (best >= 25.0 && hz < 20_000.0 && hz < fs / 2.0 * 0.9).then_some(hz)
}

/// In-place iterative radix-2 FFT. `re.len()` must be a power of two.
fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * PI / len as f64;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0, 0.0);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let (tr, ti) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                (cr, ci) = (cr * wr - ci * wi, cr * wi + ci * wr);
            }
        }
        len <<= 1;
    }
}

// ---------------------------------------------------------------------------------------------
// Processing
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    /// RBJ low/high-pass.
    fn new(high: bool, freq: f64, q: f64, fs: f64) -> Self {
        let w0 = 2.0 * PI * freq.min(fs * 0.49) / fs;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        let (b0, b1) = if high { ((1.0 + cos) / 2.0, -(1.0 + cos)) } else { ((1.0 - cos) / 2.0, 1.0 - cos) };
        Self { b0: b0 / a0, b1: b1 / a0, b2: b0 / a0, a1: -2.0 * cos / a0, a2: (1.0 - alpha) / a0, z1: 0.0, z2: 0.0 }
    }

    #[inline]
    fn run(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    fn reset(&mut self) {
        (self.z1, self.z2) = (0.0, 0.0);
    }
}

/// Butterworth Q values for a 4th-order section pair.
const BW4: [f64; 2] = [0.541_196, 1.306_563];

/// Level of the synthesised octave relative to the octave below it. Real recordings fall
/// off roughly 6–12 dB per octave up there; −9 dB sits in the middle and never sounds hissy.
const RESTORE_LEVEL_DB: f64 = -9.0;

#[derive(Clone)]
struct ChannelRestorer {
    band: [Biquad; 2],  // HP fc/2, LP fc: the source octave
    clean: [Biquad; 4], // HP4 at fc (keeps the real band untouched), LP2 ×2 near 2fc
    env_src: f64,
    env_gen: f64,
    gain: f64,
}

pub struct Lossy {
    gain: f64,
    restore: Vec<ChannelRestorer>,
    /// One-pole coefficient for the ~20 ms envelopes.
    env_k: f64,
    /// Slower smoothing of the applied gain (~60 ms), so it never flutters.
    gain_k: f64,
    target: f64,
}

impl Lossy {
    pub fn new(plan: &LossyPlan, fs: f64, channels: usize) -> Self {
        let restore = match plan.restore_from_hz {
            Some(fc) => {
                let top = (2.0 * fc).min(fs * 0.47);
                let ch = ChannelRestorer {
                    band: [Biquad::new(true, fc / 2.0, 0.707, fs), Biquad::new(false, fc, 0.707, fs)],
                    clean: [
                        Biquad::new(true, fc, BW4[0], fs),
                        Biquad::new(true, fc, BW4[1], fs),
                        Biquad::new(false, top, 0.707, fs),
                        Biquad::new(false, top, 0.707, fs),
                    ],
                    env_src: 0.0,
                    env_gen: 0.0,
                    gain: 0.0,
                };
                vec![ch; channels]
            }
            None => Vec::new(),
        };
        Self {
            gain: plan.gain,
            restore,
            env_k: 1.0 - (-1.0 / (0.020 * fs)).exp(),
            gain_k: 1.0 - (-1.0 / (0.060 * fs)).exp(),
            target: 10f64.powf(RESTORE_LEVEL_DB / 20.0),
        }
    }

    pub fn reset(&mut self) {
        for c in &mut self.restore {
            c.band.iter_mut().chain(c.clean.iter_mut()).for_each(Biquad::reset);
            (c.env_src, c.env_gen, c.gain) = (0.0, 0.0, 0.0);
        }
    }

    pub fn process(&mut self, interleaved: &mut [f64], channels: usize) {
        if self.gain != 1.0 {
            interleaved.iter_mut().for_each(|v| *v *= self.gain);
        }
        if self.restore.is_empty() {
            return;
        }
        let (ek, gk, target) = (self.env_k, self.gain_k, self.target);
        for frame in interleaved.chunks_exact_mut(channels) {
            for (x, c) in frame.iter_mut().zip(self.restore.iter_mut()) {
                let hp = c.band[0].run(*x);
                let band = c.band[1].run(hp);
                // Full-wave rectification: every component f becomes 2f (plus sums/differences,
                // which the high-pass below removes when they fall under fc).
                let mut h = band.abs();
                for f in &mut c.clean {
                    h = f.run(h);
                }
                c.env_src += ek * (band.abs() - c.env_src);
                c.env_gen += ek * (h.abs() - c.env_gen);
                let want = if c.env_gen > 1e-9 { (target * c.env_src / c.env_gen).min(8.0) } else { 0.0 };
                c.gain += gk * (want - c.gain);
                *x += c.gain * h;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::TrackMeta;

    /// White noise brick-walled at `cut` Hz in the frequency domain, like an encoder's low-pass.
    fn lowpassed_noise(fs: u32, cut: f64, secs: f64) -> Vec<f32> {
        let n = (fs as f64 * secs) as usize / FFT * FFT;
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5
        };
        let mut out = Vec::with_capacity(n * 2);
        for _ in 0..n / FFT {
            let (mut re, mut im): (Vec<f64>, Vec<f64>) = ((0..FFT).map(|_| rnd()).collect(), vec![0.0; FFT]);
            fft(&mut re, &mut im);
            let k_cut = (cut / fs as f64 * FFT as f64) as usize;
            for k in 0..FFT {
                let kk = k.min(FFT - k);
                if kk > k_cut {
                    re[k] = 0.0;
                    im[k] = 0.0;
                }
            }
            // Inverse FFT via conjugation.
            im.iter_mut().for_each(|v| *v = -*v);
            fft(&mut re, &mut im);
            for v in &re {
                let s = (v / FFT as f64 * 0.5) as f32;
                out.extend_from_slice(&[s, s]);
            }
        }
        out
    }

    fn track(samples: &[f32], fs: u32, lossy: bool) -> Track {
        let mut t = Track::from_samples(fs, 2, None, samples, TrackMeta::default());
        t.lossy = lossy;
        t
    }

    #[test]
    fn finds_mp3_style_cutoff() {
        for cut in [16_000.0, 19_000.0] {
            let t = track(&lowpassed_noise(44_100, cut, 4.0), 44_100, true);
            let found = detect_cutoff(&t).expect("cutoff");
            assert!((found - cut).abs() < 300.0, "expected ~{cut}, found {found}");
        }
    }

    #[test]
    fn full_band_audio_has_no_cutoff() {
        let t = track(&lowpassed_noise(44_100, 22_050.0, 4.0), 44_100, true);
        assert_eq!(detect_cutoff(&t), None);
    }

    #[test]
    fn headroom_matches_measured_overs() {
        let mut s = lowpassed_noise(44_100, 22_050.0, 2.0);
        s[1000] = 1.4; // a +2.9 dB over
        let t = track(&s, 44_100, true);
        let p = plan(&t, &LossySettings { headroom: true, restore_highs: false }).unwrap();
        assert!(p.gain_measured);
        assert!((p.gain * 1.4 - CEILING).abs() < 1e-3, "gain {}", p.gain);
    }

    #[test]
    fn lossless_and_clean_tracks_stay_untouched() {
        let on = LossySettings { headroom: true, restore_highs: true };
        let clean = lowpassed_noise(44_100, 22_050.0, 2.0); // peaks well under full scale
        assert_eq!(plan(&track(&clean, 44_100, true), &on), None);
        let mut hot = clean.clone();
        hot[10] = 1.5;
        assert_eq!(plan(&track(&hot, 44_100, false), &on), None, "lossless must stay bit-perfect");
    }

    #[test]
    fn restoration_fills_only_above_the_cutoff() {
        let fs = 44_100;
        let src = lowpassed_noise(fs, 16_000.0, 4.0);
        let t = track(&src, fs, true);
        let p = plan(&t, &LossySettings { headroom: false, restore_highs: true }).unwrap();
        let fc = p.restore_from_hz.unwrap();
        let mut x: Vec<f64> = src.iter().map(|&v| v as f64).collect();
        Lossy::new(&p, fs as f64, 2).process(&mut x, 2);

        let band_db = |data: &[f64], lo: f64, hi: f64| {
            let (mut re, mut im) = (vec![0.0; FFT], vec![0.0; FFT]);
            let mut acc = vec![0.0; FFT / 2];
            for w in 4..20 {
                for i in 0..FFT {
                    re[i] = data[(w * FFT + i) * 2] * (0.5 - 0.5 * (2.0 * PI * i as f64 / FFT as f64).cos());
                    im[i] = 0.0;
                }
                fft(&mut re, &mut im);
                for k in 0..FFT / 2 {
                    acc[k] += re[k] * re[k] + im[k] * im[k];
                }
            }
            let k = |hz: f64| (hz / fs as f64 * FFT as f64) as usize;
            10.0 * (acc[k(lo)..k(hi)].iter().sum::<f64>() / (k(hi) - k(lo)) as f64).log10()
        };
        let orig: Vec<f64> = src.iter().map(|&v| v as f64).collect();
        let below = band_db(&x, 12_000.0, 15_000.0);
        let air_before = band_db(&orig, fc + 800.0, 20_000.0);
        let air_after = band_db(&x, fc + 800.0, 20_000.0);
        // The empty octave gets real energy, a sensible distance under the band below it…
        assert!(air_after > air_before + 30.0, "air {air_before:.1} → {air_after:.1} dB");
        assert!(air_after < below - 3.0 && air_after > below - 30.0, "below {below:.1}, air {air_after:.1}");
        // …and the band below the cutoff is left essentially alone.
        let below_orig = band_db(&orig, 8_000.0, 12_000.0);
        assert!((band_db(&x, 8_000.0, 12_000.0) - below_orig).abs() < 0.5);
    }
}
