//! PCM → DSD on the fly: PCM at the DoP wire rate R (176.4, 352.8 or 705.6 kHz) becomes a
//! 1-bit stream at 16·R, 16 bits per channel per wire frame, ready to be sent as DoP.
//!
//! Two stages, f64 throughout:
//!
//! **A. ×16 interpolation**, four cascaded ×2 half-band FIRs (Kaiser-windowed sinc, cut-off
//! exactly a quarter of the stage's output rate, so every second tap is zero but the centre).
//! Each runs as two polyphase branches: one is a pure delay, the other the non-zero taps,
//! folded about their symmetry. Stage k (input rate R·2^(k−1)) keeps its pass band flat to
//! 0.34·R in stage 1 (60 kHz at 176.4 kHz) and to 0.5·R in stages 2–4 (stage 1 leaves nothing
//! above), and puts the images from R·2^(k−1) − pass upwards `HALFBAND_ATTEN_DB` down.
//! Lengths follow from Kaiser's formula, rounded up to the 4m+3 a half-band needs, then
//! measured (see `HalfBand::design`): 59, 39, 27 and 23 taps (15, 10, 7 and 6 multiplies per
//! pair of outputs). Measured on the cascade: flat within ±1.2e-6 dB to 20 kHz, −8e-7 dB at
//! 0.34·R, every image of a 0.3·R tone at least 138 dB down; delay 21.56 wire frames.
//!
//! **B. The modulator**: one bit, order 5 or 7, no look-ahead. Its noise transfer function
//! NTF(z) = Π(z − z_i) / Π(z − p_k) is designed here for each (order, oversampling ratio
//! osr = DSD rate / (2 · band)):
//! - Zeros on the unit circle at ω_i = (π/osr)·ξ_i, ξ_i the roots of the Legendre polynomial
//!   P_N: the zero placement that minimises the in-band noise integral (Schreier & Temes,
//!   "Understanding Delta-Sigma Data Converters", optimised zeros).
//! - Poles maximally flat, as Schreier's `synthesizeNTF` places them: for a parameter x,
//!   p_k = b_k − √(b_k² − 1), b_k = 1 − ½·x^(2/N)·e^{j(2k−1)π/N}, reflected inside the unit circle,
//!   with x found by bisection so that |NTF(−1)| = [`H_INF`]: the out-of-band gain, which is
//!   also the largest |NTF| anywhere. Lee's rule allows about 1.5 for a stable one-bit loop;
//!   the stability rule here needs less: 1.35 for the fifth order, 1.25–1.30 for the
//!   seventh, chosen per (order, rate) (see [`H_INF`]).
//! - Realised as a chain of an integrator and (N−1)/2 resonators with one feedback coefficient
//!   per state (see `step`). A resonator's local feedback g = 2·(1 − cos ω) puts a pair of
//!   open-loop poles exactly on the unit circle at e^{±jω}: the NTF zeros, exact by
//!   construction. The feedback coefficients make the closed loop's poles the designed ones:
//!   with y = cᵀx the quantiser input, det(zI − A + a·cᵀ) = D(z), which is linear in a (matrix
//!   determinant lemma: N(z)·(1 + cᵀ(zI − A)⁻¹a) = D(z)); it is solved as an N×N system from N
//!   real equations at points around z = 1 (never via polynomial coefficients in z, where all
//!   the roots crowd near z = 1 and powers of z cancel catastrophically).
//! - The input enters every state with that state's feedback coefficient and is added at the
//!   quantiser, so the loop filter only ever sees u − v, and the signal transfer function is
//!   exactly 1 (no delay, no ripple). Fed into the first integrator alone (b = a₀), the
//!   transfer would be a maximally flat low-pass of order N whose step response overshoots by
//!   10–15 %: a full-scale step then asks the loop for a local mean beyond its stable range.
//!   Measured (without dither, 2 s runs): with the limiter at depth M, a 100 Hz square wave at
//!   depth M stayed stable to M = 0.74 (5th, H∞ 1.3) and beyond 0.76 (7th, H∞ 1.25) this way,
//!   against 0.70 and 0.68 with the input into the first integrator only.
//!
//! Level: PCM 1.0 is modulation depth [`LEVEL`] (50 %, the SACD reference level). Before the
//! loop, the interpolated signal is hard-limited to the rated depth [`MAX_DEPTH`] (inter-sample
//! peaks beyond it are cut, counted by [`Encoder::clips`]). Each state is clamped to a limit
//! (see `LIMITS` for how they were found) checked once per 16-bit word; that never happens at
//! rated levels and is counted by [`Encoder::overloads`]. Past the limiter the loop only ever
//! gets what it is rated for, so even PCM far beyond full scale doesn't overload it; the
//! clamps are the second line, should a pathological input or a bug drive a state away.
//!
//! Dither: yes, uniform, at the quantiser input, its own stream per channel (so the same input
//! on two channels gives different bits, and uncorrelated noise), re-seeded by
//! [`Encoder::reset`]. Without it the loop has idle tones: a DC input puts a line at
//! depth·fs/2 (the loop's fs/2 pattern slipping at the rate the DC sets), 37–44 dB above the
//! noise floor for DC from 1e-4 to 3e-3 PCM, and a −20 dBFS tone brings lines of 17–20 dB.
//! See [`DITHER`] for the amount and why it fades out towards the rated depth.
//!
//! Measured (`cargo test --release -p hifi-engine -- --ignored dsd_encode --nocapture`; SNR is
//! a 1 kHz sine at PCM full scale against all noise and distortion from 20 Hz to 20 kHz; the
//! linear model is σ² = 1/3 shaped by |NTF|²; noise bands in dB re that tone):
//!
//! | | H∞ | dither | SNR | at −60 dBFS | linear model | 20–40 kHz | 40–100 kHz | first failure: programme / wire noise |
//! |---|---|---|---|---|---|---|---|---|
//! | DSD64, 5th | 1.35 | 0.20 | 109.6 dB | 48.7 dB | 102.0 dB | −61.9 | −23.2 | 0.68 / 0.70 |
//! | DSD64, 7th | 1.30 | 0.18 | 119.9 dB | 58.5 dB | 111.1 dB | −50.7 | −21.0 | 0.70 / 0.62 |
//! | DSD128, 5th | 1.35 | 0.20 | 127.6 dB | 66.3 dB | 119.6 dB | −100.6 | −49.0 | 0.68 / 0.68 |
//! | DSD128, 7th | 1.25 | 0.18 | 132.7 dB | 71.9 dB | 123.2 dB | −94.5 | −28.1 | 0.74 / 0.64 |
//! | DSD256, 5th | 1.35 | 0.22 | 146.9 dB | 86.0 dB | 139.5 dB | −144.5 | −82.8 | 0.68 / 0.70 |
//! | DSD256, 7th | 1.25 | 0.18 | 160.2 dB | 100.5 dB | 152.0 dB | −157.3 | −70.7 | 0.72 / 0.64 |
//!
//! "First failure": the lowest depth (steps of 0.02 from the rated 0.58) at which a signal of
//! the class runs away in 20 s (DSD64; 10 s DSD128, 5 s DSD256) with the limiter lifted
//! there (test `stability_limit`; the classes and their margins at [`H_INF`]). At DSD256 the
//! seventh order is within ~5 dB of what f32 PCM input itself holds in 20 kHz (~165 dB).
//! Tone levels are within 1.5e-4 dB at 1 and 19 kHz; the delay is 21.56 wire frames (22).

use std::f64::consts::PI;
use std::sync::{Arc, Mutex};

use realfft::num_complex::Complex;

use crate::dsd::{DSD64, kaiser_lowpass};

type C = Complex<f64>;

/// DSD rate of the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DsdRate {
    Dsd64,
    Dsd128,
    Dsd256,
}

impl DsdRate {
    pub const ALL: [DsdRate; 3] = [DsdRate::Dsd64, DsdRate::Dsd128, DsdRate::Dsd256];

    /// 64, 128 or 256: the rate as a multiple of 44.1 kHz.
    pub fn multiple(self) -> u32 {
        match self {
            DsdRate::Dsd64 => 64,
            DsdRate::Dsd128 => 128,
            DsdRate::Dsd256 => 256,
        }
    }

    pub fn from_multiple(m: u32) -> Option<Self> {
        DsdRate::ALL.into_iter().find(|r| r.multiple() == m)
    }

    /// 1-bit samples per second per channel (44.1 kHz family only).
    pub fn hz(self) -> u32 {
        DSD64 / 64 * self.multiple()
    }

    /// The PCM rate the DAC runs at for DoP: 16 DSD bits per frame.
    pub fn wire_rate(self) -> u32 {
        self.hz() / 16
    }

    /// Band the noise shaping is optimised for.
    pub fn band_hz(self) -> f64 {
        match self {
            DsdRate::Dsd64 => 20_000.0,
            DsdRate::Dsd128 => 30_000.0,
            DsdRate::Dsd256 => 40_000.0,
        }
    }

    /// Oversampling ratio of the noise shaping: 70.56, 94.08, 141.12.
    fn osr(self) -> f64 {
        self.hz() as f64 / (2.0 * self.band_hz())
    }

    fn index(self) -> usize {
        match self {
            DsdRate::Dsd64 => 0,
            DsdRate::Dsd128 => 1,
            DsdRate::Dsd256 => 2,
        }
    }
}

/// Order of the modulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModOrder {
    Fifth,
    Seventh,
}

impl ModOrder {
    pub const ALL: [ModOrder; 2] = [ModOrder::Fifth, ModOrder::Seventh];

    /// 5 or 7.
    pub fn get(self) -> usize {
        match self {
            ModOrder::Fifth => 5,
            ModOrder::Seventh => 7,
        }
    }

    pub fn from_number(n: u32) -> Option<Self> {
        ModOrder::ALL.into_iter().find(|o| o.get() as u32 == n)
    }

    fn index(self) -> usize {
        match self {
            ModOrder::Fifth => 0,
            ModOrder::Seventh => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DsdOut {
    pub rate: DsdRate,
    pub order: ModOrder,
}

impl DsdOut {
    /// The next cheaper setting, for when the processor can't keep up: halve the rate first
    /// (keeps the order: 256 → 128 → 64), at DSD64 the seventh order gives way to the fifth, then None.
    pub fn lighter(self) -> Option<DsdOut> {
        let rate = match self.rate {
            DsdRate::Dsd256 => DsdRate::Dsd128,
            DsdRate::Dsd128 => DsdRate::Dsd64,
            DsdRate::Dsd64 => {
                return match self.order {
                    ModOrder::Seventh => Some(DsdOut { rate: DsdRate::Dsd64, order: ModOrder::Fifth }),
                    ModOrder::Fifth => None,
                };
            }
        };
        Some(DsdOut { rate, order: self.order })
    }
}

/// PCM full scale (1.0) is this modulation depth: the SACD reference level ("0 dB SACD" = 50 %).
pub const LEVEL: f64 = 0.5;

/// Out-of-band gain of the NTF (|NTF| at fs/2, its largest value), per `[order][rate]` (5th,
/// 7th; DSD64, 128, 256). Chosen for stability, not SNR: for each variant the largest on a
/// grid of 0.05 (1.20 … 1.50) that keeps both classes of test signal bounded, the limiter
/// lifted to depth m and the final dither in, for 20 s (DSD64; 10 s, 5 s) (tests
/// `h_inf_search`, `stability_limit`):
/// - Programme-like signals — sines at 20 Hz, 1 kHz and 20 kHz, a 100 Hz square, DC of both
///   signs, white noise low-passed at 20 kHz peaking at full scale, and that noise driven
///   12 dB into clipping and hard-clipped at full scale (a loud clipped master, what the
///   player's clamp ahead of the encoder makes), three seeds each: bounded at every m up to
///   0.64, the first failure at 0.66 or later, 0.06 of margin over [`MAX_DEPTH`].
/// - White noise white up to half the wire rate, three seeds: bounded up to 0.60, the first
///   failure at 0.62 or later. A smaller margin, as no programme looks like it (full-band
///   noise beyond full scale), and should it ever make a loop run away, the clamps bring it
///   back within milliseconds (test `overload_recovers_all_variants`).
///
/// What binds: the fifth order on programme (DC, the square, slow sines, clipped noise: 1.40
/// first fails at 0.64 at every rate); the seventh on wire-rate noise (one step up first fails
/// at 0.58 at DSD64, 0.60 at DSD128 and DSD256), its programme margin then 0.06–0.10 over the
/// rule's.
pub const H_INF: [[f64; 3]; 2] = [[1.35, 1.35, 1.35], [1.3, 1.25, 1.25]];

/// Rated maximum modulation depth: the modulator input is hard-limited here. PCM +1.3 dB
/// over full scale; the player's peak headroom keeps true peaks at or under full scale in
/// normal use, so depth beyond this is worth less than the in-band SNR it would cost.
pub const MAX_DEPTH: f64 = 0.58;

/// Peak of the uniform dither added at the quantiser input at silence, per `[order][rate]`:
/// for each design, the smallest amount on a grid of 0.02 at which no line persists more than
/// 11 dB above its neighbours, with the next amount up clean too (test `dither_survey`: 16 DC
/// levels from 1e-5 to 3e-2 PCM, tones at −100, −60 and −20 dBFS, silence). One step lower,
/// lines of 12.4–17 dB remain. DSD256/5 is one step above that rule: at 0.20 its worst line
/// was 10.6 dB in the survey and 11.5 dB in the silence test, too close to the 12 dB limit.
/// The quantiser input spans about ±1, hence the size.
///
/// It shrinks as 1 − (u/M)² with the input u, to nothing at the rated depth M. A constant
/// dither this size took 0.07 off the stable depth (DC alone stable to 0.68 instead of 0.75,
/// measured on an earlier fifth-order design, H∞ 1.3); tapered, the stability is as without
/// dither, and the idle tones it is for (low levels) get it in full. Squared rather than
/// linear so the noise floor hardly moves with the level: silence measures −0.4 to +1.2 dB
/// against a full-scale tone's noise (a linear taper: up to 3.7 dB). Cost against 0.06 of
/// dither (the smallest tried, tones still there): 0.4–0.9 dB of SNR.
pub const DITHER: [[f64; 3]; 2] = [[0.2, 0.2, 0.22], [0.18, 0.18, 0.18]];

// ---------------------------------------------------------------------------------------------
// Stage A: ×16 interpolation
// ---------------------------------------------------------------------------------------------

/// Image rejection each half-band stage is designed for.
const HALFBAND_ATTEN_DB: f64 = 130.0;
/// Pass band edge of each stage, × the wire rate: stage 1 is flat to 0.34·R; after it nothing
/// above 0.5·R is left that matters, so the later stages get wide transitions.
const STAGE_PASS: [f64; 4] = [0.34, 0.5, 0.5, 0.5];

/// A ×2 half-band interpolator of 4m+3 taps (gain 2, so the zero-stuffed input keeps its level).
#[derive(Clone)]
struct HalfBand {
    m: usize,
    /// The non-zero branch's distinct taps, outermost first (m+1 of them; the branch has 2m+2,
    /// symmetric, summing to exactly 1). The other branch is the centre tap: 1, a pure delay.
    e: Vec<f64>,
}

impl HalfBand {
    /// Kaiser's formula gives the length: taps ≈ (A − 8) / (2.285 · 2π · Δ), rounded up to the
    /// next 4m+3. It is an estimate (55 taps for stage 1 reach 127.6 dB, not 130), so the
    /// design then measures: for each length from there up, the window β with the deepest
    /// stop band, until one reaches [`HALFBAND_ATTEN_DB`]. Result: 59, 35, 23 and 23 taps.
    fn design(stage: usize) -> Self {
        let rate_in = (1usize << stage) as f64;
        // Pass band edge and the start of its image, in cycles per output sample.
        let pass = STAGE_PASS[stage] / (2.0 * rate_in);
        let stop = 0.5 - pass;
        let need = ((HALFBAND_ATTEN_DB - 8.0) / (14.36 * (stop - pass))).ceil() as usize;
        let mut m = need.saturating_sub(3).div_ceil(4);
        loop {
            // cos(2π·f·t) for the odd offsets t of the non-zero taps, on a grid over the stop band.
            let grid: Vec<f64> = (0..=512).map(|i| stop + (0.5 - stop) * i as f64 / 512.0).collect();
            let cos: Vec<Vec<f64>> = grid.iter().map(|f| (0..=m).map(|i| (2.0 * PI * f * (2 * (m - i) + 1) as f64).cos()).collect()).collect();
            let worst = |e: &[f64]| cos.iter().map(|c| (1.0 + 2.0 * e.iter().zip(c).map(|(a, b)| a * b).sum::<f64>()).abs() / 2.0).fold(0.0, f64::max);
            let best = (0..=56)
                .map(|i| Self::taps(m, 6.0 + 0.25 * i as f64))
                .map(|e| (worst(&e), e))
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .unwrap();
            if -20.0 * best.0.log10() >= HALFBAND_ATTEN_DB {
                return Self { m, e: best.1 };
            }
            m += 1;
        }
    }

    /// The non-zero branch of a Kaiser half-band of 4m+3 taps, normalised to sum to 1.
    fn taps(m: usize, beta: f64) -> Vec<f64> {
        let h = kaiser_lowpass(4 * m + 3, 0.25, beta);
        // Even taps are the non-zero branch (odd offsets from the centre); the others are
        // sin(kπ) ≈ 1e-17 and are left out, making them exactly zero.
        let mut e: Vec<f64> = (0..=m).map(|i| h[2 * i]).collect();
        let sum: f64 = 2.0 * e.iter().sum::<f64>();
        e.iter_mut().for_each(|v| *v /= sum);
        e
    }

    /// Input samples kept between blocks.
    fn history(&self) -> usize {
        2 * self.m + 1
    }

    /// Delay in output samples.
    fn delay(&self) -> usize {
        2 * self.m + 1
    }

    /// `buf`: history() past samples then n new ones; `out`: the 2n output samples. Four
    /// output pairs at a time, so their sums are independent (and vectorise); each is added up
    /// in the same order as one at a time would be.
    #[inline]
    fn run(&self, buf: &[f64], out: &mut [f64]) {
        let m = self.m;
        let n = out.len() / 2;
        let mut i = 0;
        while i + 4 <= n {
            let w = &buf[i..i + 2 * m + 5];
            let mut acc = [0.0f64; 4];
            for (t, e) in self.e.iter().enumerate() {
                let (a, b) = (&w[t..t + 4], &w[2 * m + 1 - t..2 * m + 5 - t]);
                for j in 0..4 {
                    acc[j] += e * (a[j] + b[j]);
                }
            }
            for j in 0..4 {
                out[2 * (i + j)] = acc[j];
                out[2 * (i + j) + 1] = w[m + 1 + j];
            }
            i += 4;
        }
        for (i, o) in out.as_chunks_mut::<2>().0.iter_mut().enumerate().skip(i) {
            // The 2m+2 inputs the outputs 2i, 2i+1 depend on, newest last.
            let (lo, hi) = buf[i..i + 2 * m + 2].split_at(m + 1);
            let mut acc = 0.0;
            for ((a, b), e) in lo.iter().zip(hi.iter().rev()).zip(&self.e) {
                acc += e * (a + b);
            }
            o[0] = acc;
            o[1] = hi[0];
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Stage B: the modulator
// ---------------------------------------------------------------------------------------------

/// Positive roots of the Legendre polynomials P5 and P7 (the zero roots are the integrator).
const LEGENDRE5: [f64; 2] = [0.5384693101056831, 0.906179845938664];
const LEGENDRE7: [f64; 3] = [0.4058451513773972, 0.7415311855993945, 0.9491079123427585];

/// Clamp limits of the states x0, p1, q1, p2, q2 (, p3, q3), per `[order][rate]`: twice the
/// largest |state| seen in 20 s (DSD64; 10 s DSD128, 5 s DSD256) of each test signal of both
/// classes (see [`H_INF`]) at the rated depth, through the interpolation, limiter and dither
/// (the test `calibration`), rounded up to two digits. With the input fed forward the states
/// carry only shaped noise, so the peaks depend little on the signal (wire-rate white noise
/// gives the largest, sines, DC and audio-band noise 40–95 % of it) and follow H∞ more than
/// the rate.
const LIMITS: [[[f64; 7]; 3]; 2] = [
    [
        [9.8e-4, 1.8e-2, 1.7e-1, 7.6e-1, 2.3, 0.0, 0.0],
        [1.1e-3, 1.8e-2, 1.7e-1, 7.7e-1, 2.3, 0.0, 0.0],
        [1.1e-3, 1.9e-2, 1.7e-1, 8.0e-1, 2.3, 0.0, 0.0],
    ],
    [
        [2.2e-6, 7.6e-5, 1.6e-3, 1.7e-2, 1.5e-1, 7.0e-1, 2.2],
        [8.2e-7, 3.7e-5, 8.7e-4, 1.2e-2, 1.2e-1, 6.1e-1, 2.0],
        [8.4e-7, 3.9e-5, 8.9e-4, 1.2e-2, 1.2e-1, 6.3e-1, 2.1],
    ],
];

fn eval_zp(zeros: &[C], poles: &[C], z: C) -> C {
    let num: C = zeros.iter().map(|q| z - q).product();
    let den: C = poles.iter().map(|p| z - p).product();
    num / den
}

/// Maximally flat poles for `zeros` with |NTF(−1)| = h_inf (see the module doc).
fn ntf_poles(zeros: &[C], h_inf: f64) -> Vec<C> {
    let n = zeros.len();
    let poles_for = |x: f64| -> Vec<C> {
        let me2 = -0.5 * x.powf(2.0 / n as f64);
        (1..=n)
            .map(|k| {
                let mb2 = 1.0 + C::from_polar(me2, (2 * k - 1) as f64 * PI / n as f64);
                let p = mb2 - (mb2 * mb2 - 1.0).sqrt();
                if p.norm() > 1.0 { 1.0 / p } else { p }
            })
            .collect()
    };
    // |NTF(−1)| grows with x from 1 (poles on the zeros) to 2^N (poles at the origin).
    let gain = |lnx: f64| eval_zp(zeros, &poles_for(lnx.exp()), C::new(-1.0, 0.0)).norm();
    let (mut lo, mut hi) = (-30.0f64, 30.0f64);
    assert!(gain(lo) < h_inf && gain(hi) > h_inf, "H∞ {h_inf} out of reach");
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        if gain(mid) > h_inf { hi = mid } else { lo = mid }
    }
    poles_for((0.5 * (lo + hi)).exp())
}

/// Solve a·x = y (Gaussian elimination, partial pivoting); small systems only.
fn solve(mut a: Vec<Vec<C>>, mut y: Vec<C>) -> Vec<C> {
    let n = y.len();
    for col in 0..n {
        let piv = (col..n).max_by(|&i, &j| a[i][col].norm().total_cmp(&a[j][col].norm())).unwrap();
        a.swap(col, piv);
        y.swap(col, piv);
        for row in col + 1..n {
            let (top, rest) = a.split_at_mut(row);
            let f = rest[0][col] / top[col][col];
            for (x, t) in rest[0][col..].iter_mut().zip(&top[col][col..]) {
                *x -= f * t;
            }
            let t = y[col];
            y[row] -= f * t;
        }
    }
    let mut x = vec![C::new(0.0, 0.0); n];
    for row in (0..n).rev() {
        let s: C = (row + 1..n).map(|k| a[row][k] * x[k]).sum();
        x[row] = (y[row] - s) / a[row][row];
    }
    x
}

/// State matrix of the open loop (no feedback), states x0, p1, q1, p2, q2, … as [`step`]
/// runs them: x0 integrates; resonator j takes the previous section's old output.
fn state_matrix(g: &[f64]) -> Vec<Vec<f64>> {
    let n = 1 + 2 * g.len();
    let mut a = vec![vec![0.0; n]; n];
    a[0][0] = 1.0;
    for (j, gj) in g.iter().enumerate() {
        let (p, q, prev) = (1 + 2 * j, 2 + 2 * j, 2 * j);
        a[p][p] = 1.0;
        a[p][q] = -gj;
        a[p][prev] += 1.0;
        // q' = q + p' (the new p): that is what keeps the resonator exactly on the circle.
        a[q][q] = 1.0 - gj;
        a[q][p] = 1.0;
        a[q][prev] += 1.0;
    }
    a
}

/// Everything about one (rate, order): built once, shared by all encoders (see [`design`]).
#[derive(Clone)]
struct Design {
    out: DsdOut,
    stages: Vec<HalfBand>,
    /// The NTF as designed (the loop holds it in its coefficients; the tests check it there).
    #[cfg(test)]
    zeros: Vec<C>,
    #[cfg(test)]
    poles: Vec<C>,
    /// Feedback coefficients as [`step`] uses them: on x0, on p_j, and on q_j (which also
    /// gets p_j's through the new p_j: kpq_j is the sum).
    k0: f64,
    kp: [f64; 3],
    kpq: [f64; 3],
    /// Resonator feedback, 2·(1 − cos ω_j).
    g: [f64; 3],
    limits: [f64; 7],
    max_depth: f64,
    /// Peak of the uniform dither at the quantiser input at silence; it is scaled by
    /// 1 − u²·taper, taper = 1/M² for the rated depth M (also when a test lifts the limiter).
    dither: f64,
    taper: f64,
    /// Delay in wire frames (exact; the interpolators alone, the loop adds none).
    latency: f64,
    #[cfg(test)]
    h_inf: f64,
}

impl Design {
    fn new(out: DsdOut) -> Self {
        Self::build(out, H_INF[out.order.index()][out.rate.index()])
    }

    fn build(out: DsdOut, h_inf: f64) -> Self {
        let n = out.order.get();
        let k = (n - 1) / 2;
        let xi: &[f64] = match out.order {
            ModOrder::Fifth => &LEGENDRE5,
            ModOrder::Seventh => &LEGENDRE7,
        };
        let omega: Vec<f64> = xi.iter().map(|x| PI / out.rate.osr() * x).collect();
        let mut g = [0.0; 3];
        for (gj, w) in g.iter_mut().zip(&omega) {
            // 2·(1 − cos ω), written so it keeps its precision for ω ≈ 0.01.
            *gj = 4.0 * (w / 2.0).sin().powi(2);
        }
        let mut zeros = vec![C::new(1.0, 0.0)];
        for w in &omega {
            zeros.extend([C::from_polar(1.0, *w), C::from_polar(1.0, -*w)]);
        }
        let poles = ntf_poles(&zeros, h_inf);

        // Feedback a with det(zI − A + a·cᵀ) = D(z), c picking the last state: at each point
        // z, N(z)·cᵀ(zI − A)⁻¹·a = D(z) − N(z), N(z) = det(zI − A) as realised (from g).
        let a = state_matrix(&g[..k]);
        let numer = |z: C| g[..k].iter().fold(z - 1.0, |v, gj| v * (z * z - (2.0 - gj) * z + 1.0));
        let denom = |z: C| poles.iter().map(|p| z - p).product::<C>();
        let (mut rows, mut rhs) = (Vec::new(), Vec::new());
        // One real point and k complex ones (2 equations each) on |z − 1| = 0.5, around the
        // roots (all within 0.25 of z = 1) but clear of A's eigenvalues on the unit circle.
        for m in 0..=k {
            let z = 1.0 + C::from_polar(0.5, PI * m as f64 / (k + 1) as f64);
            let zt: Vec<Vec<C>> = (0..n).map(|i| (0..n).map(|j| if i == j { z } else { C::new(0.0, 0.0) } - a[j][i]).collect()).collect();
            let mut c = vec![C::new(0.0, 0.0); n];
            c[n - 1] = C::new(1.0, 0.0);
            let nz = numer(z);
            let row: Vec<C> = solve(zt, c).iter().map(|v| v * nz).collect();
            let y = denom(z) - nz;
            rows.push(row.iter().map(|v| C::new(v.re, 0.0)).collect());
            rhs.push(C::new(y.re, 0.0));
            if m > 0 {
                rows.push(row.iter().map(|v| C::new(v.im, 0.0)).collect());
                rhs.push(C::new(y.im, 0.0));
            }
        }
        let a_eff: Vec<f64> = solve(rows, rhs).iter().map(|v| v.re).collect();
        let (mut kp, mut kpq) = ([0.0; 3], [0.0; 3]);
        for j in 0..k {
            kp[j] = a_eff[1 + 2 * j];
            kpq[j] = a_eff[2 + 2 * j];
        }

        let stages: Vec<HalfBand> = (0..4).map(HalfBand::design).collect();
        // Stage s delays by its taps' centre at its output rate R·2^(s+1).
        let latency = stages.iter().enumerate().map(|(s, h)| h.delay() as f64 / (2u32 << s) as f64).sum();
        Self {
            out,
            stages,
            #[cfg(test)]
            zeros,
            #[cfg(test)]
            poles,
            k0: a_eff[0],
            kp,
            kpq,
            g,
            limits: LIMITS[out.order.index()][out.rate.index()],
            max_depth: MAX_DEPTH,
            dither: DITHER[out.order.index()][out.rate.index()],
            taper: 1.0 / (MAX_DEPTH * MAX_DEPTH),
            latency,
            #[cfg(test)]
            h_inf,
        }
    }

    #[cfg(test)]
    fn ntf(&self, z: C) -> C {
        eval_zp(&self.zeros, &self.poles, z)
    }
}

/// The six designs, made on first use (a few microseconds each) and kept.
static CACHE: Mutex<Vec<Arc<Design>>> = Mutex::new(Vec::new());

fn design(out: DsdOut) -> Arc<Design> {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(d) = cache.iter().find(|d| d.out == out) {
        return d.clone();
    }
    let d = Arc::new(Design::new(out));
    cache.push(d.clone());
    d
}

/// The loop coefficients for K resonators, in registers.
struct Coefs<const K: usize> {
    k0: f64,
    kp: [f64; K],
    kpq: [f64; K],
    g: [f64; K],
}

impl<const K: usize> Coefs<K> {
    fn of(d: &Design) -> Self {
        Self { k0: d.k0, kp: std::array::from_fn(|j| d.kp[j]), kpq: std::array::from_fn(|j| d.kpq[j]), g: std::array::from_fn(|j| d.g[j]) }
    }
}

/// One channel's loop state: the integrator and K resonators.
#[derive(Clone, Copy)]
struct State<const K: usize> {
    x0: f64,
    p: [f64; K],
    q: [f64; K],
}

impl<const K: usize> State<K> {
    fn load(s: &[f64; 7]) -> Self {
        Self { x0: s[0], p: std::array::from_fn(|j| s[1 + 2 * j]), q: std::array::from_fn(|j| s[2 + 2 * j]) }
    }

    fn store(&self, s: &mut [f64; 7]) {
        s[0] = self.x0;
        for j in 0..K {
            s[1 + 2 * j] = self.p[j];
            s[2 + 2 * j] = self.q[j];
        }
    }
}

/// The state update for one sample with any output `v` (the tests drive the loop linearised,
/// v = y + e, through this): what [`step`] does with v = ±1.
#[cfg(test)]
fn update<const K: usize>(c: &Coefs<K>, s: &mut State<K>, u: f64, v: f64) {
    for j in (0..K).rev() {
        let prev = if j == 0 { s.x0 } else { s.q[j - 1] };
        let t = s.p[j] + prev - c.g[j] * s.q[j];
        s.p[j] = (t + c.kp[j] * u) - c.kp[j] * v;
        s.q[j] = (s.q[j] + t + c.kpq[j] * u) - c.kpq[j] * v;
    }
    s.x0 = (s.x0 + c.k0 * u) - c.k0 * v;
}

/// One sample: the quantiser compares y = q_K + u + dither (the input is added at the
/// quantiser) with 0, i.e. q_K with `thr` = −(u + dither) (see [`threshold`]), puts out the
/// bit v = ±1, and the loop takes it in.
///
/// Every state gets its own feedback coefficient on u − v; each resonator takes the previous
/// section's old output, so the sections can update in any order (highest first here, so the
/// old value is still there). k·v is chosen rather than multiplied (the same value exactly);
/// only a select and a subtraction separate one quantiser decision from the next input.
#[inline(always)]
fn step<const K: usize>(c: &Coefs<K>, s: &mut State<K>, u: f64, thr: f64) -> bool {
    let bit = s.q[K - 1] >= thr;
    let pick = |k: f64| if bit { k } else { -k };
    for j in (0..K).rev() {
        let prev = if j == 0 { s.x0 } else { s.q[j - 1] };
        let t = s.p[j] + prev - c.g[j] * s.q[j];
        s.p[j] = (t + c.kp[j] * u) - pick(c.kp[j]);
        s.q[j] = (s.q[j] + t + c.kpq[j] * u) - pick(c.kpq[j]);
    }
    s.x0 = (s.x0 + c.k0 * u) - pick(c.k0);
    bit
}

/// Next dither value, uniform in ±`scale`·2^31 (a 64-bit LCG, Knuth's MMIX constants; its top
/// 32 bits are random enough to break up limit cycles, and it costs two integer operations).
/// Run inline, its serial multiply-add overlaps the loop's own chain and costs next to nothing
/// (a separate pass ahead of the loop measured slower).
#[inline(always)]
fn dither(x: &mut u64, scale: f64) -> f64 {
    *x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
    ((*x >> 32) as i32) as f64 * scale
}

/// The quantiser threshold for input `u`: −(u + dither), the dither full at silence and
/// shrinking as 1 − (u/M)² to nothing at the rated depth M (see [`DITHER`]).
#[inline(always)]
fn threshold(d: &Design, u: f64, rng: &mut u64, scale: f64) -> f64 {
    -(u + dither(rng, scale) * (1.0 - u * u * d.taper))
}

/// Dither generator start for channel `c`: a different stream per channel.
fn seed(c: usize) -> u64 {
    0x2545_F491_4F6C_DD1D ^ (c as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// Modulate `words` words for L channels in step: their loops are independent chains, which
/// the CPU runs side by side. Returns how many state clamps acted.
#[allow(clippy::neg_cmp_op_on_partial_ord)] // `!(x <= lim)`: NaN must fail the check too
fn run<const K: usize, const L: usize>(d: &Design, state: &mut [[f64; 7]], rng: &mut [u64], u: &[Vec<f64>], out: &mut [Vec<u16>], words: usize) -> u64 {
    let c = Coefs::<K>::of(d);
    let lim = d.limits;
    let scale = d.dither / 2_147_483_648.0;
    let mut x: [u64; L] = std::array::from_fn(|l| rng[l]);
    let mut st: [State<K>; L] = std::array::from_fn(|l| State::load(&state[l]));
    let us: [&[f64]; L] = std::array::from_fn(|l| &u[l][..16 * words]);
    let mut overloads = 0;
    for i in 0..words {
        let mut word = [0u16; L];
        for s in 0..16 {
            for l in 0..L {
                let ui = us[l][16 * i + s];
                let bit = step(&c, &mut st[l], ui, threshold(d, ui, &mut x[l], scale));
                word[l] = (word[l] << 1) | bit as u16;
            }
        }
        // Overload check once a word; NaN fails it too.
        let mut over = false;
        for s in &st {
            over |= !(s.x0.abs() <= lim[0]);
            for j in 0..K {
                over |= !(s.p[j].abs() <= lim[1 + 2 * j]) | !(s.q[j].abs() <= lim[2 + 2 * j]);
            }
        }
        if over {
            for s in &mut st {
                overloads += clamp(&mut s.x0, lim[0]);
                for j in 0..K {
                    overloads += clamp(&mut s.p[j], lim[1 + 2 * j]) + clamp(&mut s.q[j], lim[2 + 2 * j]);
                }
            }
        }
        for l in 0..L {
            out[l][i] = word[l];
        }
    }
    for (s, dst) in st.iter().zip(state.iter_mut()) {
        s.store(dst);
    }
    rng.copy_from_slice(&x);
    overloads
}

/// Clamp a state that is past its limit (NaN becomes 0); 1 if it acted.
#[cold]
fn clamp(x: &mut f64, lim: f64) -> u64 {
    if x.abs() <= lim {
        return 0;
    }
    *x = if x.is_nan() { 0.0 } else { x.clamp(-lim, lim) };
    1
}

// ---------------------------------------------------------------------------------------------
// The encoder
// ---------------------------------------------------------------------------------------------

/// Wire frames per inner block: the buffers of a channel (~60 KB) stay in the L1/L2 cache.
const CHUNK: usize = 256;

/// PCM → DoP-ready DSD for any number of channels, each encoded on its own.
pub struct Encoder {
    design: Arc<Design>,
    channels: usize,
    /// Per channel, the input of each interpolation stage: its history, then a block.
    bufs: Vec<[Vec<f64>; 4]>,
    /// Per channel, the modulator input of a block (16·CHUNK) and its words.
    u: Vec<Vec<f64>>,
    wbuf: Vec<Vec<u16>>,
    state: Vec<[f64; 7]>,
    /// Per channel, the dither generator.
    rng: Vec<u64>,
    overloads: u64,
    clips: u64,
}

impl Encoder {
    /// The design is computed once per (rate, order) and cached.
    pub fn new(out: DsdOut, channels: usize) -> Self {
        Self::with_design(design(out), channels)
    }

    fn with_design(design: Arc<Design>, channels: usize) -> Self {
        assert!(channels > 0);
        let bufs = (0..channels).map(|_| std::array::from_fn(|s| vec![0.0; design.stages[s].history() + (CHUNK << s)])).collect();
        Self {
            channels,
            bufs,
            u: vec![vec![0.0; 16 * CHUNK]; channels],
            wbuf: vec![vec![0; CHUNK]; channels],
            state: vec![[0.0; 7]; channels],
            rng: (0..channels).map(seed).collect(),
            overloads: 0,
            clips: 0,
            design,
        }
    }

    pub fn out(&self) -> DsdOut {
        self.design.out
    }

    /// Forget all history (a seek): filters and modulator state as new.
    pub fn reset(&mut self) {
        for b in &mut self.bufs {
            b.iter_mut().for_each(|v| v.fill(0.0));
        }
        self.state.fill([0.0; 7]);
        for (c, x) in self.rng.iter_mut().enumerate() {
            *x = seed(c);
        }
        self.overloads = 0;
        self.clips = 0;
    }

    /// `pcm`: interleaved frames at `out.rate.wire_rate()`, 1.0 = PCM full scale. Appends exactly one
    /// 16-bit word per input sample to `words`, interleaved the same way: the 16 DSD bits that frame
    /// becomes, oldest bit in the MSB. One frame in, one frame out, from the very first call (the
    /// filters' latency is inside).
    pub fn process(&mut self, pcm: &[f32], words: &mut Vec<u16>) {
        let ch = self.channels;
        assert_eq!(pcm.len() % ch, 0, "whole frames only");
        words.reserve(pcm.len());
        let d = &*self.design;
        let k = (d.out.order.get() - 1) / 2;
        for block in pcm.chunks(CHUNK * ch) {
            let n = block.len() / ch;
            let h0 = d.stages[0].history();
            for c in 0..ch {
                let bufs = &mut self.bufs[c];
                for (dst, f) in bufs[0][h0..h0 + n].iter_mut().zip(block.chunks_exact(ch)) {
                    // A NaN or infinity would stay in the filters' history: treat it as silence.
                    let v = f[c] as f64;
                    *dst = if v.is_finite() { v } else { 0.0 };
                }
                self.clips += interpolate(d, bufs, &mut self.u[c], n);
            }
            let mut c0 = 0;
            while c0 < ch {
                // Two channels per pass: twice as fast as one at a time for the fifth order
                // (latency-bound), a few % for the seventh (more work per sample).
                let lanes = (ch - c0).min(2);
                let (st, rng) = (&mut self.state[c0..c0 + lanes], &mut self.rng[c0..c0 + lanes]);
                let (u, w) = (&self.u[c0..c0 + lanes], &mut self.wbuf[c0..c0 + lanes]);
                self.overloads += match (k, lanes) {
                    (2, 2) => run::<2, 2>(d, st, rng, u, w, n),
                    (2, _) => run::<2, 1>(d, st, rng, u, w, n),
                    (_, 2) => run::<3, 2>(d, st, rng, u, w, n),
                    _ => run::<3, 1>(d, st, rng, u, w, n),
                };
                c0 += lanes;
            }
            for f in 0..n {
                words.extend(self.wbuf.iter().map(|w| w[f]));
            }
        }
    }

    /// The delay through the encoder in wire frames, rounded up.
    pub fn latency_frames(&self) -> usize {
        self.design.latency.ceil() as usize
    }

    /// How many times the overload protection has acted since new/reset (0 in normal operation).
    pub fn overloads(&self) -> u64 {
        self.overloads
    }

    /// How many modulator input samples the limiter cut back to the rated depth since
    /// new/reset: inter-sample peaks of PCM beyond +1.3 dB. Not an overload: the loop
    /// never sees more than it is rated for.
    pub fn clips(&self) -> u64 {
        self.clips
    }
}

/// Run the four stages for one channel: `bufs[0]` holds the history, then n new samples; the
/// modulator input (16·n samples, level applied, limited) goes to `u`. Returns the clips.
fn interpolate(d: &Design, bufs: &mut [Vec<f64>; 4], u: &mut [f64], n: usize) -> u64 {
    for s in 0..4 {
        let st = &d.stages[s];
        let (h, len) = (st.history(), n << s);
        if s < 3 {
            let (a, b) = bufs.split_at_mut(s + 1);
            let hn = d.stages[s + 1].history();
            st.run(&a[s][..h + len], &mut b[0][hn..hn + 2 * len]);
        } else {
            st.run(&bufs[s][..h + len], &mut u[..2 * len]);
        }
        bufs[s].copy_within(len..len + h, 0);
    }
    let m = d.max_depth;
    let mut clips = 0;
    for v in &mut u[..16 * n] {
        let s = LEVEL * *v;
        let c = s.clamp(-m, m);
        clips += (c != s) as u64;
        *v = c;
    }
    clips
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsd::kaiser_beta;

    fn all() -> Vec<DsdOut> {
        DsdRate::ALL.iter().flat_map(|&rate| ModOrder::ALL.map(|order| DsdOut { rate, order })).collect()
    }

    fn name(out: DsdOut) -> String {
        format!("DSD{}/{}", out.rate.multiple(), out.order.get())
    }

    fn dsd64(order: ModOrder) -> DsdOut {
        DsdOut { rate: DsdRate::Dsd64, order }
    }

    fn db(power_ratio: f64) -> f64 {
        10.0 * power_ratio.log10()
    }

    // ---- spectra ------------------------------------------------------------------------------

    /// Kaiser β of the analysis window: side lobes some 300 dB down, far below any floor here.
    const BETA: f64 = 34.0;
    /// Bins either side of a tone counted as the tone (the main lobe of β = 34 is ±11 bins).
    const LOBE: usize = 16;

    fn window(n: usize) -> Arc<Vec<f64>> {
        static CACHE: Mutex<Vec<(usize, Arc<Vec<f64>>)>> = Mutex::new(Vec::new());
        let mut cache = CACHE.lock().unwrap();
        if let Some((_, w)) = cache.iter().find(|(len, _)| *len == n) {
            return w.clone();
        }
        let i0b = crate::dsd::bessel_i0(BETA);
        let w: Arc<Vec<f64>> = Arc::new(
            (0..n)
                .map(|i| {
                    let r = 2.0 * i as f64 / n as f64 - 1.0;
                    crate::dsd::bessel_i0(BETA * (1.0 - r * r).max(0.0).sqrt()) / i0b
                })
                .collect(),
        );
        cache.push((n, w.clone()));
        w
    }

    /// One-sided spectrum of `x` (length a power of two) through the window: bin k holds the
    /// power of what falls in it, so a sine of amplitude A sums to A²/2 over its main lobe and
    /// white noise to its variance over the band.
    struct Spectrum {
        fs: f64,
        n: usize,
        x: Vec<C>,
        p: Vec<f64>,
    }

    impl Spectrum {
        /// `remove_mean`: take out the (window-weighted) mean first, so a DC offset leaves no
        /// line at the bottom of the band.
        fn new(x: &[f64], fs: f64, remove_mean: bool) -> Self {
            let n = x.len();
            assert!(n.is_power_of_two());
            let w = window(n);
            let mean = if remove_mean { x.iter().zip(w.iter()).map(|(a, b)| a * b).sum::<f64>() / w.iter().sum::<f64>() } else { 0.0 };
            let sw2: f64 = w.iter().map(|v| v * v).sum();
            let mut t: Vec<f64> = x.iter().zip(w.iter()).map(|(a, b)| (a - mean) * b).collect();
            let fft = realfft::RealFftPlanner::<f64>::new().plan_fft_forward(n);
            let mut spec = fft.make_output_vec();
            fft.process(&mut t, &mut spec).unwrap();
            let scale = 2.0 / (n as f64 * sw2);
            let p = spec.iter().map(|c| c.norm_sqr() * scale).collect();
            Self { fs, n, x: spec, p }
        }

        fn bin(&self, f: f64) -> usize {
            (f * self.n as f64 / self.fs).round() as usize
        }

        /// Amplitude of the tone at `f`.
        fn tone(&self, f: f64) -> f64 {
            let k = self.bin(f);
            (2.0 * self.p[k - LOBE..=k + LOBE].iter().sum::<f64>()).sqrt()
        }

        /// Phase of the (on-bin) sine at `f` at the first sample: sin(ωn + φ) → φ.
        fn phase(&self, f: f64) -> f64 {
            self.x[self.bin(f)].arg() + PI / 2.0
        }

        /// Power in `lo..=hi` Hz, the main lobes of the tones in `skip` left out.
        fn noise(&self, lo: f64, hi: f64, skip: &[f64]) -> f64 {
            let skip: Vec<usize> = skip.iter().map(|&f| self.bin(f)).collect();
            (self.bin(lo).max(1)..=self.bin(hi)).filter(|k| skip.iter().all(|s| k.abs_diff(*s) > LOBE)).map(|k| self.p[k]).sum()
        }

        /// For each bin k in `lo..=hi` Hz (tones in `skip` left out): how far (dB) the power
        /// over k ± 4 bins stands above the median of the same sums 20–80 bins either side.
        /// Sums over 9 bins rather than single bins: a single bin of Gaussian noise exceeds
        /// its median by 12 dB about once in 60,000, a 9-bin sum practically never.
        fn lines(&self, lo: f64, hi: f64, skip: &[f64]) -> Vec<(usize, f64)> {
            let s9: Vec<f64> = (0..self.p.len()).map(|k| self.p[k.saturating_sub(4)..(k + 5).min(self.p.len())].iter().sum()).collect();
            let skip: Vec<usize> = skip.iter().map(|&f| self.bin(f)).collect();
            let mut near = Vec::with_capacity(128);
            let mut out = Vec::new();
            for k in self.bin(lo).max(1)..=self.bin(hi) {
                if skip.iter().any(|s| k.abs_diff(*s) <= LOBE + 4) {
                    continue;
                }
                near.clear();
                for d in 20..=80 {
                    for j in [k.wrapping_sub(d), k + d] {
                        if j >= 1 && j < s9.len() && skip.iter().all(|s| j.abs_diff(*s) > LOBE + 4) {
                            near.push(s9[j]);
                        }
                    }
                }
                let mid = near.len() / 2;
                let median = *near.select_nth_unstable_by(mid, |a, b| a.total_cmp(b)).1;
                out.push((k, db(s9[k] / median)));
            }
            out
        }
    }

    /// The strongest line standing in two spectra of consecutive stretches (an idle tone
    /// persists; the modulator's noise has bursts that don't): its frequency and the lesser of
    /// its two heights above the neighbours (dB).
    fn persistent_line(a: &Spectrum, b: &Spectrum, lo: f64, hi: f64, skip: &[f64]) -> (f64, f64) {
        let (la, lb) = (a.lines(lo, hi, skip), b.lines(lo, hi, skip));
        // A tone's lobe peak can shift by a bin between stretches: compare k with b's best of k ± 2.
        let best_b: std::collections::HashMap<usize, f64> = lb.iter().copied().collect();
        la.iter()
            .map(|&(k, ha)| {
                let hb = (k.saturating_sub(2)..=k + 2).filter_map(|j| best_b.get(&j)).cloned().fold(f64::NEG_INFINITY, f64::max);
                (k as f64 * a.fs / a.n as f64, ha.min(hb))
            })
            .fold((0.0, f64::NEG_INFINITY), |m, v| if v.1 > m.1 { v } else { m })
    }

    /// The bits of channel `c` of interleaved words, as ±1.
    fn bits(words: &[u16], ch: usize, c: usize) -> Vec<f64> {
        words.iter().skip(c).step_by(ch).flat_map(|w| (0..16).rev().map(move |b| if (w >> b) & 1 == 1 { 1.0 } else { -1.0 })).collect()
    }

    /// FFT length for a measurement at `rate`: 0.37 s, bins of 2.69 Hz.
    fn fft_len(rate: DsdRate) -> usize {
        (1 << 20) * rate.multiple() as usize / 64
    }

    /// A frequency near `f` on an exact bin of the analysis FFT.
    fn on_bin(rate: DsdRate, f: f64) -> f64 {
        let step = rate.hz() as f64 / fft_len(rate) as f64;
        (f / step).round() * step
    }

    /// Wire frames let through before a measurement starts: the filters' start, transients.
    const SETTLE: usize = 4096;

    fn pcm_of(rate: DsdRate, frames: usize, from: usize, sig: &dyn Fn(f64) -> f64) -> Vec<f32> {
        let wire = rate.wire_rate() as f64;
        (from..from + frames).map(|i| sig(i as f64 / wire) as f32).collect()
    }

    /// Encode mono `sig(t)` from where `enc` is: SETTLE frames, then one FFT's worth; the
    /// spectrum of that last part.
    fn measure(enc: &mut Encoder, sig: &dyn Fn(f64) -> f64, remove_mean: bool) -> (Spectrum, f64) {
        let rate = enc.out().rate;
        let pcm = pcm_of(rate, SETTLE + fft_len(rate) / 16, 0, sig);
        let mut words = Vec::new();
        enc.process(&pcm, &mut words);
        let x = bits(&words[SETTLE..], 1, 0);
        let mean = x.iter().sum::<f64>() / x.len() as f64;
        (Spectrum::new(&x, rate.hz() as f64, remove_mean), mean)
    }

    /// Like [`measure`], two consecutive stretches.
    fn measure2(enc: &mut Encoder, sig: &dyn Fn(f64) -> f64) -> (Spectrum, Spectrum, f64) {
        let rate = enc.out().rate;
        let n = fft_len(rate) / 16;
        let pcm = pcm_of(rate, SETTLE + 2 * n, 0, sig);
        let mut words = Vec::new();
        enc.process(&pcm, &mut words);
        let x = bits(&words[SETTLE..], 1, 0);
        let mean = x.iter().sum::<f64>() / x.len() as f64;
        let fs = rate.hz() as f64;
        let half = x.len() / 2;
        (Spectrum::new(&x[..half], fs, true), Spectrum::new(&x[half..], fs, true), mean)
    }

    fn sine(a: f64, f: f64) -> impl Fn(f64) -> f64 {
        move |t| a * (2.0 * PI * f * t).sin()
    }

    /// In-band power the linear model predicts: quantisation error white with σ² = 1/3 (the
    /// step between ±1 is 2), shaped by |NTF|².
    fn linear_noise(d: &Design, lo: f64, hi: f64) -> f64 {
        let fs = d.out.rate.hz() as f64;
        let steps = 20_000;
        let dw = 2.0 * PI * (hi - lo) / fs / steps as f64;
        let sum: f64 = (0..steps).map(|i| d.ntf(C::from_polar(1.0, 2.0 * PI * (lo + (hi - lo) * (i as f64 + 0.5) / steps as f64) / fs)).norm_sqr()).sum();
        sum * dw / PI / 3.0
    }

    /// Power of a PCM full-scale sine at depth LEVEL: the reference of every SNR here.
    const FULL: f64 = LEVEL * LEVEL / 2.0;

    struct Snr {
        fs: f64,
        m60: f64,
        n20_40: f64,
        n40_100: f64,
        linear: f64,
        level_1k: f64,
        level_19k: f64,
    }

    /// The SNR measurements of one variant, printed.
    fn snr(out: DsdOut) -> Snr {
        let d = design(out);
        let f = on_bin(out.rate, 1000.0);
        let f19 = on_bin(out.rate, 19_000.0);
        let mut enc = Encoder::new(out, 1);
        let (s, _) = measure(&mut enc, &sine(1.0, f), false);
        let noise = s.noise(20.0, 20_000.0, &[f]);
        enc.reset();
        let (s60, _) = measure(&mut enc, &sine(0.001, f), false);
        enc.reset();
        let (s19, _) = measure(&mut enc, &sine(1.0, f19), false);
        let r = Snr {
            fs: db(s.tone(f).powi(2) / 2.0 / noise),
            m60: db(s60.tone(f).powi(2) / 2.0 / s60.noise(20.0, 20_000.0, &[f])),
            n20_40: db(s.noise(20_000.0, 40_000.0, &[]) / FULL),
            n40_100: db(s.noise(40_000.0, 100_000.0, &[]) / FULL),
            linear: db(FULL / linear_noise(&d, 20.0, 20_000.0)),
            level_1k: 20.0 * (s.tone(f) / LEVEL).log10(),
            level_19k: 20.0 * (s19.tone(f19) / LEVEL).log10(),
        };
        println!(
            "{}: SNR 1 kHz full scale {:.1} dB (linear model {:.1}), at -60 dBFS {:.1} dB; noise 20-40 kHz {:.1} dB, 40-100 kHz {:.1} dB re full scale; level 1 kHz {:+.1e} dB, 19 kHz {:+.1e} dB",
            name(out),
            r.fs,
            r.linear,
            r.m60,
            r.n20_40,
            r.n40_100,
            r.level_1k,
            r.level_19k
        );
        r
    }

    /// What each variant measured (DSD64 in the default run, all in `snr_all_variants`),
    /// minus 2 dB.
    fn snr_floor(out: DsdOut) -> f64 {
        match (out.rate, out.order) {
            (DsdRate::Dsd64, ModOrder::Fifth) => 107.5,
            (DsdRate::Dsd64, ModOrder::Seventh) => 117.5,
            (DsdRate::Dsd128, ModOrder::Fifth) => 125.5,
            (DsdRate::Dsd128, ModOrder::Seventh) => 130.5,
            (DsdRate::Dsd256, ModOrder::Fifth) => 145.0,
            (DsdRate::Dsd256, ModOrder::Seventh) => 158.0,
        }
    }

    fn check_snr(out: DsdOut) {
        let r = snr(out);
        assert!(r.fs >= snr_floor(out), "{}: SNR {:.1} dB", name(out), r.fs);
        assert!(r.level_1k.abs() <= 0.001 && r.level_19k.abs() <= 0.001, "{}: levels {} {}", name(out), r.level_1k, r.level_19k);
    }

    #[test]
    fn snr_and_levels_dsd64() {
        for order in ModOrder::ALL {
            check_snr(dsd64(order));
        }
    }

    /// `cargo test --release -p hifi-engine -- --ignored dsd_encode --nocapture`
    #[test]
    #[ignore]
    fn snr_all_variants() {
        for out in all() {
            check_snr(out);
        }
    }

    // ---- settings -----------------------------------------------------------------------------

    #[test]
    fn rates_orders_and_lighter() {
        for (r, m, hz, wire) in [(DsdRate::Dsd64, 64, 2_822_400, 176_400), (DsdRate::Dsd128, 128, 5_644_800, 352_800), (DsdRate::Dsd256, 256, 11_289_600, 705_600)] {
            assert_eq!((r.multiple(), r.hz(), r.wire_rate()), (m, hz, wire));
            assert_eq!(DsdRate::from_multiple(m), Some(r));
        }
        assert_eq!(DsdRate::from_multiple(512), None);
        assert_eq!((ModOrder::from_number(5), ModOrder::from_number(7), ModOrder::from_number(6)), (Some(ModOrder::Fifth), Some(ModOrder::Seventh), None));
        let chain = |out: DsdOut| std::iter::successors(Some(out), |o| o.lighter()).skip(1).map(|o| (o.rate.multiple(), o.order.get())).collect::<Vec<_>>();
        assert_eq!(chain(DsdOut { rate: DsdRate::Dsd256, order: ModOrder::Seventh }), [(128, 7), (64, 7), (64, 5)]);
        assert_eq!(chain(DsdOut { rate: DsdRate::Dsd256, order: ModOrder::Fifth }), [(128, 5), (64, 5)]);
        assert_eq!(chain(dsd64(ModOrder::Fifth)), []);
    }

    #[test]
    fn designs_are_cached() {
        let out = DsdOut { rate: DsdRate::Dsd128, order: ModOrder::Seventh };
        assert!(Arc::ptr_eq(&design(out), &design(out)));
        assert!(Arc::ptr_eq(&Encoder::new(out, 2).design, &design(out)));
        assert!(!Arc::ptr_eq(&design(out), &design(DsdOut { rate: DsdRate::Dsd128, order: ModOrder::Fifth })));
    }

    // ---- stage A ------------------------------------------------------------------------------

    /// A half-band stage's impulse response at its output rate (gain 2).
    fn halfband_taps(h: &HalfBand) -> Vec<f64> {
        let m = h.m;
        let mut t = vec![0.0; 4 * m + 3];
        for i in 0..=2 * m + 1 {
            t[2 * i] = h.e[i.min(2 * m + 1 - i)];
        }
        t[2 * m + 1] = 1.0;
        t
    }

    fn response(taps: &[f64], f: f64) -> C {
        taps.iter().enumerate().map(|(n, &h)| C::from_polar(h, -2.0 * PI * f * n as f64)).sum()
    }

    /// The whole ×16 cascade as one filter at 16·R: zero-stuff and convolve, stage by stage.
    fn cascade(d: &Design) -> Vec<f64> {
        let mut h = vec![1.0];
        for st in &d.stages {
            let up: Vec<f64> = h.iter().flat_map(|&v| [v, 0.0]).collect();
            let t = halfband_taps(st);
            let mut out = vec![0.0; up.len() + t.len() - 1];
            for (i, a) in up.iter().enumerate() {
                for (j, b) in t.iter().enumerate() {
                    out[i + j] += a * b;
                }
            }
            h = out;
        }
        h
    }

    /// The modulator input (before the level and limiter) the encoder makes of mono `pcm`.
    fn interpolated(d: &Arc<Design>, pcm: &[f64]) -> Vec<f64> {
        let mut flat = Design::new(d.out);
        flat.max_depth = f64::INFINITY;
        let mut enc = Encoder::with_design(Arc::new(flat), 1);
        let h0 = d.stages[0].history();
        let mut u = Vec::new();
        for block in pcm.chunks(CHUNK) {
            enc.bufs[0][0][h0..h0 + block.len()].copy_from_slice(block);
            interpolate(&enc.design.clone(), &mut enc.bufs[0], &mut enc.u[0], block.len());
            u.extend(enc.u[0][..16 * block.len()].iter().map(|v| v / LEVEL));
        }
        u
    }

    #[test]
    fn interpolator_meets_its_spec() {
        let d = design(dsd64(ModOrder::Fifth));
        let lens: Vec<usize> = d.stages.iter().map(|s| 4 * s.m + 3).collect();
        println!("half-band lengths {lens:?}, delay {:.4} wire frames", d.latency);
        for (s, st) in d.stages.iter().enumerate() {
            // At the stage's output rate R·2^(s+1): pass band to STAGE_PASS·R, stop band from
            // its image R·2^s − STAGE_PASS·R.
            let taps = halfband_taps(st);
            let out_rate = (2usize << s) as f64;
            let (pass, stop) = (STAGE_PASS[s] / out_rate, ((1 << s) as f64 - STAGE_PASS[s]) / out_rate);
            let grid = |a: f64, b: f64| (0..=4000).map(move |i| a + (b - a) * i as f64 / 4000.0);
            let ripple = grid(0.0, pass).map(|f| (20.0 * (response(&taps, f).norm() / 2.0).log10()).abs()).fold(0.0, f64::max);
            let floor = grid(stop, 0.5).map(|f| 20.0 * (response(&taps, f).norm() / 2.0).log10()).fold(-400.0, f64::max);
            println!("stage {}: {} taps, pass band ±{ripple:.1e} dB, stop band {:.1} dB", s + 1, taps.len(), -floor);
            assert!(floor <= -HALFBAND_ATTEN_DB, "stage {}: stop band only {:.1} dB", s + 1, -floor);
        }
        // The cascade: flat in the audio band and to 0.34·R.
        let h = cascade(&d);
        let r = 176_400.0;
        let gain_db = |f: f64| 20.0 * (response(&h, f / (16.0 * r)).norm() / 16.0).log10();
        let audio = (0..=2000).map(|i| gain_db(20_000.0 * i as f64 / 2000.0).abs()).fold(0.0, f64::max);
        let edge = gain_db(0.34 * r);
        println!("cascade: 0-20 kHz within ±{audio:.1e} dB, {edge:+.1e} dB at 0.34·R");
        assert!(audio <= 0.0001 && edge.abs() <= 0.01);

        // The streaming code is that filter, sample for sample.
        let mut imp = vec![0.0; 700];
        imp[0] = 1.0;
        let u = interpolated(&d, &imp);
        let err = h.iter().zip(&u).fold(0.0f64, |m, (a, b)| m.max((a - b).abs()));
        assert!(err < 1e-14, "impulse response differs by {err:e}");
        assert!(u[h.len()..].iter().all(|&v| v == 0.0));

        // Every image of a 0.3·R tone, measured at 16·R.
        let n = 1 << 18;
        let step = 16.0 * r / n as f64;
        let f0 = (0.3 * r / step).round() * step;
        let x: Vec<f64> = (0..n / 16 + 2000).map(|i| 0.5 * (2.0 * PI * f0 * i as f64 / r).sin()).collect();
        let y = interpolated(&d, &x);
        let sp = Spectrum::new(&y[16 * 1000..16 * 1000 + n], 16.0 * r, false);
        let tone = sp.tone(f0);
        let mut worst: f64 = 0.0;
        for k in 1..=8 {
            for f in [k as f64 * r - f0, k as f64 * r + f0] {
                if f < 8.0 * r {
                    worst = worst.max(sp.tone(f) / tone);
                }
            }
        }
        println!("0.3·R tone: images at least {:.1} dB down", -20.0 * worst.log10());
        assert!(20.0 * worst.log10() <= -125.0);
    }

    // ---- stage B: the design ------------------------------------------------------------------

    #[test]
    fn ntf_is_as_designed() {
        for out in all() {
            let d = design(out);
            let h_inf = H_INF[out.order.index()][out.rate.index()];
            assert!(d.poles.iter().all(|p| p.norm() < 1.0), "{}: pole outside", name(out));
            let (mut peak, mut at) = (0.0, 0.0);
            for i in 0..=100_000 {
                let w = PI * i as f64 / 100_000.0;
                let g = d.ntf(C::from_polar(1.0, w)).norm();
                if g > peak {
                    (peak, at) = (g, w);
                }
            }
            println!("{}: max |NTF| {peak:.5} at {:.4}·π, poles {:?}", name(out), at / PI, d.poles.iter().map(|p| format!("{:.4}∠{:.4}", p.norm(), p.arg())).collect::<Vec<_>>());
            assert!((peak / h_inf - 1.0).abs() < 0.01 && at == PI, "{}: max {peak} at {at}", name(out));
            // The realised zeros (from g) are the designed ones.
            for (j, z) in d.zeros.iter().skip(1).step_by(2).enumerate() {
                assert!((2.0 - 2.0 * z.re - d.g[j]).abs() < 1e-15);
            }
        }
    }

    /// The loop with the quantiser replaced by v = y + e, `e` and `u` given per sample.
    fn linear_response(d: &Design, n: usize, u: impl Fn(usize) -> f64, e: impl Fn(usize) -> f64) -> Vec<f64> {
        fn go<const K: usize>(d: &Design, n: usize, u: &dyn Fn(usize) -> f64, e: &dyn Fn(usize) -> f64) -> Vec<f64> {
            let c = Coefs::<K>::of(d);
            let mut s = State::<K>::load(&[0.0; 7]);
            (0..n)
                .map(|i| {
                    let v = s.q[K - 1] + u(i) + e(i);
                    update(&c, &mut s, u(i), v);
                    v
                })
                .collect()
        }
        if d.out.order == ModOrder::Fifth { go::<2>(d, n, &u, &e) } else { go::<3>(d, n, &u, &e) }
    }

    fn dft(x: &[f64], w: f64) -> C {
        x.iter().enumerate().map(|(n, &v)| C::from_polar(v, -w * n as f64)).sum()
    }

    #[test]
    fn linear_loop_has_the_designed_ntf_and_unity_stf() {
        for out in all() {
            let d = design(out);
            let fs = out.rate.hz() as f64;
            let n = 1 << 15;
            let ntf = linear_response(&d, n, |_| 0.0, |i| (i == 0) as u8 as f64);
            let stf = linear_response(&d, n, |i| (i == 0) as u8 as f64, |_| 0.0);
            assert!(ntf[n - 100..].iter().all(|v| v.abs() < 1e-20), "{}: impulse response hasn't died out", name(out));
            let (mut inband, mut outband) = (0.0f64, 0.0f64);
            for f in [1000.0, 10_000.0, 19_000.0, 30_000.0, 50_000.0, 100_000.0, 200_000.0, 400_000.0, 1e6, fs / 2.0] {
                let w = 2.0 * PI * f / fs;
                let want = d.ntf(C::from_polar(1.0, w));
                let err = (dft(&ntf, w) - want).norm() / want.norm();
                if f <= 20_000.0 { inband = inband.max(err) } else { outband = outband.max(err) }
            }
            let stf_db = [1000.0, 20_000.0].map(|f| 20.0 * dft(&stf, 2.0 * PI * f / fs).norm().log10());
            println!("{}: NTF of the loop vs the design: in band {inband:.1e}, out of band {outband:.1e}; STF {:+.1e} dB at 1 kHz, {:+.1e} dB at 20 kHz", name(out), stf_db[0], stf_db[1]);
            assert!(inband <= 1e-3 && outband <= 1e-6, "{}: {inband:e} {outband:e}", name(out));
            assert!(stf_db.iter().all(|g| g.abs() <= 0.001));
        }
    }

    // ---- the modulator as it runs -------------------------------------------------------------

    /// The modulator a sample at a time, keeping the largest |state| seen: the same `step`
    /// as the encoder's, so the same bits.
    struct RawMod {
        d: Arc<Design>,
        st: [f64; 7],
        rng: u64,
        peak: [f64; 7],
    }

    impl RawMod {
        fn new(d: Arc<Design>) -> Self {
            Self { d, st: [0.0; 7], rng: seed(0), peak: [0.0; 7] }
        }

        fn feed(&mut self, u: &[f64], bits: &mut Vec<bool>) {
            fn go<const K: usize>(m: &mut RawMod, u: &[f64], bits: &mut Vec<bool>) {
                let c = Coefs::<K>::of(&m.d);
                let mut s = State::<K>::load(&m.st);
                let scale = m.d.dither / 2_147_483_648.0;
                for &x in u {
                    bits.push(step(&c, &mut s, x, threshold(&m.d, x, &mut m.rng, scale)));
                    m.peak[0] = m.peak[0].max(s.x0.abs());
                    for j in 0..K {
                        m.peak[1 + 2 * j] = m.peak[1 + 2 * j].max(s.p[j].abs());
                        m.peak[2 + 2 * j] = m.peak[2 + 2 * j].max(s.q[j].abs());
                    }
                }
                s.store(&mut m.st);
            }
            if self.d.out.order == ModOrder::Fifth { go::<2>(self, u, bits) } else { go::<3>(self, u, bits) }
        }
    }

    /// The modulator input the encoder makes of mono `pcm` (interpolated, level, limited).
    fn mod_input(enc: &mut Encoder, pcm: &[f32], u: &mut Vec<f64>) {
        let d = enc.design.clone();
        let h0 = d.stages[0].history();
        for block in pcm.chunks(CHUNK) {
            for (dst, v) in enc.bufs[0][0][h0..h0 + block.len()].iter_mut().zip(block) {
                *dst = *v as f64;
            }
            enc.clips += interpolate(&d, &mut enc.bufs[0], &mut enc.u[0], block.len());
            u.extend_from_slice(&enc.u[0][..16 * block.len()]);
        }
    }

    /// `step` is `update` with v = ±1, to the last bit of every state.
    #[test]
    fn step_is_the_update() {
        fn go<const K: usize>(d: &Design) {
            let c = Coefs::<K>::of(d);
            let (mut a, mut b) = (State::<K>::load(&[0.0; 7]), State::<K>::load(&[0.0; 7]));
            for i in 0..100_000u64 {
                let u = 0.6 * (2.0 * noise_at(i, 1) - 1.0);
                let thr = -(u + 0.2 * (2.0 * noise_at(i, 2) - 1.0));
                let v = if a.q[K - 1] >= thr { 1.0 } else { -1.0 };
                assert_eq!(step(&c, &mut a, u, thr), v > 0.0);
                update(&c, &mut b, u, v);
                assert!(a.x0 == b.x0 && a.p == b.p && a.q == b.q, "sample {i}");
            }
        }
        go::<2>(&design(dsd64(ModOrder::Fifth)));
        go::<3>(&design(dsd64(ModOrder::Seventh)));
    }

    #[test]
    fn fast_loop_equals_the_reference() {
        for order in ModOrder::ALL {
            let out = dsd64(order);
            let pcm: Vec<f32> = (0..20_000).map(|i| (0.9 * (i as f64 * 0.07).sin() + 0.1 * (2.0 * noise_at(i, 4) - 1.0)) as f32).collect();
            let mut words = Vec::new();
            Encoder::new(out, 1).process(&pcm, &mut words);
            let mut u = Vec::new();
            mod_input(&mut Encoder::new(out, 1), &pcm, &mut u);
            let mut m = RawMod::new(design(out));
            let mut b = Vec::new();
            m.feed(&u, &mut b);
            assert!(bits(&words, 1, 0).iter().zip(&b).all(|(x, y)| (*x > 0.0) == *y), "{}", name(out));
        }
    }

    /// Repeatable uniform noise in 0 … 1: sample `i` of stream `seed`.
    fn noise_at(i: u64, seed: u64) -> f64 {
        let mut x = (i ^ seed.rotate_left(32)).wrapping_add(0x9E37_79B9_7F4A_7C15).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 31;
        x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 29;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }

    /// A PCM signal by wire frame.
    type Signal = Box<dyn Fn(usize) -> f64 + Send + Sync>;

    /// Wire frames of the longest stability run: 20 s at DSD64 (10 s DSD128, 5 s DSD256).
    const LONGEST: usize = 20 * 176_400;

    /// White noise low-passed at 20 kHz (Kaiser FIR, −6 dB at 20 kHz, 90 dB down from 24 kHz)
    /// at `rate`'s wire rate, normalised to a peak of exactly 1.0 over [`LONGEST`] frames;
    /// made once per (rate, seed) and kept.
    fn audio_noise(rate: DsdRate, seed: u64) -> Arc<Vec<f64>> {
        type Made = Vec<((DsdRate, u64), Arc<Vec<f64>>)>;
        static CACHE: Mutex<Made> = Mutex::new(Vec::new());
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, v)) = cache.iter().find(|(k, _)| *k == (rate, seed)) {
            return v.clone();
        }
        let wire = rate.wire_rate() as f64;
        let taps = (((90.0 - 8.0) / (14.36 * 8000.0 / wire)) as usize) | 1;
        let h = kaiser_lowpass(taps, 20_000.0 / wire, kaiser_beta(90.0));
        let white: Vec<f64> = (0..LONGEST + taps).map(|i| 2.0 * noise_at(i as u64, seed) - 1.0).collect();
        let mut x: Vec<f64> = white.windows(taps).take(LONGEST).map(|w| w.iter().zip(&h).map(|(a, b)| a * b).sum()).collect();
        let peak = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        x.iter_mut().for_each(|v| *v /= peak);
        let x = Arc::new(x);
        cache.push(((rate, seed), x.clone()));
        x
    }

    /// The programme-like test signals at PCM amplitude `a`, by wire frame: sines, a square,
    /// DC of both signs, audio-band noise peaking at `a`, and the same noise driven 12 dB
    /// into clipping and hard-clipped at ±`a` (a loud, clipped master), three seeds each.
    fn programme(a: f64, rate: DsdRate) -> Vec<(&'static str, Signal)> {
        let wire = rate.wire_rate() as f64;
        let t = move |i: usize| i as f64 / wire;
        let mut v: Vec<(&'static str, Signal)> = vec![
            ("sine 20 Hz", Box::new(move |i| a * (2.0 * PI * 20.0 * t(i)).sin())),
            ("sine 1 kHz", Box::new(move |i| a * (2.0 * PI * 1000.0 * t(i)).sin())),
            ("sine 20 kHz", Box::new(move |i| a * (2.0 * PI * 20_000.0 * t(i)).sin())),
            ("square 100 Hz", Box::new(move |i| if (t(i) * 100.0).fract() < 0.5 { a } else { -a })),
            ("DC +", Box::new(move |_| a)),
            ("DC -", Box::new(move |_| -a)),
        ];
        let drive = 10f64.powf(12.0 / 20.0);
        for (k, (plain, clipped)) in [("audio noise 1", "clipped noise 1"), ("audio noise 2", "clipped noise 2"), ("audio noise 3", "clipped noise 3")].into_iter().enumerate() {
            let x = audio_noise(rate, 21 + k as u64);
            let y = x.clone();
            v.push((plain, Box::new(move |i| a * x[i % x.len()])));
            v.push((clipped, Box::new(move |i| a * (drive * y[i % y.len()]).clamp(-1.0, 1.0))));
        }
        v
    }

    /// White noise, white up to half the wire rate (uniform, PCM amplitude `a`), three seeds:
    /// the torture test, judged with a smaller margin than [`programme`].
    fn wire_noise(a: f64) -> Vec<(&'static str, Signal)> {
        vec![
            ("wire noise 1", Box::new(move |i| a * (2.0 * noise_at(i as u64, 9) - 1.0))),
            ("wire noise 2", Box::new(move |i| a * (2.0 * noise_at(i as u64, 10) - 1.0))),
            ("wire noise 3", Box::new(move |i| a * (2.0 * noise_at(i as u64, 11) - 1.0))),
        ]
    }

    /// Both classes.
    fn signals(a: f64, rate: DsdRate) -> Vec<(&'static str, Signal)> {
        let mut v = programme(a, rate);
        v.extend(wire_noise(a));
        v
    }

    /// Encode `secs` of `sig` (mono) in blocks; overloads, clips.
    fn run_signal(out: DsdOut, secs: f64, sig: &dyn Fn(usize) -> f64) -> (u64, u64) {
        let mut enc = Encoder::new(out, 1);
        let frames = (secs * out.rate.wire_rate() as f64) as usize;
        let (mut pcm, mut words) = (Vec::new(), Vec::new());
        let mut i = 0;
        while i < frames {
            pcm.clear();
            pcm.extend((i..(i + 8192).min(frames)).map(|i| sig(i) as f32));
            words.clear();
            enc.process(&pcm, &mut words);
            i += pcm.len();
        }
        (enc.overloads(), enc.clips())
    }

    /// Every signal at PCM full scale (depth 0.5) and at depth 0.6, `secs` at DSD64 (scaled
    /// down at the higher rates, same number of samples): no overload.
    fn check_stable(outs: &[DsdOut], secs64: f64) {
        for &out in outs {
            let d = design(out);
            assert!(d.max_depth >= 0.58);
            let secs = secs64 * 64.0 / out.rate.multiple() as f64;
            std::thread::scope(|sc| {
                for depth in [0.5, 0.6] {
                    for (sig_name, sig) in signals(depth / LEVEL, out.rate) {
                        sc.spawn(move || {
                            let (over, clips) = run_signal(out, secs, &sig);
                            println!("{} {sig_name} at depth {depth}: {over} overloads, {clips} clipped samples in {secs} s", name(out));
                            assert_eq!(over, 0, "{} {sig_name} at depth {depth}", name(out));
                        });
                    }
                }
            });
        }
    }

    #[test]
    fn stable_at_rated_levels_dsd64() {
        check_stable(&ModOrder::ALL.map(dsd64), 4.0);
    }

    /// 20 s at DSD64, 10 s at DSD128, 5 s at DSD256.
    #[test]
    #[ignore]
    fn stable_at_rated_levels_long() {
        check_stable(&all(), 20.0);
    }

    // ---- calibration and limits (ignored: long) ----------------------------------------------

    /// Run `sig` (PCM, by wire frame) for `secs` through the interpolation and limiter of
    /// `d` and the reference modulator; the state peaks.
    fn signal_peaks(d: Arc<Design>, secs: f64, sig: &dyn Fn(usize) -> f64) -> [f64; 7] {
        signal_peaks_until(d, secs, sig, &[f64::INFINITY; 7]).0
    }

    /// Like [`signal_peaks`], stopping as soon as a state passes `stop`; true if it did.
    fn signal_peaks_until(d: Arc<Design>, secs: f64, sig: &dyn Fn(usize) -> f64, stop: &[f64; 7]) -> ([f64; 7], bool) {
        let frames = (secs * d.out.rate.wire_rate() as f64) as usize;
        let mut enc = Encoder::with_design(d.clone(), 1);
        let mut m = RawMod::new(d);
        let (mut pcm, mut u, mut b) = (Vec::new(), Vec::new(), Vec::new());
        let mut i = 0;
        while i < frames {
            pcm.clear();
            pcm.extend((i..(i + 8192).min(frames)).map(|i| sig(i) as f32));
            u.clear();
            mod_input(&mut enc, &pcm, &mut u);
            b.clear();
            m.feed(&u, &mut b);
            i += pcm.len();
            if m.peak.iter().zip(stop).any(|(p, s)| p > s) {
                return (m.peak, true);
            }
        }
        (m.peak, false)
    }

    /// Whether every test signal at depth `m` (PCM m/LEVEL, the limiter lifted to m, dither
    /// as `d` has it) runs `secs64` s at DSD64 (scaled to the same samples at higher rates)
    /// without a state running away (past 100× its peak at depth 0.2); the ones that fail.
    fn unstable_at(d: &Design, m: f64, secs64: f64, class: Class) -> Vec<&'static str> {
        let out = d.out;
        let secs = secs64 * 64.0 / out.rate.multiple() as f64;
        let mut t = d.clone();
        t.limits = [f64::INFINITY; 7];
        let low = signal_peaks(Arc::new(t.clone()), 0.5 * 64.0 / out.rate.multiple() as f64, &|_| 0.2 / LEVEL);
        let stop = low.map(|l| 100.0 * l.max(1e-30));
        t.max_depth = m;
        let d = Arc::new(t);
        let sigs = match class {
            Class::Programme => programme(m / LEVEL, out.rate),
            Class::WireNoise => wire_noise(m / LEVEL),
        };
        std::thread::scope(|sc| {
            let hs: Vec<_> = sigs.iter().map(|(n, f)| {
                let d = d.clone();
                sc.spawn(move || (*n, signal_peaks_until(d, secs, &**f, &stop).1))
            }).collect();
            hs.into_iter().map(|h| h.join().unwrap()).filter(|r| r.1).map(|r| r.0).collect()
        })
    }

    #[derive(Clone, Copy)]
    enum Class {
        Programme,
        WireNoise,
    }

    /// The first failures the stability rule asks for (see [`H_INF`]): programme-like signals
    /// at 0.66 or later (bounded to 0.06 over the rated depth), wire-rate white noise at 0.62
    /// or later (bounded to 0.02 over).
    const PROGRAMME_FAIL: f64 = 0.66;
    const WIRE_NOISE_FAIL: f64 = 0.62;

    /// The first depth from the rated one up, in steps of 0.02 (to 0.80), at which a signal
    /// of `class` runs away in `secs64` s; and which signals did.
    fn first_failure(d: &Design, secs64: f64, class: Class) -> (f64, Vec<&'static str>) {
        let mut m = MAX_DEPTH;
        while m < 0.805 {
            let bad = unstable_at(d, m, secs64, class);
            if !bad.is_empty() {
                return (m, bad);
            }
            m += 0.02;
        }
        (f64::INFINITY, Vec::new())
    }

    /// The full-scale SNR of `d` (1 kHz, 20 Hz – 20 kHz), clamps off: the limits are
    /// calibrated for the chosen design and would act on a candidate's larger states.
    fn snr_of(d: Design) -> f64 {
        let d = Design { limits: [f64::INFINITY; 7], ..d };
        let f = on_bin(d.out.rate, 1000.0);
        let (s, _) = measure(&mut Encoder::with_design(Arc::new(d), 1), &sine(1.0, f), false);
        db(s.tone(f).powi(2) / 2.0 / s.noise(20.0, 20_000.0, &[f]))
    }

    /// H∞ for each variant: the largest on a 0.05 grid (1.20 … 1.50) that keeps programme-like
    /// signals to a first failure at 0.66 or later and wire-rate white noise at 0.62 or later
    /// (20 s at DSD64, the limiter lifted to the depth). Prints, per candidate, both first
    /// failing depths and the full-scale SNR. Every candidate runs with the variant's dither as
    /// chosen; the chosen H∞ is then checked again with the dither surveyed for it.
    /// `cargo test --release -p hifi-engine -- --ignored dsd_encode::tests::h_inf_search --nocapture`
    #[test]
    #[ignore]
    fn h_inf_search() {
        let rows: Vec<String> = std::thread::scope(|sc| {
            let hs: Vec<_> = all()
                .into_iter()
                .map(|out| {
                    sc.spawn(move || {
                        let mut line = format!("{}:", name(out));
                        let mut best = None;
                        for i in 0..=6 {
                            let h = 1.2 + 0.05 * i as f64;
                            let d = Design::build(out, h);
                            let (prog, pbad) = first_failure(&d, 20.0, Class::Programme);
                            let (wire, wbad) = first_failure(&d, 20.0, Class::WireNoise);
                            let snr = snr_of(d);
                            if prog >= PROGRAMME_FAIL - 1e-9 && wire >= WIRE_NOISE_FAIL - 1e-9 {
                                best = Some(h);
                            }
                            line += &format!("\n  H {h:.2}: programme fails at {prog:.2} {pbad:?}; wire noise at {wire:.2} {wbad:?}; SNR {snr:.1} dB");
                        }
                        line + &format!("\n  chosen {best:?}")
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for r in rows {
            println!("{r}");
        }
    }

    /// The state peaks at the rated depth, over 20 s (DSD64; 10 s DSD128, 5 s DSD256) of
    /// every test signal, through the encoder's interpolation and limiter: [`LIMITS`] are
    /// twice these, rounded up to 2 significant digits.
    /// `cargo test --release -p hifi-engine -- --ignored dsd_encode::tests::calibration --nocapture`
    #[test]
    #[ignore]
    fn calibration() {
        for order in ModOrder::ALL {
            let mut table = Vec::new();
            for rate in DsdRate::ALL {
                let out = DsdOut { rate, order };
                let mut d = Design::new(out);
                d.limits = [f64::INFINITY; 7];
                let d = Arc::new(d);
                let secs = 20.0 * 64.0 / rate.multiple() as f64;
                let sigs = signals(d.max_depth / LEVEL, rate);
                let peaks: Vec<[f64; 7]> = std::thread::scope(|sc| {
                    let hs: Vec<_> = sigs.iter().map(|(_, f)| { let d = d.clone(); sc.spawn(move || signal_peaks(d, secs, &**f)) }).collect();
                    hs.into_iter().map(|h| h.join().unwrap()).collect()
                });
                let mut worst = [0.0f64; 7];
                for ((sig_name, _), p) in sigs.iter().zip(&peaks) {
                    println!("{} {sig_name}: {:?}", name(out), p[..order.get()].iter().map(|v| format!("{v:.3e}")).collect::<Vec<_>>());
                    worst.iter_mut().zip(p).for_each(|(w, v)| *w = w.max(*v));
                }
                let round2 = |v: f64| {
                    let e = 10f64.powf(v.log10().floor() - 1.0);
                    (v / e).ceil() * e
                };
                table.push(format!("[{}]", (0..7).map(|i| if i < order.get() { format!("{:e}", round2(2.0 * worst[i])) } else { "0.0".into() }).collect::<Vec<_>>().join(", ")));
            }
            println!("{order:?} limits: [{}]", table.join(", "));
        }
    }

    /// The rule [`H_INF`] is chosen by, with the limiter lifted to depth m and 20 s runs
    /// (DSD64; 10 s DSD128, 5 s DSD256): programme-like signals stay bounded at every m up to
    /// 0.64 (first failure 0.66 or later), wire-rate white noise up to 0.60 (first failure
    /// 0.62 or later). Prints where each class first fails, and which signals.
    /// `cargo test --release -p hifi-engine -- --ignored dsd_encode::tests::stability_limit --nocapture`
    #[test]
    #[ignore]
    fn stability_limit() {
        let mut short = Vec::new();
        for out in all() {
            let d = design(out);
            let (prog, pbad) = first_failure(&d, 20.0, Class::Programme);
            let (wire, wbad) = first_failure(&d, 20.0, Class::WireNoise);
            println!("{}: H∞ {:.2}, dither {}; programme first fails at {prog:.2} {pbad:?}; wire noise at {wire:.2} {wbad:?}", name(out), d.h_inf, d.dither);
            if prog < PROGRAMME_FAIL - 1e-9 || wire < WIRE_NOISE_FAIL - 1e-9 {
                short.push(name(out));
            }
        }
        assert!(short.is_empty(), "short of the rule: {short:?}");
    }

    // ---- overload ---------------------------------------------------------------------------

    /// In-band SNR of a full-scale 1 kHz tone, after `before` (PCM, by wire frame) has run
    /// for `before_frames` and the tone for 10 ms more; the encoder's overloads and clips, and
    /// the longest run of all-0 or all-1 words.
    fn snr_after(d: Arc<Design>, before: &dyn Fn(usize) -> f64, before_frames: usize) -> (f64, u64, u64, usize) {
        let rate = d.out.rate;
        let wire = rate.wire_rate() as f64;
        let f = on_bin(rate, 1000.0);
        let gap = (0.010 * wire) as usize;
        let n = fft_len(rate) / 16;
        let pcm: Vec<f32> = (0..before_frames + gap + n)
            .map(|i| if i < before_frames { before(i) } else { (2.0 * PI * f * i as f64 / wire).sin() } as f32)
            .collect();
        let mut enc = Encoder::with_design(d, 1);
        let mut words = Vec::new();
        enc.process(&pcm, &mut words);
        assert!(enc.state[0].iter().all(|v| v.is_finite()));
        let (mut run, mut longest) = (0, 0);
        for w in &words {
            run = if *w == 0 || *w == 0xFFFF { run + 1 } else { 0 };
            longest = longest.max(run);
        }
        let sp = Spectrum::new(&bits(&words[before_frames + gap..], 1, 0), rate.hz() as f64, false);
        let snr = db(sp.tone(f).powi(2) / 2.0 / sp.noise(20.0, 20_000.0, &[f]));
        (snr, enc.overloads(), enc.clips(), longest)
    }

    fn check_overload(out: DsdOut) {
        let d = design(out);
        let wire = out.rate.wire_rate() as f64;
        let (normal, ..) = snr_after(d.clone(), &|_| 0.0, 0);
        let frames = (0.1 * wire) as usize;
        let sine = |i: usize| 4.0 * (2.0 * PI * 1000.0 * i as f64 / wire).sin();
        let square = |i: usize| if (i as f64 / wire * 100.0).fract() < 0.5 { 4.0 } else { -4.0 };
        for (sig_name, sig) in [("sine", &sine as &dyn Fn(usize) -> f64), ("square", &square)] {
            let (snr, over, clips, stuck) = snr_after(d.clone(), sig, frames);
            println!("{}: after 100 ms of a PCM 4.0 {sig_name}: SNR {snr:.1} dB 10 ms later (normal {normal:.1}); {over} overloads, {clips} clipped, longest all-0/all-1 run {stuck} words", name(out));
            assert!(snr >= normal - 3.0, "{}: {sig_name}", name(out));
            assert!(stuck < 64);
        }
        // Without the limiter the loop does go unstable: the clamps must catch it, and it
        // must come back as fast.
        let mut wild = Design::new(out);
        wild.max_depth = f64::INFINITY;
        let (snr, over, _, _) = snr_after(Arc::new(wild), &sine, frames);
        println!("{}: limiter off, PCM 4.0 sine: {over} clamp actions; SNR {snr:.1} dB 10 ms later", name(out));
        assert!(over > 0 && snr >= normal - 3.0, "{}: {over} {snr}", name(out));
    }

    #[test]
    fn overload_recovers_dsd64() {
        for order in ModOrder::ALL {
            check_overload(dsd64(order));
        }
    }

    #[test]
    #[ignore]
    fn overload_recovers_all_variants() {
        for out in all() {
            check_overload(out);
        }
    }

    // ---- silence and low levels -------------------------------------------------------------

    fn check_quiet(out: DsdOut, more: bool) {
        let f = on_bin(out.rate, 1000.0);
        let mut enc = Encoder::new(out, 1);
        let (loud, _) = measure(&mut enc, &sine(1.0, f), true);
        let loud_noise = loud.noise(20.0, 20_000.0, &[f]);
        let music = |t: f64| 0.3 * (2.0 * PI * 440.0 * t).sin() + 0.2 * (2.0 * PI * 3_100.0 * t).sin() + 0.1 * (2.0 * PI * 12_345.0 * t).sin();
        // Name, after a second of music, signal by time, the mean it must give, a tone to skip.
        type Case = (String, bool, Box<dyn Fn(f64) -> f64>, f64, Option<f64>);
        let mut cases: Vec<Case> = vec![
            ("silence from reset".into(), false, Box::new(|_| 0.0), 0.0, None),
            ("silence after music".into(), true, Box::new(|_| 0.0), 0.0, None),
            ("DC 0.001".into(), false, Box::new(|_| 0.001), 0.001 * LEVEL, None),
            ("1 kHz at -100 dBFS".into(), false, Box::new(sine(1e-5, f)), 0.0, Some(f)),
        ];
        if more {
            for dc in [-0.001, 3e-5, 1e-4, -3e-4, 3e-3, 1e-2] {
                cases.push((format!("DC {dc}"), false, Box::new(move |_| dc), dc * LEVEL, None));
            }
        }
        for (case, after_music, sig, mean_want, tone) in cases {
            enc.reset();
            if after_music {
                let pcm = pcm_of(out.rate, out.rate.wire_rate() as usize, 0, &music);
                enc.process(&pcm, &mut Vec::new());
            }
            let (sp, sp2, mean) = measure2(&mut enc, &*sig);
            let skip: Vec<f64> = tone.into_iter().collect();
            let noise = sp.noise(20.0, 20_000.0, &skip);
            let (at, above) = persistent_line(&sp, &sp2, 20.0, 20_000.0, &skip);
            println!(
                "{} {case}: strongest line {above:.1} dB above its neighbours at {at:.0} Hz; in-band noise {:+.2} dB vs a full-scale tone; mean {mean:.2e} (want {mean_want:.2e})",
                name(out),
                db(noise / loud_noise)
            );
            assert!(above <= 12.0, "{} {case}: line at {at} Hz, {above:.1} dB", name(out));
            assert!(db(noise / loud_noise) <= 3.0, "{} {case}", name(out));
            assert!((mean - mean_want).abs() <= 1e-4, "{} {case}: mean {mean}", name(out));
        }
    }

    #[test]
    fn silence_and_low_levels_dsd64() {
        for order in ModOrder::ALL {
            check_quiet(dsd64(order), false);
        }
    }

    #[test]
    #[ignore]
    fn silence_and_low_levels_all_variants() {
        for out in all() {
            check_quiet(out, true);
        }
    }

    // ---- streaming behaviour ----------------------------------------------------------------

    /// Interleaved test music: a different mix per channel, some of it beyond full scale.
    fn music(frames: usize, ch: usize) -> Vec<f32> {
        (0..frames * ch)
            .map(|i| {
                let (f, c) = ((i / ch) as f64, (i % ch) as f64);
                (0.6 * (f * (0.013 + 0.007 * c)).sin() + 0.5 * (f * 0.31).sin() + 0.15 * (2.0 * noise_at(i as u64, 5) - 1.0)) as f32
            })
            .collect()
    }

    fn encode_in(out: DsdOut, ch: usize, pcm: &[f32], block: usize) -> Vec<u16> {
        let mut enc = Encoder::new(out, ch);
        let mut words = Vec::new();
        for b in pcm.chunks(block * ch) {
            let before = words.len();
            enc.process(b, &mut words);
            assert_eq!(words.len() - before, b.len(), "one word per sample");
        }
        words
    }

    #[test]
    fn chunking_does_not_matter() {
        for order in ModOrder::ALL {
            let out = dsd64(order);
            let pcm = music(12_000, 2);
            let whole = encode_in(out, 2, &pcm, pcm.len());
            for block in [1, 7, 2048, 5000] {
                assert!(encode_in(out, 2, &pcm, block) == whole, "{}: blocks of {block}", name(out));
            }
        }
    }

    #[test]
    fn channels_are_independent_and_identical() {
        for order in ModOrder::ALL {
            let out = dsd64(order);
            let frames = 10_000;
            // Three different channels (a pair and a single in the loop): each as if alone,
            // given its channel's dither stream.
            let pcm = music(frames, 3);
            let w = encode_in(out, 3, &pcm, 1000);
            for c in 0..3 {
                let alone: Vec<f32> = pcm.iter().skip(c).step_by(3).copied().collect();
                let wc: Vec<u16> = w.iter().skip(c).step_by(3).copied().collect();
                let mut enc = Encoder::new(out, 1);
                enc.rng[0] = seed(c);
                let mut mono = Vec::new();
                enc.process(&alone, &mut mono);
                assert!(mono == wc, "{} channel {c}", name(out));
            }
            // The same signal on both channels: the same loop, but its own dither on each, so
            // the bits differ (and so does the noise: uncorrelated between the channels).
            let mono = music(frames, 1);
            let both: Vec<f32> = mono.iter().flat_map(|&v| [v, v]).collect();
            let w = encode_in(out, 2, &both, 1000);
            let same = w.as_chunks::<2>().0.iter().filter(|p| p[0] == p[1]).count();
            assert!(same < frames / 10, "{}: {same} identical words", name(out));
            let mut enc = Encoder::new(out, 2);
            enc.design = Arc::new(Design { dither: 0.0, ..Design::new(out) });
            let mut w = Vec::new();
            enc.process(&both, &mut w);
            assert!(w.as_chunks::<2>().0.iter().all(|p| p[0] == p[1]), "{} without dither", name(out));
        }
    }

    #[test]
    fn reset_equals_new() {
        for order in ModOrder::ALL {
            let out = dsd64(order);
            let pcm = music(6000, 2);
            let fresh = encode_in(out, 2, &pcm, 6000);
            let mut enc = Encoder::new(out, 2);
            let mut junk = Vec::new();
            enc.process(&music(3333, 2).iter().map(|v| v * 4.0).collect::<Vec<_>>(), &mut junk);
            assert!(enc.clips() > 0);
            enc.reset();
            assert_eq!((enc.overloads(), enc.clips()), (0, 0));
            let mut again = Vec::new();
            enc.process(&pcm, &mut again);
            assert!(again == fresh, "{}", name(out));
        }
    }

    #[test]
    fn non_finite_input_is_silence() {
        let out = dsd64(ModOrder::Seventh);
        let mut pcm = music(4000, 1);
        let silent: Vec<f32> = pcm.iter().enumerate().map(|(i, &v)| if i % 97 == 5 { 0.0 } else { v }).collect();
        for (i, v) in pcm.iter_mut().enumerate() {
            if i % 97 == 5 {
                *v = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY][i % 3];
            }
        }
        assert!(encode_in(out, 1, &pcm, 512) == encode_in(out, 1, &silent, 512));
    }

    #[test]
    fn latency_is_the_measured_delay() {
        for order in ModOrder::ALL {
            let out = dsd64(order);
            let f = on_bin(out.rate, 1000.0);
            let mut enc = Encoder::new(out, 1);
            let (sp, _) = measure(&mut enc, &sine(1.0, f), false);
            // The tone's phase at the first measured sample, against the input's at that time.
            let fs = out.rate.hz() as f64;
            let lag = (2.0 * PI * f * (16 * SETTLE) as f64 / fs - sp.phase(f)).rem_euclid(2.0 * PI) * fs / (2.0 * PI * f);
            println!("{}: delay {:.4} DSD samples, design {:.4}; latency_frames {}", name(out), lag, 16.0 * enc.design.latency, enc.latency_frames());
            assert!((lag - 16.0 * enc.design.latency).abs() < 0.05);
            assert_eq!(enc.latency_frames(), enc.design.latency.ceil() as usize);
        }
    }

    // ---- round trip through the decoder -----------------------------------------------------

    /// Amplitude of `freq` in `x` and the RMS of everything else (least-squares sinusoid).
    fn tone_and_residual(x: &[f64], rate: f64, freq: f64) -> (f64, f64) {
        let w = |i: usize| 2.0 * PI * freq * i as f64 / rate;
        let (re, im) = x.iter().enumerate().fold((0.0, 0.0), |(a, b), (i, v)| (a + v * w(i).cos(), b + v * w(i).sin()));
        let (re, im) = (2.0 * re / x.len() as f64, 2.0 * im / x.len() as f64);
        let resid = x.iter().enumerate().map(|(i, v)| (v - re * w(i).cos() - im * w(i).sin()).powi(2)).sum::<f64>() / x.len() as f64;
        ((re * re + im * im).sqrt(), resid.sqrt())
    }

    fn check_round_trip(out: DsdOut) {
        let wire = out.rate.wire_rate() as f64;
        let frames = (0.3 * wire) as usize;
        let pcm: Vec<f32> = (0..frames)
            .flat_map(|i| [1000.0, 3000.0].map(|f| (0.5 * (2.0 * PI * f * i as f64 / wire).sin()) as f32))
            .collect();
        let mut words = Vec::new();
        Encoder::new(out, 2).process(&pcm, &mut words);
        let runs: Vec<Vec<u8>> = (0..2).map(|c| words.iter().skip(c).step_by(2).flat_map(|w| w.to_be_bytes()).collect()).collect();
        let mut conv = crate::dsd::Converter::new(out.rate.hz(), 2).unwrap();
        let mut dec = Vec::new();
        conv.process(&runs, &mut dec);
        let rate = crate::dsd::pcm_rate(out.rate.hz()) as f64;
        let lp = kaiser_lowpass(511, 20_000.0 / rate, kaiser_beta(120.0));
        for (c, f) in [(0, 1000.0), (1, 3000.0)] {
            let ch: Vec<f64> = dec.iter().skip(c).step_by(2).map(|&v| v as f64).collect();
            let start = (0.05 * rate) as usize;
            let x: Vec<f64> = (start..start + (0.2 * rate) as usize).map(|i| lp.iter().enumerate().map(|(k, g)| g * ch[i + k]).sum()).collect();
            let (amp, resid) = tone_and_residual(&x, rate, f);
            let want = 0.5 * LEVEL * crate::dsd::PCM_GAIN;
            let snr = 20.0 * (amp / resid).log10();
            println!("{} round trip ch{c}: amplitude {amp:.6} (want {want:.6}, {:+.3} %), residual {snr:.1} dB down", name(out), 100.0 * (amp / want - 1.0));
            assert!((amp / want - 1.0).abs() <= 0.005 && snr >= 90.0, "{} ch{c}: {amp} {snr}", name(out));
        }
    }

    #[test]
    fn round_trip_through_the_decoder() {
        for order in ModOrder::ALL {
            check_round_trip(dsd64(order));
        }
    }

    #[test]
    #[ignore]
    fn round_trip_all_variants() {
        for out in all() {
            check_round_trip(out);
        }
    }

    /// The dither amount per variant: on a grid of 0.02, the smallest at which no line
    /// persists more than 11 dB above its neighbours in any of 20 cases (16 DC levels from
    /// 1e-5 to 3e-2 PCM, tones at −100, −60 and −20 dBFS, silence), the next amount up clean
    /// as well. Prints every amount tried, the SNR, and silence's noise against a full-scale
    /// tone's.
    /// `cargo test --release -p hifi-engine -- --ignored dsd_encode::tests::dither_survey --nocapture`
    #[test]
    #[ignore]
    fn dither_survey() {
        let dcs: Vec<f64> = (0..16).map(|i| (if i % 2 == 0 { 1.0 } else { -1.0 }) * 1e-5 * 10f64.powf(i as f64 * 3.5 / 15.0)).collect();
        let worst_at = |out: DsdOut, amp: f64| -> (f64, f64, f64) {
            let d = Arc::new(Design { dither: amp, limits: [f64::INFINITY; 7], ..Design::new(out) });
            let f = on_bin(out.rate, 1000.0);
            let mut enc = Encoder::with_design(d, 1);
            let (loud, _) = measure(&mut enc, &sine(1.0, f), true);
            let ln = loud.noise(20.0, 20_000.0, &[f]);
            let mut worst: f64 = 0.0;
            for &dc in &dcs {
                enc.reset();
                let (a, b, _) = measure2(&mut enc, &|_| dc);
                worst = worst.max(persistent_line(&a, &b, 20.0, 20_000.0, &[]).1);
            }
            for sig in [&sine(1e-5, f) as &dyn Fn(f64) -> f64, &sine(1e-3, f), &sine(0.1, f)] {
                enc.reset();
                let (a, b, _) = measure2(&mut enc, sig);
                worst = worst.max(persistent_line(&a, &b, 20.0, 20_000.0, &[f, 2.0 * f, 3.0 * f]).1);
            }
            enc.reset();
            let (q, q2, _) = measure2(&mut enc, &|_| 0.0);
            worst = worst.max(persistent_line(&q, &q2, 20.0, 20_000.0, &[]).1);
            (worst, db(FULL / ln), db(q.noise(20.0, 20_000.0, &[]) / ln))
        };
        let rows: Vec<String> = std::thread::scope(|sc| {
            let hs: Vec<_> = all()
                .into_iter()
                .map(|out| {
                    sc.spawn(move || {
                        let mut line = format!("{} (H∞ {}):", name(out), H_INF[out.order.index()][out.rate.index()]);
                        let amps: Vec<f64> = (0..=12).map(|i| 0.06 + 0.02 * i as f64).collect();
                        let res: Vec<(f64, f64, f64)> = amps.iter().map(|&a| worst_at(out, a)).collect();
                        let mut chosen = None;
                        for (i, (&a, r)) in amps.iter().zip(&res).enumerate() {
                            line += &format!("\n  {a:.2}: worst line {:.1} dB, SNR {:.1}, silence noise {:+.2} dB", r.0, r.1, r.2);
                            if chosen.is_none() && r.0 <= 11.0 && res.get(i + 1).is_some_and(|n| n.0 <= 11.0) {
                                chosen = Some(a);
                            }
                        }
                        line + &format!("\n  chosen {chosen:?}")
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for r in rows {
            println!("{r}");
        }
    }
}
