//! Optional sound processing ("enhancers"). Off by default: with no processing the player
//! takes the untouched bit-perfect path and none of this code runs.

pub mod crossfeed;
pub mod dsd_encode;
pub mod eq;
pub mod eq_io;
pub mod fir;
pub mod loudness;
pub mod lossy;
pub mod resample;
pub mod upsample;

use std::sync::Arc;

use crossfeed::{Crossfeed, CrossfeedLevel};
use dsd_encode::DsdOut;
use eq::EqPreset;
use fir::{Convolver, FirControl, ImpulseResponse};
use lossy::{Lossy, LossyPlan, LossySettings};
use loudness::{LevelMode, LevelSettings};
use resample::{ResampleQuality, Resampler};
use upsample::{Doubler, FilterSpec};

/// An impulse response to convolve with. Compared by identity: the same loaded file.
#[derive(Debug, Clone)]
pub struct Convolution(pub Arc<ImpulseResponse>);

impl PartialEq for Convolution {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// How far to upsample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Upsample {
    Off,
    /// Highest same-family rate the DAC offers, but no higher than this (Hz).
    UpTo(f64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DspSettings {
    pub upsample: Upsample,
    /// Upsample with a long filter (see [`upsample`]) instead of the standard one, where the
    /// device rate is at least twice the source's (see [`long_filter`]). None = standard.
    pub upsample_filter: Option<FilterSpec>,
    pub eq: Option<EqPreset>,
    /// The equaliser starts in its level-matched compare bypass.
    pub eq_bypass: bool,
    pub convolution: Option<Convolution>,
    pub crossfeed: Option<CrossfeedLevel>,
    /// Applied to lossy tracks only, and only when the analysis finds something to fix.
    pub lossy: LossySettings,
    /// Volume levelling and peak headroom: each track plays at the gain set on it
    /// ([`crate::Track::set_level`]).
    pub level: LevelSettings,
    /// Encode PCM to DSD at this rate and send it as DoP (see [`dsd_encode`]), where the
    /// device can take it: exclusive access, DoP at the wire rate. Where it can't, playback
    /// goes on as if this were unset (see `output::plan`). A DSD track sent as DoP ignores it.
    pub dsd_out: Option<DsdOut>,
}

impl Default for DspSettings {
    fn default() -> Self {
        Self {
            upsample: Upsample::Off,
            upsample_filter: None,
            eq: None,
            eq_bypass: false,
            convolution: None,
            crossfeed: None,
            lossy: LossySettings::default(),
            level: LevelSettings::default(),
            dsd_out: None,
        }
    }
}

impl DspSettings {
    /// Processing that applies to every track. (Off, and no lossy plan → bit-perfect path.)
    pub fn is_active(&self) -> bool {
        self.reshapes() || self.level.mode != LevelMode::Off
    }

    /// Processing that rebuilds the waveform, so it may peak where the source's samples
    /// don't — what the peak headroom is for. The DSD encoder counts: it upsamples to the wire
    /// rate, and the headroom is what keeps its input's true peaks within what the modulator
    /// is rated for ([`dsd_encode::MAX_DEPTH`]).
    pub fn reshapes(&self) -> bool {
        self.upsample != Upsample::Off
            || self.eq.is_some()
            || self.convolution.is_some()
            || self.crossfeed.is_some()
            || self.dsd_out.is_some()
    }

    /// Tracks play at the gain set on them.
    pub fn levels(&self) -> bool {
        self.level.mode != LevelMode::Off || self.level.headroom
    }
}

/// The long upsampling filter that runs for `in_rate → out_rate`, if one does: when upsampling
/// is on (or the output is encoded to DSD, which always upsamples to the wire rate), a long
/// filter is chosen, and the device runs at twice the source rate or more (the ×2 stage has
/// nothing to do below that). Otherwise the chain is the standard one. The spec is the one
/// built ([`FilterSpec::effective`]).
pub fn long_filter(settings: &DspSettings, in_rate: u32, out_rate: u32) -> Option<FilterSpec> {
    let doubles = in_rate > 0 && out_rate as u64 >= 2 * in_rate as u64;
    let upsamples = settings.upsample != Upsample::Off || settings.dsd_out.is_some();
    (upsamples && doubles).then_some(settings.upsample_filter?.effective())
}

/// The processing chain for one track at one device rate. Runs on the render thread. (The
/// parametric EQ itself runs later: in the audio callback, or after the chain on the render
/// thread when the output is encoded to DSD — see [`eq::EqStage`]. The encoder runs after it,
/// see `output::render_loop`.)
pub struct Chain {
    channels: usize,
    /// The long filter's ×2 stage, ahead of `resampler` (which then only takes 2·in_rate on
    /// to the device rate, if that is higher still).
    doubler: Option<Doubler>,
    doubled: Vec<f64>,
    /// Frames the ×2 stage has taken since it last started over, and frames it has put out.
    doubler_in: usize,
    doubler_out: usize,
    /// A block of silence, to let the ×2 stage's tail out a block at a time.
    zeros: Vec<f64>,
    /// The tail is on its way out: call again with `last` (see [`Chain::finishing`]).
    finishing: bool,
    resampler: Option<Resampler>,
    crossfeed: Option<Crossfeed>,
    fir: Option<Convolver>,
    fir_out: Vec<f64>,
    lossy: Option<Lossy>,
    scratch: Vec<f64>,
    out: Vec<f64>,
}

impl Chain {
    /// `out_rate` is the rate the device will run at (equal to `in_rate` when not resampling).
    /// `lossy` is the per-track plan from [`lossy::plan`]; it runs first, at the source rate.
    /// `fir`: the FIR stage's control (with its first filters), when one is needed.
    pub fn new(
        settings: &DspSettings,
        lossy: Option<&LossyPlan>,
        fir: Option<(Arc<FirControl>, Arc<fir::FirFilters>)>,
        in_rate: u32,
        out_rate: u32,
        channels: usize,
    ) -> Self {
        let fs = out_rate as f64;
        // (The design can take a moment the first time: see `upsample::filter`.)
        let doubler = long_filter(settings, in_rate, out_rate).map(|spec| Doubler::new(upsample::filter(spec), channels));
        let resampler = match &doubler {
            Some(_) => (out_rate != 2 * in_rate).then(|| Resampler::new(2 * in_rate, out_rate, channels, ResampleQuality::AFTER_DOUBLING)),
            None => (in_rate != out_rate).then(|| Resampler::new(in_rate, out_rate, channels, ResampleQuality::HIGH)),
        };
        Self {
            channels,
            doubler,
            doubled: Vec::new(),
            doubler_in: 0,
            doubler_out: 0,
            zeros: Vec::new(),
            finishing: false,
            resampler,
            crossfeed: settings.crossfeed.map(|l| Crossfeed::new(l, fs)),
            fir: fir.map(|(c, first)| Convolver::new(c, first)),
            fir_out: Vec::new(),
            lossy: lossy.map(|p| Lossy::new(p, in_rate as f64, channels)),
            scratch: Vec::new(),
            out: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        if let Some(d) = &mut self.doubler {
            d.reset();
        }
        self.doubler_in = 0;
        self.doubler_out = 0;
        self.finishing = false;
        if let Some(r) = &mut self.resampler {
            r.reset();
        }
        if let Some(f) = &mut self.fir {
            f.reset();
        }
        if let Some(c) = &mut self.crossfeed {
            c.reset();
        }
        if let Some(l) = &mut self.lossy {
            l.reset();
        }
    }

    /// Process a block of interleaved f32 input; returns interleaved output at the device rate.
    /// `gain`: the level of the track the block is from, applied first — so at a gapless join
    /// each track's samples get its own gain, whatever the chain's delay. `last` flushes the
    /// tails of the stages. The ×2 stage's (it may hold a hundred seconds of input) comes out
    /// a block per call, so that no call takes much longer than one in the middle of a track:
    /// while [`Chain::finishing`] says so, call again with `last` and no input.
    pub fn process(&mut self, input: &[f32], gain: f64, last: bool) -> &[f64] {
        self.scratch.clear();
        self.scratch.extend(input.iter().map(|&s| s as f64 * gain));
        if let Some(l) = &mut self.lossy {
            l.process(&mut self.scratch, self.channels);
        }
        let ch = self.channels;
        let mut last = last;
        if let Some(d) = &mut self.doubler {
            self.doubled.clear();
            d.process(&self.scratch, &mut self.doubled);
            self.doubler_in += self.scratch.len() / ch;
            if last {
                // What `Doubler::flush` does in one go: silence in until every frame taken has
                // come out, the surplus cut off, a fresh start.
                if self.doubled.len() / ch + self.doubler_out < 2 * self.doubler_in {
                    self.zeros.resize(d.filter().block() * ch, 0.0);
                    d.process(&self.zeros, &mut self.doubled);
                }
                let want = 2 * self.doubler_in - self.doubler_out;
                self.finishing = self.doubled.len() / ch < want;
                // The stages after it let their tails out once it is through.
                last = !self.finishing;
            }
            if last {
                self.doubled.truncate((2 * self.doubler_in - self.doubler_out) * ch);
                d.reset();
                self.doubler_in = 0;
                self.doubler_out = 0;
            } else {
                self.doubler_out += self.doubled.len() / ch;
            }
            std::mem::swap(&mut self.scratch, &mut self.doubled);
        }
        self.out.clear();
        match &mut self.resampler {
            Some(r) => {
                r.process(&self.scratch, &mut self.out);
                if last {
                    r.flush(&mut self.out);
                }
            }
            None => std::mem::swap(&mut self.scratch, &mut self.out),
        }
        if let Some(c) = &mut self.crossfeed {
            c.process(&mut self.out, self.channels);
        }
        if let Some(f) = &mut self.fir {
            self.fir_out.clear();
            f.process(&self.out, &mut self.fir_out, last);
            std::mem::swap(&mut self.out, &mut self.fir_out);
        }
        &self.out
    }

    /// The last call (with `last`) didn't let the whole tail out yet: call again.
    pub fn finishing(&self) -> bool {
        self.finishing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;
    use upsample::FilterKind;

    fn long(kind: FilterKind, taps: usize, up_to: f64) -> DspSettings {
        DspSettings { upsample: Upsample::UpTo(up_to), upsample_filter: Some(FilterSpec { kind, taps }), ..Default::default() }
    }

    fn sine(freq: f64, fs: f64, frames: usize, amp: f64) -> Vec<f32> {
        (0..frames).map(|i| (amp * (2.0 * PI * freq * i as f64 / fs).sin()) as f32).collect()
    }

    /// Repeatable noise in −0.5 … 0.5.
    fn noise(n: usize, seed: u64) -> Vec<f32> {
        let mut x = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                ((x >> 11) as f64 / (1u64 << 53) as f64 - 0.5) as f32
            })
            .collect()
    }

    /// Through the chain the way the render thread feeds it: blocks of 2048 frames, then an
    /// empty last call that lets the tail out.
    fn run(chain: &mut Chain, input: &[f32], channels: usize) -> Vec<f64> {
        let mut out = Vec::new();
        for block in input.chunks(2048 * channels) {
            out.extend_from_slice(chain.process(block, 1.0, false));
        }
        loop {
            out.extend_from_slice(chain.process(&[], 1.0, true));
            if !chain.finishing() {
                return out;
            }
        }
    }

    /// Amplitude of `freq` in one second of mono output starting at `from`: a whole number of
    /// cycles of every whole-Hz frequency, so the projection picks out exactly that one.
    fn amp(y: &[f64], from: usize, freq: f64, fs: u32) -> f64 {
        let w = 2.0 * PI * freq / fs as f64;
        let (mut a, mut b) = (0.0, 0.0);
        for (i, v) in y[from..from + fs as usize].iter().enumerate() {
            let t = (from + i) as f64;
            a += v * (w * t).sin();
            b += v * (w * t).cos();
        }
        2.0 * a.hypot(b) / fs as f64
    }

    /// 19 kHz at 0.5 from 44.1 kHz to `out_rate`: its level change (dB), and how far down
    /// (dB) each of `images` is, in the middle second of three.
    fn tone(settings: &DspSettings, out_rate: u32, images: &[f64]) -> (f64, Vec<f64>) {
        let mut chain = Chain::new(settings, None, None, 44_100, out_rate, 1);
        let y = run(&mut chain, &sine(19_000.0, 44_100.0, 3 * 44_100, 0.5), 1);
        let n = out_rate as usize;
        let kept = amp(&y, n, 19_000.0, out_rate);
        let down = images.iter().map(|&f| 20.0 * (kept / amp(&y, n, f, out_rate)).log10()).collect();
        (20.0 * (kept / 0.5).log10(), down)
    }

    #[test]
    fn long_filter_runs_only_from_twice_the_source_rate() {
        let s = long(FilterKind::Linear, 65_536, 768_000.0);
        assert_eq!(long_filter(&s, 44_100, 88_200), Some(FilterSpec { kind: FilterKind::Linear, taps: 65_536 }));
        assert_eq!(long_filter(&s, 44_100, 96_000).map(|f| f.taps), Some(65_536));
        assert_eq!(long_filter(&s, 44_100, 88_199), None);
        assert_eq!(long_filter(&s, 96_000, 176_400), None);
        assert_eq!(long_filter(&s, 44_100, 44_100), None);
        assert_eq!(long_filter(&DspSettings { upsample: Upsample::Off, ..s.clone() }, 44_100, 176_400), None);
        assert_eq!(long_filter(&DspSettings { upsample_filter: None, ..s.clone() }, 44_100, 176_400), None);
        // The spec as built.
        assert_eq!(long_filter(&long(FilterKind::Linear, 5_000, 192_000.0), 44_100, 176_400).map(|f| f.taps), Some(8_192));
    }

    #[test]
    fn doubling_keeps_the_band_and_drops_the_image() {
        let s = long(FilterKind::Linear, 65_536, 88_200.0);
        // Exactly twice as long, the tail included.
        let mut chain = Chain::new(&s, None, None, 44_100, 88_200, 2);
        assert_eq!(run(&mut chain, &noise(2 * 100_001, 1), 2).len(), 2 * 2 * 100_001);
        let (gain, down) = tone(&s, 88_200, &[25_100.0]);
        println!("44.1 → 88.2, Linear 65536: 19 kHz {gain:+.2e} dB, image at 25.1 kHz {:.1} dB down", down[0]);
        assert!(gain.abs() <= 0.001, "19 kHz at {gain} dB");
        assert!(down[0] >= 140.0, "image only {:.1} dB down", down[0]);
    }

    #[test]
    fn doubling_then_resampling_keeps_the_band_and_the_time() {
        let s = long(FilterKind::Linear, 65_536, 768_000.0);
        for (out_rate, ratio) in [(176_400, 4), (352_800, 8)] {
            // 25.1 kHz: the ×2 stage's image; 69.2 kHz: the second stage's (88.2 − 19).
            let (gain, down) = tone(&s, out_rate, &[25_100.0, 69_200.0]);
            println!("44.1 → {}, Linear 65536: 19 kHz {gain:+.2e} dB, images at 25.1 / 69.2 kHz {:.1} / {:.1} dB down", out_rate as f64 / 1000.0, down[0], down[1]);
            assert!(gain.abs() <= 0.001, "→ {out_rate}: 19 kHz at {gain} dB");
            assert!(down.iter().all(|&d| d >= 140.0), "→ {out_rate}: images {down:?} dB down");
            // An impulse at input frame n peaks at output frame ratio·n.
            let mut x = vec![0.0f32; 50_000];
            x[20_000] = 1.0;
            let y = run(&mut Chain::new(&s, None, None, 44_100, out_rate, 1), &x, 1);
            let peak = y.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).unwrap().0;
            println!("  impulse at input 20000 peaks at output {peak} (want {})", ratio * 20_000);
            assert!((peak as i64 - ratio * 20_000).abs() <= 1, "→ {out_rate}: peak at {peak}");
        }
    }

    #[test]
    fn doubling_into_the_other_family() {
        // 44.1 → 88.2 (minimum phase) → 96: 25.1 kHz is the ×2 stage's image, 26.8 kHz the
        // second stage's (88.2 − 19 = 69.2 kHz, folded at 48).
        let s = long(FilterKind::Minimum, 65_536, 96_000.0);
        let (gain, down) = tone(&s, 96_000, &[25_100.0, 26_800.0]);
        println!("44.1 → 96, Minimum 65536: 19 kHz {gain:+.2e} dB, images at 25.1 / 26.8 kHz {:.1} / {:.1} dB down", down[0], down[1]);
        assert!(gain.abs() <= 0.001, "19 kHz at {gain} dB");
        assert!(down.iter().all(|&d| d >= 140.0), "images {down:?} dB down");
    }

    /// Wherever the long filter doesn't run, the chain is the standard resampler, bit for bit.
    #[test]
    fn without_the_long_filter_the_chain_is_unchanged() {
        let input = noise(2 * 30_000, 5);
        let cases = [
            (DspSettings { upsample: Upsample::UpTo(192_000.0), ..Default::default() }, 44_100, 176_400),
            // A filter chosen, but upsampling off (a DAC that can't do the source rate).
            (DspSettings { upsample: Upsample::Off, ..long(FilterKind::Linear, 65_536, 192_000.0) }, 44_100, 88_200),
            // Less than twice the source rate.
            (long(FilterKind::Linear, 65_536, 192_000.0), 44_100, 48_000),
            (long(FilterKind::Minimum, 65_536, 192_000.0), 96_000, 176_400),
        ];
        for (settings, from, to) in cases {
            let got = run(&mut Chain::new(&settings, None, None, from, to, 2), &input, 2);
            let mut r = Resampler::new(from, to, 2, ResampleQuality::HIGH);
            let mut want = Vec::new();
            for block in input.chunks(2048 * 2) {
                r.process(&block.iter().map(|&v| v as f64).collect::<Vec<_>>(), &mut want);
            }
            r.flush(&mut want);
            assert!(got == want, "{from} → {to} differs from the standard resampler");
        }
    }

    /// The tail comes out a block per call, and all of it: exactly what the ×2 stage's own
    /// flush lets out in one go.
    #[test]
    fn the_tail_comes_out_in_steps_as_the_doubler_flushes_it() {
        let s = long(FilterKind::Linear, 65_536, 88_200.0);
        let input = noise(2 * 50_001, 3);
        let mut chain = Chain::new(&s, None, None, 44_100, 88_200, 2);
        let mut got = Vec::new();
        for block in input.chunks(2048 * 2) {
            got.extend_from_slice(chain.process(block, 1.0, false));
        }
        let mut steps = 0;
        loop {
            let out = chain.process(&[], 1.0, true);
            assert!(out.len() <= 2 * 2 * 2 * 1024, "a step of {} samples", out.len());
            got.extend_from_slice(out);
            steps += 1;
            if !chain.finishing() {
                break;
            }
        }
        let mut d = Doubler::new(upsample::filter(s.upsample_filter.unwrap()), 2);
        let mut want = Vec::new();
        let x: Vec<f64> = input.iter().map(|&v| v as f64).collect();
        for block in x.chunks(2048 * 2) {
            d.process(block, &mut want);
        }
        d.flush(&mut want);
        println!("tail of Linear 65536 let out in {steps} steps");
        assert!(steps > 1);
        assert!(got == want);
        // And it starts over: the same again gives the same.
        assert!(run(&mut chain, &input, 2) == want);
    }

    #[test]
    fn reset_is_a_fresh_start() {
        let s = long(FilterKind::Linear, 65_536, 192_000.0);
        let input = noise(2 * 40_000, 9);
        let fresh = run(&mut Chain::new(&s, None, None, 44_100, 176_400, 2), &input, 2);
        let mut chain = Chain::new(&s, None, None, 44_100, 176_400, 2);
        for block in noise(2 * 25_000, 10).chunks(2048 * 2) {
            chain.process(block, 1.0, false);
        }
        chain.reset();
        assert!(run(&mut chain, &input, 2) == fresh);
    }
}
