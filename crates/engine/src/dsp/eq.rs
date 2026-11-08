//! Parametric equaliser: a cascade of biquads (RBJ Audio EQ Cookbook), processed in f64.
//!
//! Profiles come from AutoEQ's `ParametricEQ.txt` format, e.g. for headphone correction:
//! ```text
//! Preamp: -6.4 dB
//! Filter 1: ON LSC Fc 105 Hz Gain 6.0 dB Q 0.70
//! Filter 2: ON PK Fc 2700 Hz Gain -3.1 dB Q 1.41
//! Filter 3: ON HSC Fc 10000 Hz Gain 2.5 dB Q 0.70
//! ```

use std::f64::consts::PI;

use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FilterKind {
    Peak,
    LowShelf,
    HighShelf,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    pub kind: FilterKind,
    pub freq: f64,
    pub gain_db: f64,
    pub q: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct EqProfile {
    pub name: String,
    pub preamp_db: f64,
    pub bands: Vec<Band>,
}

impl EqProfile {
    /// Parse AutoEQ / Equalizer APO "ParametricEQ.txt". Unknown or disabled lines are skipped.
    pub fn parse_autoeq(name: &str, text: &str) -> Result<Self> {
        let mut p = EqProfile { name: name.to_string(), ..Default::default() };
        for line in text.lines().map(str::trim) {
            let words: Vec<&str> = line.split_whitespace().collect();
            if let Some(rest) = line.strip_prefix("Preamp:") {
                p.preamp_db = rest.trim().trim_end_matches("dB").trim().parse()?;
            } else if line.starts_with("Filter") && words.contains(&"ON") {
                let kind = match words.iter().find(|w| matches!(**w, "PK" | "PEQ" | "LSC" | "LS" | "HSC" | "HS")) {
                    Some(&"PK") | Some(&"PEQ") => FilterKind::Peak,
                    Some(&"LSC") | Some(&"LS") => FilterKind::LowShelf,
                    Some(&"HSC") | Some(&"HS") => FilterKind::HighShelf,
                    _ => continue,
                };
                let value = |key: &str| -> Option<f64> {
                    let i = words.iter().position(|w| *w == key)?;
                    words.get(i + 1)?.parse().ok()
                };
                let (Some(freq), Some(gain_db)) = (value("Fc"), value("Gain")) else { continue };
                let q = value("Q").unwrap_or(0.707);
                p.bands.push(Band { kind, freq, gain_db, q });
            }
        }
        if p.bands.is_empty() {
            bail!("no filters found — expected AutoEQ ParametricEQ.txt lines like `Filter 1: ON PK Fc 100 Hz Gain 3 dB Q 1.0`");
        }
        Ok(p)
    }

    /// Gain (dB) of the whole profile at `freq`, for drawing the curve in the UI.
    pub fn response_db(&self, freq: f64, sample_rate: f64) -> f64 {
        self.preamp_db
            + self
                .bands
                .iter()
                .map(|b| Coeffs::design(b, sample_rate).magnitude_db(freq, sample_rate))
                .sum::<f64>()
    }
}

#[derive(Debug, Clone, Copy)]
struct Coeffs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Coeffs {
    fn design(b: &Band, fs: f64) -> Self {
        let a = 10f64.powf(b.gain_db / 40.0);
        let w0 = 2.0 * PI * b.freq.min(fs * 0.49) / fs;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * b.q);
        let (b0, b1, b2, a0, a1, a2) = match b.kind {
            FilterKind::Peak => (1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a),
            FilterKind::LowShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                (
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
                (
                    a * ((a + 1.0) + (a - 1.0) * cos + s),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                    a * ((a + 1.0) + (a - 1.0) * cos - s),
                    (a + 1.0) - (a - 1.0) * cos + s,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos),
                    (a + 1.0) - (a - 1.0) * cos - s,
                )
            }
        };
        Self { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
    }

    fn magnitude_db(&self, freq: f64, fs: f64) -> f64 {
        let w = 2.0 * PI * freq / fs;
        // |H(e^jw)|² from numerator and denominator polynomials.
        let (c1, s1, c2, s2) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
        let num_re = self.b0 + self.b1 * c1 + self.b2 * c2;
        let num_im = -(self.b1 * s1 + self.b2 * s2);
        let den_re = 1.0 + self.a1 * c1 + self.a2 * c2;
        let den_im = -(self.a1 * s1 + self.a2 * s2);
        10.0 * ((num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im)).log10()
    }
}

/// Transposed direct form II — numerically well-behaved in f64.
#[derive(Debug, Clone, Copy, Default)]
struct State {
    z1: f64,
    z2: f64,
}

/// A running equaliser for interleaved audio at a fixed rate.
pub struct Equalizer {
    coeffs: Vec<Coeffs>,
    state: Vec<Vec<State>>, // [channel][band]
    preamp: f64,
    channels: usize,
}

impl Equalizer {
    pub fn new(profile: &EqProfile, sample_rate: f64, channels: usize) -> Self {
        let coeffs: Vec<Coeffs> = profile.bands.iter().map(|b| Coeffs::design(b, sample_rate)).collect();
        Self {
            state: vec![vec![State::default(); coeffs.len()]; channels],
            coeffs,
            preamp: 10f64.powf(profile.preamp_db / 20.0),
            channels,
        }
    }

    pub fn reset(&mut self) {
        self.state.iter_mut().flatten().for_each(|s| *s = State::default());
    }

    pub fn process(&mut self, interleaved: &mut [f64]) {
        for frame in interleaved.chunks_exact_mut(self.channels) {
            for (ch, x) in frame.iter_mut().enumerate() {
                let mut v = *x * self.preamp;
                for (c, s) in self.coeffs.iter().zip(self.state[ch].iter_mut()) {
                    let y = c.b0 * v + s.z1;
                    s.z1 = c.b1 * v - c.a1 * y + s.z2;
                    s.z2 = c.b2 * v - c.a2 * y;
                    v = y;
                }
                *x = v;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "Preamp: -6.4 dB\n\
        Filter 1: ON LSC Fc 105 Hz Gain 6.0 dB Q 0.70\n\
        Filter 2: ON PK Fc 2700 Hz Gain -3.1 dB Q 1.41\n\
        Filter 3: OFF PK Fc 5000 Hz Gain 9.0 dB Q 2.00\n\
        Filter 4: ON HSC Fc 10000 Hz Gain 2.5 dB Q 0.70\n";

    #[test]
    fn parses_autoeq_and_skips_disabled() {
        let p = EqProfile::parse_autoeq("test", SAMPLE).unwrap();
        assert_eq!(p.preamp_db, -6.4);
        assert_eq!(p.bands.len(), 3);
        assert_eq!(p.bands[0], Band { kind: FilterKind::LowShelf, freq: 105.0, gain_db: 6.0, q: 0.7 });
        assert_eq!(p.bands[2].kind, FilterKind::HighShelf);
    }

    #[test]
    fn peak_has_its_gain_at_centre_frequency() {
        let p = EqProfile { name: "".into(), preamp_db: 0.0, bands: vec![Band { kind: FilterKind::Peak, freq: 1000.0, gain_db: 6.0, q: 1.0 }] };
        assert!((p.response_db(1000.0, 48_000.0) - 6.0).abs() < 0.01);
        assert!(p.response_db(100.0, 48_000.0).abs() < 0.2);
    }

    #[test]
    fn processing_matches_designed_gain() {
        // A 1 kHz sine through a +6 dB peak at 1 kHz should come out ~2× in amplitude.
        let fs = 48_000.0;
        let p = EqProfile { name: "".into(), preamp_db: 0.0, bands: vec![Band { kind: FilterKind::Peak, freq: 1000.0, gain_db: 6.0, q: 1.0 }] };
        let mut eq = Equalizer::new(&p, fs, 1);
        let mut x: Vec<f64> = (0..48_000).map(|i| (2.0 * PI * 1000.0 * i as f64 / fs).sin() * 0.1).collect();
        eq.process(&mut x);
        let peak = x[24_000..].iter().fold(0.0f64, |m, v| m.max(v.abs()));
        assert!((20.0 * (peak / 0.1).log10() - 6.0).abs() < 0.05, "got {peak}");
    }
}
