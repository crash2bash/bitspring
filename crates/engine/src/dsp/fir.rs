//! Long FIR filters by FFT convolution, on the render thread: linear-phase EQ, free-form
//! (GraphicEQ) curves, and convolution with impulse responses (room correction, headphone
//! IRs made with REW, rePhase, HeSuVi-style mono/stereo files).
//!
//! The convolver is uniformly partitioned overlap-save: the filter is cut into P blocks of B
//! taps, the input's spectra sit in a frequency-domain delay line, and each output block is
//! the sum of P spectral products. A new filter is swapped in on a block boundary; for that
//! one block both filters run on the same delay line and their outputs are crossfaded, so an
//! edit never clicks. The filter's own delay (half its length for linear phase, the main
//! peak of an IR) is skipped at the start and flushed at the end, so the music stays aligned
//! with the position shown.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use super::eq::{EqPreset, Phase};
use super::resample::{self, ResampleQuality, Resampler};
use crate::UserError;

type C = Complex<f64>;

/// Longest impulse response we take (seconds) — rooms rarely need more than 1, reverbs ~4.
pub const MAX_IR_SECONDS: f64 = 6.0;

/// An impulse response as read from its file.
pub struct ImpulseResponse {
    pub name: String,
    pub rate: u32,
    /// Per channel (1 = applied to every channel, 2 = left and right).
    pub channels: Vec<Vec<f64>>,
}

impl fmt::Debug for ImpulseResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ImpulseResponse({}, {} Hz, {}×{})", self.name, self.rate, self.channels.len(), self.len())
    }
}

impl ImpulseResponse {
    /// Read a WAV/FLAC/AIFF… impulse response.
    pub fn load(path: &Path) -> Result<Arc<Self>> {
        let track = crate::decode::load_track(path).map_err(|e| UserError::IrUnreadable { detail: e.to_string() })?;
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Self::from_samples(name, track.sample_rate, track.channels, track.frames(), |f, c| track.sample(f, c) as f64)
    }

    pub fn from_samples(
        name: String,
        rate: u32,
        channels: usize,
        frames: usize,
        sample: impl Fn(usize, usize) -> f64,
    ) -> Result<Arc<Self>> {
        if !(1..=2).contains(&channels) {
            bail!(UserError::IrChannels { channels });
        }
        if frames == 0 || rate == 0 {
            bail!(UserError::IrEmpty);
        }
        if frames as f64 > rate as f64 * MAX_IR_SECONDS {
            bail!(UserError::IrTooLong { seconds: MAX_IR_SECONDS });
        }
        let mut chans: Vec<Vec<f64>> = (0..channels).map(|c| (0..frames).map(|f| sample(f, c)).collect()).collect();
        // Trailing silence costs CPU and says nothing.
        let end = chans.iter().map(|ch| ch.iter().rposition(|v| v.abs() > 1e-7).map_or(0, |i| i + 1)).max().unwrap_or(0);
        if end == 0 {
            bail!(UserError::IrEmpty);
        }
        chans.iter_mut().for_each(|c| c.truncate(end));
        Ok(Arc::new(Self { name, rate, channels: chans }))
    }

    pub fn len(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The response at the device rate (resampled, and scaled so its gain stays the same).
    fn at_rate(&self, fs: u32) -> Result<Vec<Vec<f64>>> {
        if self.rate == fs {
            return Ok(self.channels.clone());
        }
        if resample::phases(self.rate, fs) > resample::MAX_PHASES {
            bail!(UserError::Resample { from: self.rate, to: fs });
        }
        let scale = self.rate as f64 / fs as f64;
        Ok(self
            .channels
            .iter()
            .map(|ch| {
                let mut r = Resampler::new(self.rate, fs, 1, ResampleQuality::HIGH);
                let mut out = Vec::with_capacity((ch.len() as f64 / scale) as usize + 1024);
                // Lead in with silence: the resampler skips its own delay, and with it the
                // pre-ringing of whatever starts at the very first sample.
                r.process(&[0.0; LEAD_IN], &mut out);
                r.process(ch, &mut out);
                r.flush(&mut out);
                out.iter_mut().for_each(|v| *v *= scale);
                out
            })
            .collect())
    }
}

/// Silence put before an IR that gets resampled (input samples).
const LEAD_IN: usize = 256;

/// Where the main arrival is: the earliest channel's absolute peak.
fn peak_index(chans: &[Vec<f64>]) -> usize {
    chans
        .iter()
        .map(|c| c.iter().enumerate().fold((0, 0.0f64), |(bi, bv), (i, v)| if v.abs() > bv { (i, v.abs()) } else { (bi, bv) }).0)
        .min()
        .unwrap_or(0)
}

/// Taps of the EQ's FIR at this rate (power of two). ~0.68 s: fine enough for a narrow cut
/// at 20 Hz.
fn eq_taps(fs: f64) -> usize {
    let base = 32_768.0 * (fs / 48_000.0);
    (base.round() as usize).next_power_of_two().max(8192)
}

fn plan(n: usize) -> (Arc<dyn Fft<f64>>, Arc<dyn Fft<f64>>) {
    let mut p = FftPlanner::new();
    (p.plan_fft_forward(n), p.plan_fft_inverse(n))
}

/// Linear-phase FIR of `n` taps, centred on n/2, with the magnitude `db(f)`.
fn linear_phase(n: usize, fs: f64, db: &dyn Fn(f64) -> f64) -> Vec<f64> {
    let (_, inv) = plan(n);
    let mut spec: Vec<C> = (0..n)
        .map(|k| {
            let f = k.min(n - k) as f64 * fs / n as f64;
            C::new(10f64.powf(db(f) / 20.0), 0.0)
        })
        .collect();
    inv.process(&mut spec);
    let scale = 1.0 / n as f64;
    (0..n)
        .map(|i| {
            // Blackman window, peak at the centre tap.
            let x = 2.0 * std::f64::consts::PI * i as f64 / n as f64;
            let w = 0.42 - 0.5 * x.cos() + 0.08 * (2.0 * x).cos();
            spec[(i + n / 2) % n].re * scale * w
        })
        .collect()
}

/// Minimum-phase FIR of `n` taps with the magnitude `db(f)` (homomorphic method: fold the real
/// cepstrum of the log magnitude onto positive time).
fn minimum_phase(n: usize, fs: f64, db: &dyn Fn(f64) -> f64) -> Vec<f64> {
    let m = 4 * n;
    let (fwd, inv) = plan(m);
    let mut c: Vec<C> = (0..m)
        .map(|k| {
            let f = k.min(m - k) as f64 * fs / m as f64;
            C::new(db(f).max(-200.0) / 20.0 * std::f64::consts::LN_10, 0.0)
        })
        .collect();
    inv.process(&mut c);
    let scale = 1.0 / m as f64;
    for (i, v) in c.iter_mut().enumerate() {
        let fold = match i {
            0 => 1.0,
            _ if i < m / 2 => 2.0,
            _ if i == m / 2 => 1.0,
            _ => 0.0,
        };
        *v = C::new(v.re * scale * fold, 0.0);
    }
    fwd.process(&mut c);
    for v in c.iter_mut() {
        *v = v.exp();
    }
    inv.process(&mut c);
    let fade = n / 8;
    (0..n)
        .map(|i| {
            let w = if i + fade < n { 1.0 } else { 0.5 + 0.5 * (std::f64::consts::PI * (i + fade + 1 - n) as f64 / fade as f64).cos() };
            c[i].re * scale * w
        })
        .collect()
}

/// Full linear convolution of two tap sets.
fn convolve(a: &[f64], b: &[f64]) -> Vec<f64> {
    let len = a.len() + b.len() - 1;
    let n = len.next_power_of_two();
    let (fwd, inv) = plan(n);
    let spec = |x: &[f64]| {
        let mut v: Vec<C> = x.iter().map(|&s| C::new(s, 0.0)).chain(std::iter::repeat(C::new(0.0, 0.0))).take(n).collect();
        fwd.process(&mut v);
        v
    };
    let (sa, sb) = (spec(a), spec(b));
    let mut y: Vec<C> = sa.iter().zip(&sb).map(|(x, y)| x * y).collect();
    inv.process(&mut y);
    y.iter().take(len).map(|v| v.re / n as f64).collect()
}

/// A filter cut into partitions, as spectra: `[channel][partition][2B bins]`. Channels with
/// identical taps share one Arc.
pub struct FirFilters {
    chans: Vec<Arc<Vec<Vec<C>>>>,
}

/// Everything fixed for one playback (rate, block size, length budget, the IR at this rate),
/// so new filters for live edits come out the same shape and can be swapped in seamlessly.
pub struct FirDesigner {
    fs: f64,
    channels: usize,
    /// Taps of the EQ part (0: the EQ needs no FIR).
    eq_len: usize,
    linear: bool,
    ir: Option<Vec<Vec<f64>>>,
    /// Block size B and partition count P.
    pub block: usize,
    pub parts: usize,
    /// Output samples to skip so the music stays in time (the filter's own delay).
    pub delay: usize,
}

impl FirDesigner {
    /// None when neither the EQ nor a convolution needs this stage.
    pub fn new(eq: Option<&EqPreset>, ir: Option<&ImpulseResponse>, fs: f64, channels: usize) -> Result<Option<Self>> {
        let eq_fir = eq.filter(|e| e.needs_fir());
        if eq_fir.is_none() && ir.is_none() {
            return Ok(None);
        }
        let linear = eq_fir.is_some_and(|e| e.phase == Phase::Linear);
        let eq_len = match eq_fir {
            None => 0,
            Some(_) if linear => eq_taps(fs),
            Some(_) => eq_taps(fs) / 2,
        };
        let ir = ir.map(|r| r.at_rate(fs.round() as u32)).transpose()?;
        let ir_len = ir.as_ref().map_or(0, |r| r[0].len());
        let ir_delay = ir.as_ref().map_or(0, |r| peak_index(r));
        let total = eq_len + ir_len.max(1) - 1;
        let block = (total / 16).next_power_of_two().clamp(1024, 16_384);
        Ok(Some(Self {
            fs,
            channels,
            eq_len,
            linear,
            ir,
            block,
            parts: total.max(1).div_ceil(block),
            delay: if linear { eq_len / 2 } else { 0 } + ir_delay,
        }))
    }

    /// The EQ has a part in this stage (else it holds only a convolution).
    pub fn has_eq(&self) -> bool {
        self.eq_len > 0
    }

    /// True when `eq` can be applied by a new design (same FIR layout as at the start).
    pub fn fits(&self, eq: Option<&EqPreset>) -> bool {
        let wants = eq.filter(|e| e.needs_fir());
        match wants {
            None => self.eq_len == 0,
            Some(e) => self.eq_len > 0 && self.linear == (e.phase == Phase::Linear),
        }
    }

    /// The filters for `eq` (bypass: the EQ part becomes a plain delay of the same length).
    pub fn design(&self, eq: Option<&EqPreset>, bypass: bool) -> Arc<FirFilters> {
        let taps_for = |ch: usize| -> Vec<f64> {
            let eq_part = (self.eq_len > 0).then(|| {
                let n = self.eq_len;
                match eq {
                    Some(e) if !bypass => {
                        let fs = self.fs;
                        if self.linear {
                            // With the preamp and balance (see IirPlan::new).
                            let gain = e.channel_gain_db(ch, fs);
                            linear_phase(n, fs, &|f| e.shape_db(f, fs, ch) + gain)
                        } else {
                            minimum_phase(n, fs, &|f| e.curve_db(f))
                        }
                    }
                    _ => {
                        let mut d = vec![0.0; n];
                        d[if self.linear { n / 2 } else { 0 }] = 1.0;
                        d
                    }
                }
            });
            let ir_part = self.ir.as_ref().map(|r| r[ch % r.len()].clone());
            match (eq_part, ir_part) {
                (Some(a), Some(b)) => convolve(&a, &b),
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => vec![1.0],
            }
        };
        // Channels differ only with per-channel bands or a stereo IR.
        let per_channel = self.ir.as_ref().is_some_and(|r| r.len() > 1)
            || (self.linear
                && eq.is_some_and(|e| {
                    e.balance != 0.0 || e.bands.iter().any(|b| b.enabled && b.channels != super::eq::Channels::Both)
                }));
        let (fwd, _) = plan(2 * self.block);
        let partition = |taps: Vec<f64>| -> Arc<Vec<Vec<C>>> {
            Arc::new(
                (0..self.parts)
                    .map(|p| {
                        let mut v = vec![C::new(0.0, 0.0); 2 * self.block];
                        for (i, slot) in v.iter_mut().take(self.block).enumerate() {
                            if let Some(&t) = taps.get(p * self.block + i) {
                                *slot = C::new(t, 0.0);
                            }
                        }
                        fwd.process(&mut v);
                        v
                    })
                    .collect(),
            )
        };
        let chans = if per_channel {
            (0..self.channels).map(|ch| partition(taps_for(ch))).collect()
        } else {
            let one = partition(taps_for(0));
            vec![one; self.channels]
        };
        Arc::new(FirFilters { chans })
    }
}

/// The FIR stage's shared half: the designer, and a slot new filters arrive in.
pub struct FirControl {
    pub designer: FirDesigner,
    slot: Mutex<Option<Arc<FirFilters>>>,
}

impl FirControl {
    pub fn new(designer: FirDesigner) -> Arc<Self> {
        Arc::new(Self { designer, slot: Mutex::new(None) })
    }

    pub fn post(&self, filters: Arc<FirFilters>) {
        *self.slot.lock().unwrap() = Some(filters);
    }

    fn take(&self) -> Option<Arc<FirFilters>> {
        self.slot.try_lock().ok().and_then(|mut s| s.take())
    }
}

struct ConvChan {
    /// Previous and current input block (overlap-save keeps one block of history).
    prev: Vec<f64>,
    cur: Vec<f64>,
    /// Input spectra, newest at `head`.
    fdl: Vec<Vec<C>>,
    out: Vec<f64>,
}

/// Running convolution of interleaved audio.
pub struct Convolver {
    control: Arc<FirControl>,
    b: usize,
    p: usize,
    channels: usize,
    fwd: Arc<dyn Fft<f64>>,
    inv: Arc<dyn Fft<f64>>,
    scratch: Vec<C>,
    buf: Vec<C>,
    acc: Vec<C>,
    acc_next: Vec<C>,
    filters: Arc<FirFilters>,
    next: Option<Arc<FirFilters>>,
    chans: Vec<ConvChan>,
    head: usize,
    /// Frames in the current input block.
    fill: usize,
    /// Real input frames taken since the start / last reset.
    fed: usize,
    /// Output frames computed (including the skipped delay).
    produced: usize,
}

impl Convolver {
    pub fn new(control: Arc<FirControl>, first: Arc<FirFilters>) -> Self {
        let (b, p, channels) = (control.designer.block, control.designer.parts, control.designer.channels);
        let (fwd, inv) = plan(2 * b);
        let scratch_len = fwd.get_inplace_scratch_len().max(inv.get_inplace_scratch_len());
        Self {
            b,
            p,
            channels,
            scratch: vec![C::new(0.0, 0.0); scratch_len],
            buf: vec![C::new(0.0, 0.0); 2 * b],
            acc: vec![C::new(0.0, 0.0); 2 * b],
            acc_next: vec![C::new(0.0, 0.0); 2 * b],
            fwd,
            inv,
            filters: first,
            next: None,
            chans: (0..channels)
                .map(|_| ConvChan { prev: vec![0.0; b], cur: vec![0.0; b], fdl: vec![vec![C::new(0.0, 0.0); 2 * b]; p], out: vec![0.0; b] })
                .collect(),
            head: 0,
            fill: 0,
            fed: 0,
            produced: 0,
            control,
        }
    }

    pub fn reset(&mut self) {
        for c in &mut self.chans {
            c.prev.fill(0.0);
            c.cur.fill(0.0);
            c.fdl.iter_mut().for_each(|s| s.fill(C::new(0.0, 0.0)));
        }
        if let Some(n) = self.next.take() {
            self.filters = n;
        }
        self.fill = 0;
        self.fed = 0;
        self.produced = 0;
    }

    /// Feed interleaved input, append interleaved output. `last` pushes the tail out so the
    /// output ends up exactly as long as the input.
    pub fn process(&mut self, input: &[f64], out: &mut Vec<f64>, last: bool) {
        if let Some(f) = self.control.take() {
            self.next = Some(f);
        }
        let ch = self.channels;
        for frame in input.chunks_exact(ch) {
            for (c, &s) in frame.iter().enumerate() {
                self.chans[c].cur[self.fill] = s;
            }
            self.fill += 1;
            self.fed += 1;
            if self.fill == self.b {
                self.block(out);
            }
        }
        if last {
            let delay = self.control.designer.delay;
            while self.produced < delay + self.fed {
                for c in &mut self.chans {
                    c.cur[self.fill..].fill(0.0);
                }
                self.block(out);
            }
        }
    }

    fn block(&mut self, out: &mut Vec<f64>) {
        let (b, p) = (self.b, self.p);
        let scale = 1.0 / (2 * b) as f64;
        let fading = self.next.take();
        for (c, chan) in self.chans.iter_mut().enumerate() {
            for i in 0..b {
                self.buf[i] = C::new(chan.prev[i], 0.0);
                self.buf[b + i] = C::new(chan.cur[i], 0.0);
            }
            self.fwd.process_with_scratch(&mut self.buf, &mut self.scratch);
            chan.fdl[self.head].copy_from_slice(&self.buf);

            let mac = |acc: &mut Vec<C>, h: &Vec<Vec<C>>| {
                acc.fill(C::new(0.0, 0.0));
                for (j, part) in h.iter().enumerate().take(p) {
                    let x = &chan.fdl[(self.head + p - j) % p];
                    for ((a, x), y) in acc.iter_mut().zip(x).zip(part) {
                        *a += x * y;
                    }
                }
            };
            mac(&mut self.acc, &self.filters.chans[c]);
            self.inv.process_with_scratch(&mut self.acc, &mut self.scratch);
            match &fading {
                Some(next) => {
                    mac(&mut self.acc_next, &next.chans[c]);
                    self.inv.process_with_scratch(&mut self.acc_next, &mut self.scratch);
                    for i in 0..b {
                        let t = (i as f64 + 0.5) / b as f64;
                        chan.out[i] = (self.acc[b + i].re * (1.0 - t) + self.acc_next[b + i].re * t) * scale;
                    }
                }
                None => {
                    for i in 0..b {
                        chan.out[i] = self.acc[b + i].re * scale;
                    }
                }
            }
            std::mem::swap(&mut chan.prev, &mut chan.cur);
        }
        if let Some(n) = fading {
            self.filters = n;
        }
        self.head = (self.head + 1) % p;
        self.fill = 0;

        let delay = self.control.designer.delay;
        for i in 0..b {
            let n = self.produced + i;
            if n >= delay && n - delay < self.fed {
                out.extend(self.chans.iter().map(|c| c.out[i]));
            }
        }
        self.produced += b;
    }
}

#[cfg(test)]
mod tests {
    use super::super::eq::{Band, FilterKind};
    use super::*;

    fn run(conv: &mut Convolver, input: &[f64], chunk: usize) -> Vec<f64> {
        let mut out = Vec::new();
        let chunks: Vec<&[f64]> = input.chunks(chunk).collect();
        for (i, c) in chunks.iter().enumerate() {
            conv.process(c, &mut out, i + 1 == chunks.len());
        }
        out
    }

    fn ir(taps: Vec<f64>) -> Arc<ImpulseResponse> {
        let n = taps.len();
        ImpulseResponse::from_samples("t".into(), 48_000, 1, n, |f, _| taps[f]).unwrap()
    }

    #[test]
    fn convolution_matches_direct_and_keeps_length_and_timing() {
        // IR: peak at tap 3, then a small echo.
        let mut taps = vec![0.0; 3000];
        taps[3] = 1.0;
        taps[2500] = 0.25;
        let ir = ir(taps.clone());
        let d = FirDesigner::new(None, Some(&ir), 48_000.0, 1).unwrap().unwrap();
        assert_eq!(d.delay, 3);
        let control = FirControl::new(d);
        let first = control.designer.design(None, false);
        let mut conv = Convolver::new(control, first);
        let input: Vec<f64> = (0..20_000).map(|i| ((i * 7919) % 1000) as f64 / 1000.0 - 0.5).collect();
        let out = run(&mut conv, &input, 777);
        assert_eq!(out.len(), input.len());
        for n in 0..input.len() {
            // Delay compensated: y[n] = x[n] + 0.25·x[n − 2497].
            let want = input[n] + if n >= 2497 { 0.25 * input[n - 2497] } else { 0.0 };
            assert!((out[n] - want).abs() < 1e-9, "n={n}: {} vs {want}", out[n]);
        }
    }

    #[test]
    fn linear_phase_eq_has_the_designed_gain_and_no_shift() {
        let fs = 48_000.0;
        let eq = EqPreset {
            phase: Phase::Linear,
            bands: vec![Band { kind: FilterKind::Peak, freq: 1000.0, gain: 6.0, q: 1.0, ..Default::default() }],
            ..Default::default()
        };
        let d = FirDesigner::new(Some(&eq), None, fs, 1).unwrap().unwrap();
        assert_eq!(d.delay, eq_taps(fs) / 2);
        let control = FirControl::new(d);
        let first = control.designer.design(Some(&eq), false);
        let mut conv = Convolver::new(control, first);
        let input: Vec<f64> = (0..96_000).map(|i| (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / fs).sin() * 0.1).collect();
        let out = run(&mut conv, &input, 2048);
        assert_eq!(out.len(), input.len());
        // Zero phase: the output is the input scaled by 2 (+6 dB), sample for sample.
        for n in 40_000..60_000 {
            assert!((out[n] - input[n] * 10f64.powf(6.0 / 20.0)).abs() < 2e-3, "n={n}");
        }
    }

    #[test]
    fn minimum_phase_curve_follows_the_curve() {
        let fs = 48_000.0;
        let eq = EqPreset { curve: vec![[200.0, -6.0], [2000.0, 6.0]], ..Default::default() };
        let d = FirDesigner::new(Some(&eq), None, fs, 1).unwrap().unwrap();
        assert_eq!(d.delay, 0);
        let taps = minimum_phase(d.eq_len, fs, &|f| eq.curve_db(f));
        // Check the magnitude response at a few points.
        for (f, want) in [(100.0, -6.0), (632.0, 0.0), (5000.0, 6.0)] {
            let w = 2.0 * std::f64::consts::PI * f / fs;
            let (re, im) = taps.iter().enumerate().fold((0.0, 0.0), |(r, i), (n, &h)| (r + h * (w * n as f64).cos(), i - h * (w * n as f64).sin()));
            let db = 10.0 * (re * re + im * im).log10();
            assert!((db - want).abs() < 0.3, "{f} Hz: {db}");
        }
    }

    #[test]
    fn filter_swap_is_seamless() {
        let fs = 48_000.0;
        let mut eq = EqPreset { phase: Phase::Linear, ..Default::default() };
        let d = FirDesigner::new(Some(&eq), None, fs, 1).unwrap().unwrap();
        let control = FirControl::new(d);
        let first = control.designer.design(Some(&eq), false);
        let mut conv = Convolver::new(control.clone(), first);
        let input: Vec<f64> = (0..96_000).map(|i| (2.0 * std::f64::consts::PI * 440.0 * i as f64 / fs).sin() * 0.1).collect();
        let mut out = Vec::new();
        conv.process(&input[..40_000], &mut out, false);
        eq.preamp = 0.0;
        eq.bands.push(Band { kind: FilterKind::Peak, freq: 440.0, gain: -6.0, q: 1.0, ..Default::default() });
        control.post(control.designer.design(Some(&eq), false));
        conv.process(&input[40_000..], &mut out, true);
        let max_step = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f64, f64::max);
        assert!(max_step < 0.0062, "step {max_step}");
        // Well after the swap, and before the filter starts seeing the end of the input.
        let tail = out[60_000..75_000].iter().fold(0.0f64, |m, v| m.max(v.abs()));
        assert!((20.0 * (tail / 0.1).log10() + 6.0).abs() < 0.05);
    }

    /// How much faster than real time the heaviest case runs: linear phase at 384 kHz.
    /// `cargo test --release -p hifi-engine -- --ignored --nocapture realtime`
    #[test]
    #[ignore]
    fn realtime_factor_linear_phase_384k() {
        let fs = 384_000.0;
        let eq = EqPreset {
            phase: Phase::Linear,
            bands: (0..10).map(|i| Band { kind: FilterKind::Peak, freq: 30.0 * 2f64.powi(i), gain: 3.0, q: 1.4, ..Default::default() }).collect(),
            ..Default::default()
        };
        let control = FirControl::new(FirDesigner::new(Some(&eq), None, fs, 2).unwrap().unwrap());
        let t0 = std::time::Instant::now();
        let first = control.designer.design(Some(&eq), false);
        let design = t0.elapsed();
        let mut conv = Convolver::new(control, first);
        let input = vec![0.1f64; 384_000 * 2 * 5];
        let t0 = std::time::Instant::now();
        let mut out = Vec::new();
        for c in input.chunks(4096) {
            out.clear();
            conv.process(c, &mut out, false);
        }
        let el = t0.elapsed().as_secs_f64();
        println!("design {design:?}; 5 s of stereo 384k in {el:.3} s → {:.0}× real time", 5.0 / el);
    }

    #[test]
    fn rejects_bad_irs() {
        assert!(ImpulseResponse::from_samples("x".into(), 48_000, 3, 10, |_, _| 1.0).is_err());
        assert!(ImpulseResponse::from_samples("x".into(), 48_000, 1, 10, |_, _| 0.0).is_err());
        assert!(ImpulseResponse::from_samples("x".into(), 1000, 1, 7000, |_, _| 1.0).is_err());
    }

    #[test]
    fn ir_resampled_keeps_its_gain() {
        // A 1 kHz-ish lowpass-ish IR (box of 8 taps, DC gain 1) at 44.1k, used at 96k.
        let r = ImpulseResponse::from_samples("x".into(), 44_100, 1, 8, |_, _| 0.125).unwrap();
        let at = r.at_rate(96_000).unwrap();
        let dc: f64 = at[0].iter().sum();
        assert!((dc - 1.0).abs() < 0.01, "{dc}");
    }
}
