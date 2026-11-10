//! Optional sound processing ("enhancers"). Off by default: with no processing the player
//! takes the untouched bit-perfect path and none of this code runs.

pub mod crossfeed;
pub mod eq;
pub mod eq_io;
pub mod fir;
pub mod lossy;
pub mod resample;

use std::sync::Arc;

use crossfeed::{Crossfeed, CrossfeedLevel};
use eq::EqPreset;
use fir::{Convolver, FirControl, ImpulseResponse};
use lossy::{Lossy, LossyPlan, LossySettings};
use resample::{ResampleQuality, Resampler};

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
    pub eq: Option<EqPreset>,
    /// The equaliser starts in its level-matched compare bypass.
    pub eq_bypass: bool,
    pub convolution: Option<Convolution>,
    pub crossfeed: Option<CrossfeedLevel>,
    /// Applied to lossy tracks only, and only when the analysis finds something to fix.
    pub lossy: LossySettings,
}

impl Default for DspSettings {
    fn default() -> Self {
        Self {
            upsample: Upsample::Off,
            eq: None,
            eq_bypass: false,
            convolution: None,
            crossfeed: None,
            lossy: LossySettings::default(),
        }
    }
}

impl DspSettings {
    /// Processing that applies to every track. (Off, and no lossy plan → bit-perfect path.)
    pub fn is_active(&self) -> bool {
        self.upsample != Upsample::Off || self.eq.is_some() || self.convolution.is_some() || self.crossfeed.is_some()
    }
}

/// The processing chain for one track at one device rate. Runs on the render thread. (The
/// parametric EQ itself runs later, in the audio callback — see [`eq::EqStage`].)
pub struct Chain {
    channels: usize,
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
        Self {
            channels,
            resampler: (in_rate != out_rate)
                .then(|| Resampler::new(in_rate, out_rate, channels, ResampleQuality::HIGH)),
            crossfeed: settings.crossfeed.map(|l| Crossfeed::new(l, fs)),
            fir: fir.map(|(c, first)| Convolver::new(c, first)),
            fir_out: Vec::new(),
            lossy: lossy.map(|p| Lossy::new(p, in_rate as f64, channels)),
            scratch: Vec::new(),
            out: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
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
    /// `last` flushes the resampler's tail.
    pub fn process(&mut self, input: &[f32], last: bool) -> &[f64] {
        self.scratch.clear();
        self.scratch.extend(input.iter().map(|&s| s as f64));
        if let Some(l) = &mut self.lossy {
            l.process(&mut self.scratch, self.channels);
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
}
