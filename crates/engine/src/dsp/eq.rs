//! Equaliser: the preset model, filter design, and the live (realtime) parametric stage.
//!
//! A preset is a list of parametric bands (peak, shelves, passes, notch, band-pass, all-pass,
//! each for both channels or just one), a preamp, a channel balance, an optional free-form
//! curve (GraphicEQ) and a phase mode:
//!
//! * **Minimum phase** — the bands run as biquads (RBJ Audio EQ Cookbook, f64) inside the audio
//!   callback itself, so every edit is heard within one device buffer, crossfaded so it never
//!   clicks. A curve, if any, runs as a minimum-phase FIR on the render thread ([`super::fir`]).
//! * **Linear phase** — bands and curve become one symmetric FIR (no phase shift at all, at the
//!   cost of pre-ringing and ~0.3 s of latency the player compensates). Edits are heard after
//!   the render buffer (~0.5 s).
//!
//! Preamp and balance always stay in the callback, as does "compare": a level-matched bypass
//! that swaps the EQ for a plain gain equal to its average loudness, so A/B judges the tone,
//! not the volume.

use std::cell::UnsafeCell;
use std::f64::consts::PI;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum FilterKind {
    #[default]
    Peak,
    LowShelf,
    HighShelf,
    LowPass,
    HighPass,
    BandPass,
    Notch,
    AllPass,
}

impl FilterKind {
    /// Has a gain parameter.
    pub fn has_gain(self) -> bool {
        matches!(self, Self::Peak | Self::LowShelf | Self::HighShelf)
    }
    /// Steepness is set as a slope (dB/oct) rather than only Q.
    pub fn has_slope(self) -> bool {
        matches!(self, Self::LowPass | Self::HighPass)
    }
}

/// Which channels a band applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Channels {
    #[default]
    Both,
    Left,
    Right,
}

impl Channels {
    fn covers(self, ch: usize) -> bool {
        match self {
            Self::Both => true,
            Self::Left => ch == 0,
            Self::Right => ch == 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    #[default]
    Minimum,
    Linear,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Band {
    pub enabled: bool,
    pub kind: FilterKind,
    /// Centre / corner frequency, Hz.
    pub freq: f64,
    /// dB, for peak and shelves.
    pub gain: f64,
    pub q: f64,
    /// Low/high-pass steepness in dB per octave: 6, 12, … 48. At 12 the filter is one biquad
    /// with this band's Q; other slopes are Butterworth.
    pub slope: u32,
    pub channels: Channels,
}

impl Default for Band {
    fn default() -> Self {
        Self { enabled: true, kind: FilterKind::Peak, freq: 1000.0, gain: 0.0, q: 1.0, slope: 12, channels: Channels::Both }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct EqPreset {
    pub name: String,
    /// dB. Ignored when `auto_preamp` is on.
    pub preamp: f64,
    /// Lower the level by exactly the curve's highest boost, so the EQ can never clip.
    pub auto_preamp: bool,
    /// dB; positive turns the left channel down (image moves right), negative the right.
    pub balance: f64,
    pub bands: Vec<Band>,
    /// Free-form curve as [Hz, dB] points, interpolated on a log-frequency axis (GraphicEQ).
    pub curve: Vec<[f64; 2]>,
    pub phase: Phase,
}

/// Lowest and highest gains we design for: beyond this a filter is a mistake, not a setting.
const GAIN_LIMIT: f64 = 30.0;

impl EqPreset {
    /// Clamp every value into a range the filters can be designed for, drop NaNs, sort the
    /// curve. Imported and UI-sent presets pass through this before use.
    pub fn sanitized(mut self) -> Self {
        let fin = |v: f64, d: f64| if v.is_finite() { v } else { d };
        self.preamp = fin(self.preamp, 0.0).clamp(-60.0, GAIN_LIMIT);
        self.balance = fin(self.balance, 0.0).clamp(-24.0, 24.0);
        for b in &mut self.bands {
            b.freq = fin(b.freq, 1000.0).clamp(10.0, 40_000.0);
            b.gain = fin(b.gain, 0.0).clamp(-GAIN_LIMIT, GAIN_LIMIT);
            b.q = fin(b.q, 0.707).clamp(0.05, 50.0);
            b.slope = (b.slope.clamp(6, 48) / 6) * 6;
        }
        self.curve.retain(|[f, g]| f.is_finite() && g.is_finite() && *f > 0.0);
        for p in &mut self.curve {
            p[1] = p[1].clamp(-GAIN_LIMIT, GAIN_LIMIT);
        }
        self.curve.sort_by(|a, b| a[0].total_cmp(&b[0]));
        self.curve.dedup_by(|a, b| a[0] == b[0]);
        self
    }

    /// Needs the FIR stage on the render thread (linear phase, or a free-form curve).
    pub fn needs_fir(&self) -> bool {
        self.phase == Phase::Linear || !self.curve.is_empty()
    }

    /// Bands for output channel `ch` (0 = left, 1 = right, others get the both-channel bands).
    fn bands_for(&self, ch: usize) -> impl Iterator<Item = &Band> {
        self.bands.iter().filter(move |b| b.enabled && b.channels.covers(ch))
    }

    /// The curve's gain at `freq` (0 with no curve).
    pub fn curve_db(&self, freq: f64) -> f64 {
        let c = &self.curve;
        match c.len() {
            0 => 0.0,
            _ if freq <= c[0][0] => c[0][1],
            _ if freq >= c[c.len() - 1][0] => c[c.len() - 1][1],
            _ => {
                let i = c.partition_point(|p| p[0] <= freq);
                let ([f0, g0], [f1, g1]) = (c[i - 1], c[i]);
                let t = (freq / f0).ln() / (f1 / f0).ln();
                g0 + (g1 - g0) * t
            }
        }
    }

    /// Gain of the bands and curve (no preamp/balance) at `freq` for channel `ch`.
    pub fn shape_db(&self, freq: f64, sample_rate: f64, ch: usize) -> f64 {
        let w = 2.0 * PI * freq / sample_rate;
        self.bands_for(ch).flat_map(|b| sections(b, sample_rate)).map(|s| s.magnitude_db(w)).sum::<f64>()
            + self.curve_db(freq)
    }

    /// Highest boost of the shape over the audible range, either channel (≥ 0).
    pub fn max_boost_db(&self, sample_rate: f64) -> f64 {
        let top = (sample_rate * 0.5).min(22_000.0);
        let mut freqs: Vec<f64> = (0..=480).map(|i| 10.0 * (top / 10.0f64).powf(i as f64 / 480.0)).collect();
        freqs.extend(self.bands.iter().filter(|b| b.enabled && b.freq < top).map(|b| b.freq));
        freqs.extend(self.curve.iter().map(|p| p[0]).filter(|f| *f < top));
        let channels = if self.bands.iter().any(|b| b.channels != Channels::Both) { 2 } else { 1 };
        (0..channels)
            .flat_map(|ch| freqs.iter().map(move |&f| self.shape_db(f, sample_rate, ch)))
            .fold(0.0f64, f64::max)
    }

    /// The preamp in effect (dB): the set one, or the automatic one.
    pub fn effective_preamp(&self, sample_rate: f64) -> f64 {
        if self.auto_preamp { -self.max_boost_db(sample_rate) } else { self.preamp }
    }

    /// Per-channel gain (dB) from preamp and balance.
    pub fn channel_gain_db(&self, ch: usize, sample_rate: f64) -> f64 {
        let pre = self.effective_preamp(sample_rate);
        match ch {
            0 => pre - self.balance.max(0.0),
            1 => pre - (-self.balance).max(0.0),
            _ => pre,
        }
    }

    /// Whole response at `freq` for channel `ch`, dB — what the curve editor draws.
    pub fn response_db(&self, freq: f64, sample_rate: f64, ch: usize) -> f64 {
        self.channel_gain_db(ch, sample_rate) + self.shape_db(freq, sample_rate, ch)
    }

    /// Average loudness change of the whole EQ (dB): the power mean over 40 Hz–16 kHz with
    /// equal weight per octave, which is roughly how music's energy spreads. The compare
    /// bypass plays at this gain so A/B is level-matched.
    pub fn loudness_db(&self, sample_rate: f64) -> f64 {
        let top = 16_000.0f64.min(sample_rate * 0.45);
        let n = 200;
        let mut sum = 0.0;
        let mut count = 0.0;
        for ch in 0..2 {
            for i in 0..=n {
                let f = 40.0 * (top / 40.0).powf(i as f64 / n as f64);
                sum += 10f64.powf(self.response_db(f, sample_rate, ch) / 10.0);
                count += 1.0;
            }
        }
        10.0 * (sum / count).log10()
    }
}

// ---------------------------------------------------------------------------------------------
// Filter design
// ---------------------------------------------------------------------------------------------

/// One second-order section, normalised (a0 = 1). First-order filters have b2 = a2 = 0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Biquad {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Biquad {
    pub const IDENTITY: Self = Self { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    fn norm(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
    }

    /// RBJ cookbook biquad of `kind` at `freq` (already below Nyquist).
    fn rbj(kind: FilterKind, freq: f64, gain_db: f64, q: f64, fs: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * freq / fs;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        match kind {
            FilterKind::Peak => Self::norm(1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a),
            FilterKind::LowShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                Self::norm(
                    a * ((a + 1.0) - (a - 1.0) * cos + s),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                    a * ((a + 1.0) - (a - 1.0) * cos - s),
                    (a + 1.0) + (a - 1.0) * cos + s,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                    (a + 1.0) + (a - 1.0) * cos - s,
                )
            }
            FilterKind::HighShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                Self::norm(
                    a * ((a + 1.0) + (a - 1.0) * cos + s),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                    a * ((a + 1.0) + (a - 1.0) * cos - s),
                    (a + 1.0) - (a - 1.0) * cos + s,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos),
                    (a + 1.0) - (a - 1.0) * cos - s,
                )
            }
            FilterKind::LowPass => Self::norm((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            FilterKind::HighPass => Self::norm((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            // Constant 0 dB peak gain.
            FilterKind::BandPass => Self::norm(alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            FilterKind::Notch => Self::norm(1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            FilterKind::AllPass => Self::norm(1.0 - alpha, -2.0 * cos, 1.0 + alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
        }
    }

    /// First-order (6 dB/oct) low- or high-pass, bilinear transform with prewarping.
    fn first_order(high: bool, freq: f64, fs: f64) -> Self {
        let k = (PI * freq / fs).tan();
        let a0 = k + 1.0;
        let a1 = (k - 1.0) / a0;
        if high { Self { b0: 1.0 / a0, b1: -1.0 / a0, b2: 0.0, a1, a2: 0.0 } } else { Self { b0: k / a0, b1: k / a0, b2: 0.0, a1, a2: 0.0 } }
    }

    /// |H(e^jw)| in dB.
    pub fn magnitude_db(&self, w: f64) -> f64 {
        let (c1, s1, c2, s2) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
        let num_re = self.b0 + self.b1 * c1 + self.b2 * c2;
        let num_im = -(self.b1 * s1 + self.b2 * s2);
        let den_re = 1.0 + self.a1 * c1 + self.a2 * c2;
        let den_im = -(self.a1 * s1 + self.a2 * s2);
        10.0 * ((num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im)).max(1e-30).log10()
    }
}

/// The biquads that realise `band` at `fs` (empty when the band does nothing at this rate).
pub fn sections(band: &Band, fs: f64) -> Vec<Biquad> {
    if !band.enabled {
        return Vec::new();
    }
    let nyq = fs * 0.5;
    let limit = fs * 0.49;
    let f = band.freq;
    let q = band.q.clamp(0.05, 50.0);
    let gain = band.gain.clamp(-GAIN_LIMIT, GAIN_LIMIT);
    match band.kind {
        // Above Nyquist these have no effect on anything we can play.
        FilterKind::Peak | FilterKind::Notch | FilterKind::BandPass | FilterKind::AllPass | FilterKind::LowPass
            if f >= limit =>
        {
            if band.kind == FilterKind::BandPass { vec![Biquad::rbj(FilterKind::BandPass, limit, 0.0, q, fs)] } else { Vec::new() }
        }
        FilterKind::HighShelf if f >= nyq => Vec::new(),
        FilterKind::Peak | FilterKind::LowShelf | FilterKind::HighShelf if gain == 0.0 => Vec::new(),
        FilterKind::LowPass | FilterKind::HighPass => {
            let high = band.kind == FilterKind::HighPass;
            let f = f.min(limit);
            let order = (band.slope.clamp(6, 48) / 6) as usize;
            if order == 2 {
                return vec![Biquad::rbj(band.kind, f, 0.0, q, fs)];
            }
            let mut out = Vec::with_capacity(order.div_ceil(2));
            if !order.is_multiple_of(2) {
                out.push(Biquad::first_order(high, f, fs));
            }
            // Butterworth pole pairs.
            for k in 1..=order / 2 {
                let theta = if order.is_multiple_of(2) { (2 * k - 1) as f64 * PI / (2 * order) as f64 } else { k as f64 * PI / order as f64 };
                out.push(Biquad::rbj(band.kind, f, 0.0, 1.0 / (2.0 * theta.cos()), fs));
            }
            out
        }
        kind => vec![Biquad::rbj(kind, f.min(limit), gain, q, fs)],
    }
}

// ---------------------------------------------------------------------------------------------
// The live stage (runs inside the audio callback)
// ---------------------------------------------------------------------------------------------

/// Coefficients and running state for every output channel, built off the realtime thread.
pub struct IirPlan {
    chans: Vec<ChanPlan>,
    /// Linear gain for the compare bypass (the EQ's average loudness).
    bypass_gain: f64,
    /// Same sections per channel as the plan it replaces: its state can be carried over.
    shape_key: Vec<usize>,
}

struct ChanPlan {
    gain: f64,
    sections: Vec<Biquad>,
    /// Transposed direct form II state per section.
    state: Vec<[f64; 2]>,
}

impl IirPlan {
    /// `bands`: false when the bands run on the FIR stage instead (linear phase). Preamp and
    /// balance then go there too, so a lower preamp and the boost it makes room for change at
    /// the same moment (the FIR is heard ~0.7 s after the callback: a preamp raised here while
    /// the old boost is still on its way would clip). The callback is left with the compare.
    pub fn new(eq: &EqPreset, fs: f64, channels: usize, bands: bool) -> Box<Self> {
        let chans: Vec<ChanPlan> = (0..channels)
            .map(|ch| {
                let sections: Vec<Biquad> =
                    if bands { eq.bands_for(ch).flat_map(|b| sections(b, fs)).collect() } else { Vec::new() };
                ChanPlan {
                    gain: if bands { 10f64.powf(eq.channel_gain_db(ch, fs) / 20.0) } else { 1.0 },
                    state: vec![[0.0; 2]; sections.len()],
                    sections,
                }
            })
            .collect();
        Box::new(Self {
            shape_key: chans.iter().map(|c| c.sections.len()).collect(),
            chans,
            bypass_gain: 10f64.powf(eq.loudness_db(fs) / 20.0),
        })
    }

    #[inline(always)]
    fn run(&mut self, ch: usize, x: f64) -> f64 {
        let c = &mut self.chans[ch];
        let mut v = x * c.gain;
        for (b, s) in c.sections.iter().zip(c.state.iter_mut()) {
            let y = b.b0 * v + s[0];
            s[0] = b.b1 * v - b.a1 * y + s[1];
            s[1] = b.b2 * v - b.a2 * y;
            v = y;
        }
        v
    }

    fn clear(&mut self) {
        self.chans.iter_mut().flat_map(|c| c.state.iter_mut()).for_each(|s| *s = [0.0; 2]);
    }
}

/// Hands new plans from a control thread to the callback without locks or allocation in the
/// callback: plans travel as raw pointers through two slots. `pending` holds the newest
/// unclaimed plan; `retired` holds the one the callback is done with, freed by the render
/// thread (never by the callback).
pub struct EqMailbox {
    pending: AtomicPtr<IirPlan>,
    retired: AtomicPtr<IirPlan>,
    bypass: AtomicBool,
}

impl EqMailbox {
    pub fn new() -> Self {
        Self { pending: AtomicPtr::new(ptr::null_mut()), retired: AtomicPtr::new(ptr::null_mut()), bypass: AtomicBool::new(false) }
    }

    /// Control side: replace whatever plan is still waiting.
    pub fn post(&self, plan: Box<IirPlan>) {
        let old = self.pending.swap(Box::into_raw(plan), Ordering::AcqRel);
        if !old.is_null() {
            // SAFETY: a pending plan was never claimed by the callback; we own it again.
            drop(unsafe { Box::from_raw(old) });
        }
    }

    pub fn set_bypass(&self, on: bool) {
        self.bypass.store(on, Ordering::Relaxed);
    }

    pub fn bypass(&self) -> bool {
        self.bypass.load(Ordering::Relaxed)
    }

    /// Non-realtime side: free the plan the callback retired.
    pub fn collect(&self) {
        let r = self.retired.swap(ptr::null_mut(), Ordering::AcqRel);
        if !r.is_null() {
            // SAFETY: the callback gave this plan up for good when it stored it here.
            drop(unsafe { Box::from_raw(r) });
        }
    }

    /// Callback side: claim the newest plan — only while the retired slot is free, so the
    /// plan it replaces always has somewhere to go.
    fn take(&self) -> Option<Box<IirPlan>> {
        if !self.retired.load(Ordering::Acquire).is_null() {
            return None;
        }
        let p = self.pending.swap(ptr::null_mut(), Ordering::AcqRel);
        // SAFETY: posted by `post` via Box::into_raw; the swap made us its only owner.
        (!p.is_null()).then(|| unsafe { Box::from_raw(p) })
    }

    fn retire(&self, plan: Box<IirPlan>) {
        let prev = self.retired.swap(Box::into_raw(plan), Ordering::AcqRel);
        // `take` only runs with the slot empty and one swap is in flight at a time, so this
        // is always null. If it ever weren't, leaking beats freeing on the realtime thread.
        debug_assert!(prev.is_null());
    }
}

impl Default for EqMailbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for EqMailbox {
    fn drop(&mut self) {
        for slot in [&self.pending, &self.retired] {
            let p = slot.swap(ptr::null_mut(), Ordering::AcqRel);
            if !p.is_null() {
                // SAFETY: nobody else can reach the mailbox any more.
                drop(unsafe { Box::from_raw(p) });
            }
        }
    }
}

/// The callback's own half: current plan, the one fading out, and the input history new
/// plans warm up on.
pub struct LiveEq {
    channels: usize,
    current: Box<IirPlan>,
    fading: Option<Box<IirPlan>>,
    fade_pos: usize,
    fade_len: usize,
    /// 0 = EQ, 1 = bypass; moves toward the mailbox's setting by 1/fade_len per frame.
    bypass_mix: f64,
    /// Recent input (interleaved, circular), so a new plan can settle before it is heard.
    history: Vec<f32>,
    history_head: usize,
    history_fill: usize,
}

impl LiveEq {
    pub fn new(plan: Box<IirPlan>, fs: f64, channels: usize, bypass: bool) -> Self {
        // ~20 ms crossfades; ~85 ms of history (enough for a 20 Hz filter to ring in).
        let fade_len = ((fs * 0.02) as usize).max(64);
        let history_frames = ((fs * 0.085) as usize).clamp(2048, 16_384);
        Self {
            channels,
            current: plan,
            fading: None,
            fade_pos: 0,
            fade_len,
            bypass_mix: if bypass { 1.0 } else { 0.0 },
            history: vec![0.0; history_frames * channels],
            history_head: 0,
            history_fill: 0,
        }
    }

    /// A seek or restart: forget filter state and history.
    pub fn reset(&mut self) {
        self.current.clear();
        self.history_fill = 0;
    }

    /// Process interleaved samples in place. Realtime-safe: no locks, no allocation.
    /// Returns the peak |sample| written (before clamping to full scale).
    pub fn process(&mut self, mailbox: &EqMailbox, data: &mut [f32]) -> f32 {
        let ch = self.channels;
        if self.fading.is_none()
            && let Some(mut next) = mailbox.take()
        {
            self.adopt(&mut next);
            let old = std::mem::replace(&mut self.current, next);
            self.fading = Some(old);
            self.fade_pos = 0;
        }
        let bypass_target = if mailbox.bypass() { 1.0 } else { 0.0 };
        let step = 1.0 / self.fade_len as f64;
        let mut peak = 0f32;
        for frame in data.chunks_exact_mut(ch) {
            self.remember(frame);
            let t = match &self.fading {
                Some(_) => (self.fade_pos as f64 + 1.0) * step,
                None => 1.0,
            };
            if self.bypass_mix != bypass_target {
                self.bypass_mix = if bypass_target > self.bypass_mix {
                    (self.bypass_mix + step).min(1.0)
                } else {
                    (self.bypass_mix - step).max(0.0)
                };
            }
            let bypass_gain = self.current.bypass_gain;
            for (c, s) in frame.iter_mut().enumerate() {
                let x = *s as f64;
                let mut y = self.current.run(c, x);
                if let Some(old) = &mut self.fading {
                    y = old.run(c, x) * (1.0 - t) + y * t;
                }
                if self.bypass_mix > 0.0 {
                    y = y * (1.0 - self.bypass_mix) + x * bypass_gain * self.bypass_mix;
                }
                let out = y as f32;
                peak = peak.max(out.abs());
                *s = out.clamp(-1.0, 1.0);
            }
            if self.fading.is_some() {
                self.fade_pos += 1;
                if self.fade_pos >= self.fade_len
                    && let Some(old) = self.fading.take()
                {
                    mailbox.retire(old);
                }
            }
        }
        peak
    }

    /// Bring `next` to steady state before it is heard: carry the state over when it has the
    /// same structure (a band being dragged), else run it over the recent input.
    fn adopt(&self, next: &mut IirPlan) {
        if next.shape_key == self.current.shape_key {
            for (n, c) in next.chans.iter_mut().zip(&self.current.chans) {
                n.state.copy_from_slice(&c.state);
            }
            return;
        }
        let ch = self.channels;
        let frames = self.history.len() / ch;
        let start = (self.history_head + frames - self.history_fill) % frames;
        for i in 0..self.history_fill {
            let f = (start + i) % frames;
            for c in 0..ch {
                next.run(c, self.history[f * ch + c] as f64);
            }
        }
    }

    #[inline(always)]
    fn remember(&mut self, frame: &[f32]) {
        let ch = self.channels;
        let frames = self.history.len() / ch;
        let at = self.history_head * ch;
        self.history[at..at + ch].copy_from_slice(frame);
        self.history_head = (self.history_head + 1) % frames;
        self.history_fill = (self.history_fill + 1).min(frames);
    }
}

/// The live stage as stored in the player: the mailbox (shared) and the callback's half.
pub struct EqStage {
    pub mailbox: EqMailbox,
    live: UnsafeCell<LiveEq>,
}

// SAFETY: `live` is touched only by the audio callback thread (see `live`); the mailbox is
// atomics only.
unsafe impl Sync for EqStage {}

impl EqStage {
    pub fn new(eq: &EqPreset, fs: f64, channels: usize, bypass: bool) -> Self {
        let mailbox = EqMailbox::new();
        mailbox.set_bypass(bypass);
        let plan = IirPlan::new(eq, fs, channels, eq.phase == Phase::Minimum);
        Self { mailbox, live: UnsafeCell::new(LiveEq::new(plan, fs, channels, bypass)) }
    }

    /// # Safety
    /// Only the audio callback may call this, and never re-entrantly.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn live(&self) -> &mut LiveEq {
        // SAFETY: upheld by the caller.
        unsafe { &mut *self.live.get() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peak(freq: f64, gain: f64, q: f64) -> Band {
        Band { kind: FilterKind::Peak, freq, gain, q, ..Default::default() }
    }

    fn preset(bands: Vec<Band>) -> EqPreset {
        EqPreset { bands, ..Default::default() }
    }

    fn sine(freq: f64, fs: f64, secs: f64, channels: usize) -> Vec<f32> {
        (0..(fs * secs) as usize)
            .flat_map(|i| std::iter::repeat_n(((2.0 * PI * freq * i as f64 / fs).sin() * 0.1) as f32, channels))
            .collect()
    }

    fn level_db(x: &[f32], skip: usize) -> f64 {
        20.0 * (x[skip..].iter().fold(0f32, |m, v| m.max(v.abs())) as f64 / 0.1).log10()
    }

    #[test]
    fn peak_has_its_gain_at_centre_frequency() {
        let p = preset(vec![peak(1000.0, 6.0, 1.0)]);
        assert!((p.response_db(1000.0, 48_000.0, 0) - 6.0).abs() < 0.01);
        assert!(p.response_db(100.0, 48_000.0, 0).abs() < 0.2);
    }

    #[test]
    fn butterworth_slopes_are_steep_as_promised() {
        for slope in [6u32, 12, 18, 24, 36, 48] {
            let b = Band { kind: FilterKind::HighPass, freq: 1000.0, q: std::f64::consts::FRAC_1_SQRT_2, slope, ..Default::default() };
            let p = preset(vec![b]);
            // −3 dB at the corner, and `slope` dB per octave well below it.
            assert!((p.response_db(1000.0, 96_000.0, 0) + 3.01).abs() < 0.1, "slope {slope} corner");
            let per_oct = p.response_db(125.0, 96_000.0, 0) - p.response_db(62.5, 96_000.0, 0);
            assert!((per_oct - slope as f64).abs() < 0.5, "slope {slope}: {per_oct}");
        }
    }

    #[test]
    fn notch_and_bandpass_and_allpass() {
        let fs = 48_000.0;
        let n = preset(vec![Band { kind: FilterKind::Notch, freq: 1000.0, q: 2.0, ..Default::default() }]);
        assert!(n.response_db(1000.0, fs, 0) < -60.0);
        let bp = preset(vec![Band { kind: FilterKind::BandPass, freq: 1000.0, q: 2.0, ..Default::default() }]);
        assert!(bp.response_db(1000.0, fs, 0).abs() < 0.01);
        assert!(bp.response_db(100.0, fs, 0) < -20.0);
        let ap = preset(vec![Band { kind: FilterKind::AllPass, freq: 1000.0, q: 0.7, ..Default::default() }]);
        for f in [50.0, 1000.0, 15_000.0] {
            assert!(ap.response_db(f, fs, 0).abs() < 1e-9);
        }
    }

    #[test]
    fn auto_preamp_cancels_the_biggest_boost() {
        let mut p = preset(vec![peak(100.0, 6.0, 1.0), peak(3000.0, 4.0, 2.0), peak(8000.0, -9.0, 1.0)]);
        p.auto_preamp = true;
        assert!((p.effective_preamp(48_000.0) + 6.0).abs() < 0.05);
        // Overlapping boosts add up.
        let p = preset(vec![peak(1000.0, 6.0, 0.5), peak(1200.0, 6.0, 0.5)]);
        assert!(p.max_boost_db(48_000.0) > 11.0);
    }

    #[test]
    fn balance_turns_one_side_down_only() {
        let p = EqPreset { balance: 2.0, preamp: -1.0, ..Default::default() };
        assert_eq!(p.channel_gain_db(0, 48_000.0), -3.0);
        assert_eq!(p.channel_gain_db(1, 48_000.0), -1.0);
    }

    #[test]
    fn per_channel_bands() {
        let mut b = peak(1000.0, 6.0, 1.0);
        b.channels = Channels::Right;
        let p = preset(vec![b]);
        assert!(p.response_db(1000.0, 48_000.0, 0).abs() < 1e-9);
        assert!((p.response_db(1000.0, 48_000.0, 1) - 6.0).abs() < 0.01);
    }

    #[test]
    fn curve_interpolates_on_log_frequency() {
        let p = EqPreset { curve: vec![[100.0, 0.0], [400.0, 6.0]], ..Default::default() };
        assert_eq!(p.curve_db(50.0), 0.0);
        assert!((p.curve_db(200.0) - 3.0).abs() < 1e-9);
        assert_eq!(p.curve_db(1000.0), 6.0);
    }

    #[test]
    fn live_stage_matches_designed_gain() {
        let fs = 48_000.0;
        let p = preset(vec![peak(1000.0, 6.0, 1.0)]);
        let stage = EqStage::new(&p, fs, 2, false);
        let mut x = sine(1000.0, fs, 1.0, 2);
        // SAFETY: test thread is the only "callback".
        unsafe { stage.live() }.process(&stage.mailbox, &mut x);
        assert!((level_db(&x, 48_000) - 6.0).abs() < 0.05);
    }

    #[test]
    fn new_plan_crossfades_in_without_a_jump() {
        let fs = 48_000.0;
        let stage = EqStage::new(&preset(vec![peak(1000.0, 0.0, 1.0)]), fs, 1, false);
        let live = unsafe { stage.live() };
        let mut x = sine(1000.0, fs, 0.5, 1);
        live.process(&stage.mailbox, &mut x[..12_000]);
        stage.mailbox.post(IirPlan::new(&preset(vec![peak(1000.0, 6.0, 1.0)]), fs, 1, true));
        live.process(&stage.mailbox, &mut x[12_000..]);
        // No sample-to-sample step bigger than the sine's own slope allows (≈2× at +6 dB).
        let max_step = x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
        assert!(max_step < 0.03, "step {max_step}");
        assert!((level_db(&x, 20_000) - 6.0).abs() < 0.05);
        stage.mailbox.collect();
    }

    #[test]
    fn bypass_is_level_matched() {
        let fs = 48_000.0;
        let mut p = preset(vec![peak(100.0, 6.0, 0.7)]);
        p.preamp = -6.0;
        let stage = EqStage::new(&p, fs, 1, true);
        let mut x = sine(1000.0, fs, 0.5, 1);
        unsafe { stage.live() }.process(&stage.mailbox, &mut x);
        // 1 kHz is out of the boost: the EQ plays it at −6 dB + a little; the bypass at the
        // EQ's average loudness, which is between that and the boosted bass.
        let bypass = level_db(&x, 1000);
        assert!((bypass - p.loudness_db(fs)).abs() < 0.05);
        assert!(bypass > -6.0 && bypass < 0.0, "{bypass}");
    }

    /// `cargo test --release -p hifi-engine -- --ignored --nocapture realtime`
    #[test]
    #[ignore]
    fn realtime_factor_callback_32_bands_384k() {
        let fs = 384_000.0;
        let p = preset((0..32).map(|i| peak(20.0 * 1.25f64.powi(i), 2.0, 2.0)).collect());
        let stage = EqStage::new(&p, fs, 2, false);
        let mut x = sine(1000.0, fs, 5.0, 2);
        let t0 = std::time::Instant::now();
        for c in x.chunks_mut(1024) {
            unsafe { stage.live() }.process(&stage.mailbox, c);
        }
        let el = t0.elapsed().as_secs_f64();
        println!("32 bands, 5 s of stereo 384k in {el:.3} s → {:.0}× real time", 5.0 / el);
    }

    #[test]
    fn sanitize_clamps_nonsense() {
        let p = EqPreset {
            preamp: f64::NAN,
            bands: vec![Band { freq: -5.0, gain: 99.0, q: 0.0, slope: 13, ..Default::default() }],
            curve: vec![[200.0, 1.0], [f64::NAN, 0.0], [100.0, 2.0]],
            ..Default::default()
        }
        .sanitized();
        assert_eq!(p.preamp, 0.0);
        let b = &p.bands[0];
        assert_eq!((b.freq, b.gain, b.q, b.slope), (10.0, 30.0, 0.05, 12));
        assert_eq!(p.curve, vec![[100.0, 2.0], [200.0, 1.0]]);
    }
}
