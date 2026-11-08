//! Streaming rational-ratio resampler: polyphase windowed-sinc FIR, f64 throughout.
//!
//! Output rate = input × L / M (reduced fraction). The prototype low-pass is a Kaiser-windowed
//! sinc designed at the virtual rate `fs_in × L`, split into L phases of `taps` coefficients.
//! Each output sample is one dot product of `taps` input samples with one phase — so the cost
//! is `taps` multiply-adds per output sample per channel, independent of L.
//!
//! Linear phase: the filter is symmetric, so every frequency is delayed equally and transients
//! keep their shape. The delay itself is compensated, so output stays aligned with the source.

use std::f64::consts::PI;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResampleQuality {
    /// Coefficients per phase. More = steeper transition band.
    pub taps: usize,
    /// Kaiser β: stop-band attenuation ≈ 8.7·β + 9 dB for this design (β=10 → ~100 dB).
    pub kaiser_beta: f64,
    /// Pass band edge as a fraction of the lower Nyquist (0.91 → flat to ~20 kHz at 44.1k).
    pub passband: f64,
}

impl ResampleQuality {
    /// Flat to 91% of Nyquist (20 kHz at 44.1k), ~100 dB down from Nyquist on.
    pub const HIGH: Self = Self { taps: 160, kaiser_beta: 10.0, passband: 0.91 };
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Zeroth-order modified Bessel function of the first kind (for the Kaiser window).
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    while term > sum * 1e-16 {
        term *= (x / (2.0 * k)) * (x / (2.0 * k));
        sum += term;
        k += 1.0;
    }
    sum
}

pub struct Resampler {
    l: usize,
    m: usize,
    taps: usize,
    channels: usize,
    /// bank[phase][k], k = 0 .. taps (reversed for a forward dot product).
    bank: Vec<Vec<f64>>,
    /// Pending input per channel, oldest first.
    buf: Vec<Vec<f64>>,
    /// Position of the next output sample, in units of 1/L input samples from buf[0].
    pos: usize,
    /// Output samples still to drop to cancel the filter's group delay.
    skip: usize,
}

/// Most polyphase branches we build (out_rate / gcd). Standard rates need at most a few
/// hundred; an odd source rate (or a forged header) could otherwise ask for millions.
pub const MAX_PHASES: usize = 8192;

/// Polyphase branches converting `in_rate` → `out_rate` needs.
pub fn phases(in_rate: u32, out_rate: u32) -> usize {
    (out_rate as u64 / gcd(in_rate as u64, out_rate as u64).max(1)) as usize
}

impl Resampler {
    /// `in_rate → out_rate` must need at most [`MAX_PHASES`] phases (see [`phases`]).
    pub fn new(in_rate: u32, out_rate: u32, channels: usize, q: ResampleQuality) -> Self {
        let g = gcd(in_rate as u64, out_rate as u64);
        let (l, m) = ((out_rate as u64 / g) as usize, (in_rate as u64 / g) as usize);
        assert!(l <= MAX_PHASES, "resampling {in_rate} → {out_rate} Hz needs {l} phases");
        let taps = q.taps;
        let n = taps * l;
        // A windowed sinc's cut-off is its −6 dB point, so put it midway through the transition
        // band (pass-band edge → lower Nyquist), in cycles per sample of the virtual rate.
        let fc = (q.passband + 1.0) / 2.0 * 0.5 * (1.0f64).min(l as f64 / m as f64) / l as f64;
        // Centre on a whole virtual sample (index n/2) so the delay is an exact integer.
        let centre = (n / 2) as f64;
        let i0_beta = bessel_i0(q.kaiser_beta);
        let mut proto: Vec<f64> = (0..n)
            .map(|i| {
                let t = i as f64 - centre;
                let sinc = if t == 0.0 { 2.0 * fc } else { (2.0 * PI * fc * t).sin() / (PI * t) };
                let r = t / centre;
                let w = bessel_i0(q.kaiser_beta * (1.0 - r * r).max(0.0).sqrt()) / i0_beta;
                sinc * w
            })
            .collect();
        // Unity DC gain per output sample: the L phases together sum to L.
        let sum: f64 = proto.iter().sum();
        proto.iter_mut().for_each(|c| *c *= l as f64 / sum);

        let bank = (0..l)
            .map(|p| (0..taps).map(|k| proto[k * l + p]).rev().collect())
            .collect();

        // Output at `pos` is centred on virtual time pos + (taps−1)·L − n/2. Priming the buffer
        // with taps/2 zeros and starting at pos = L makes that exactly the first input sample,
        // so output stays sample-aligned with the source (no group-delay offset).
        let delay_in = taps / 2;
        Self {
            l,
            m,
            taps,
            channels,
            bank,
            buf: vec![vec![0.0; delay_in]; channels],
            pos: l,
            skip: 0,
        }
    }

    pub fn ratio(&self) -> f64 {
        self.l as f64 / self.m as f64
    }

    pub fn reset(&mut self) {
        let delay_in = self.taps / 2;
        self.buf.iter_mut().for_each(|b| {
            b.clear();
            b.resize(delay_in, 0.0);
        });
        self.pos = self.l;
        self.skip = 0;
    }

    /// Feed interleaved input; appends interleaved output to `out`.
    pub fn process(&mut self, input: &[f64], out: &mut Vec<f64>) {
        for (i, s) in input.iter().enumerate() {
            self.buf[i % self.channels].push(*s);
        }
        self.drain(out);
    }

    /// Push the filter tail out at end of stream.
    pub fn flush(&mut self, out: &mut Vec<f64>) {
        let pad = vec![0.0; self.taps * self.channels];
        self.process(&pad, out);
    }

    fn drain(&mut self, out: &mut Vec<f64>) {
        let available = self.buf[0].len();
        while self.pos / self.l + self.taps <= available {
            let (start, phase) = (self.pos / self.l, self.pos % self.l);
            let coeffs = &self.bank[phase];
            if self.skip > 0 {
                self.skip -= 1;
            } else {
                for ch in 0..self.channels {
                    let x = &self.buf[ch][start..start + self.taps];
                    out.push(x.iter().zip(coeffs).map(|(a, b)| a * b).sum());
                }
            }
            self.pos += self.m;
        }
        let consumed = self.pos / self.l;
        if consumed > 0 {
            self.buf.iter_mut().for_each(|b| {
                b.drain(..consumed);
            });
            self.pos -= consumed * self.l;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Amplitude of `freq` in a mono signal (Goertzel, normalised).
    fn level(x: &[f64], freq: f64, fs: f64) -> f64 {
        let w = 2.0 * PI * freq / fs;
        let (mut s1, mut s2) = (0.0, 0.0);
        for v in x {
            let s = v + 2.0 * w.cos() * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        let power = s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2;
        2.0 * power.sqrt() / x.len() as f64
    }

    fn sine(freq: f64, fs: f64, n: usize, amp: f64) -> Vec<f64> {
        (0..n).map(|i| amp * (2.0 * PI * freq * i as f64 / fs).sin()).collect()
    }

    fn run(input: &[f64], from: u32, to: u32) -> Vec<f64> {
        let mut r = Resampler::new(from, to, 1, ResampleQuality::HIGH);
        let mut out = Vec::new();
        for chunk in input.chunks(4096) {
            r.process(chunk, &mut out);
        }
        out
    }

    #[test]
    fn length_follows_the_ratio() {
        let out = run(&vec![0.0; 44_100], 44_100, 48_000);
        assert!((out.len() as i64 - 48_000).abs() < 200, "{}", out.len());
    }

    #[test]
    fn passband_tone_keeps_its_level_across_families() {
        let x = sine(1000.0, 44_100.0, 44_100, 0.5);
        let y = run(&x, 44_100, 48_000);
        let a = level(&y[4_800..40_000], 1000.0, 48_000.0);
        assert!((20.0 * (a / 0.5).log10()).abs() < 0.01, "level {a}");
    }

    #[test]
    fn upsampling_rejects_images() {
        // 20 kHz at 44.1k ×2 → the image at 24.1 kHz must be ~100 dB down.
        let x = sine(20_000.0, 44_100.0, 44_100, 0.5);
        let y = run(&x, 44_100, 88_200);
        let wanted = level(&y[8_000..80_000], 20_000.0, 88_200.0);
        let image = level(&y[8_000..80_000], 24_100.0, 88_200.0);
        let rejection = 20.0 * (wanted / image).log10();
        assert!(rejection > 90.0, "image rejection only {rejection:.1} dB");
        assert!((20.0 * (wanted / 0.5).log10()).abs() < 0.1, "20 kHz level {wanted}");
    }

    #[test]
    fn downsampling_rejects_aliases() {
        // DXD on a 192k DAC: 352.8 → 176.4. A 100 kHz tone is above the new Nyquist (88.2k)
        // and would alias to 76.4 kHz; it must be filtered out, while 30 kHz passes untouched.
        let n = 352_800;
        let x: Vec<f64> = sine(30_000.0, 352_800.0, n, 0.25).iter()
            .zip(sine(100_000.0, 352_800.0, n, 0.25))
            .map(|(a, b)| a + b)
            .collect();
        let y = run(&x, 352_800, 176_400);
        let kept = level(&y[10_000..170_000], 30_000.0, 176_400.0);
        let alias = level(&y[10_000..170_000], 76_400.0, 176_400.0);
        assert!((20.0 * (kept / 0.25).log10()).abs() < 0.05, "30 kHz level {kept}");
        assert!(20.0 * (0.25 / alias).log10() > 90.0, "alias only {:.1} dB down", 20.0 * (0.25 / alias).log10());
    }

    #[test]
    fn output_is_time_aligned_with_input() {
        // An impulse at input sample 1000 should peak at output sample 2000 for ×2.
        let mut x = vec![0.0; 4_000];
        x[1000] = 1.0;
        let y = run(&x, 44_100, 88_200);
        let peak = y.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).unwrap().0;
        assert!((peak as i64 - 2000).abs() <= 1, "peak at {peak}");
    }
}
