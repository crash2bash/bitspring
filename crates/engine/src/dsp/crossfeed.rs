//! Headphone crossfeed after Benjamin Bauer's design (as in libbs2b).
//!
//! On speakers each ear hears both speakers — the far one a little later and with less treble,
//! because the head is in the way. Headphones remove that, so hard-panned recordings sound
//! unnaturally "inside the head". Crossfeed feeds a low-passed, slightly delayed copy of each
//! channel into the other and lifts the direct path's treble to keep the overall tone flat.

use std::f64::consts::PI;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrossfeedLevel {
    /// Low-pass cut-off of the crossfed signal, Hz.
    pub cutoff_hz: f64,
    /// How much quieter the crossfed signal is at low frequencies, dB.
    pub feed_db: f64,
}

impl CrossfeedLevel {
    /// Bauer's default: close to a virtual speaker pair, subtle.
    pub const DEFAULT: Self = Self { cutoff_hz: 700.0, feed_db: 4.5 };
    /// Chu Moy's: a bit stronger.
    pub const CHU_MOY: Self = Self { cutoff_hz: 700.0, feed_db: 6.0 };
    /// Jan Meier's: lightest blend, keeps most of the original separation.
    pub const JAN_MEIER: Self = Self { cutoff_hz: 650.0, feed_db: 9.5 };
}

pub struct Crossfeed {
    a0_lo: f64,
    b1_lo: f64,
    a0_hi: f64,
    a1_hi: f64,
    b1_hi: f64,
    gain: f64,
    lo: [f64; 2],
    hi: [f64; 2],
    asis: [f64; 2],
}

impl Crossfeed {
    pub fn new(level: CrossfeedLevel, sample_rate: f64) -> Self {
        let g_lo_db = level.feed_db * -5.0 / 6.0 - 3.0;
        let g_hi_db = level.feed_db / 6.0 - 3.0;
        let g_lo = 10f64.powf(g_lo_db / 20.0);
        let g_hi = 1.0 - 10f64.powf(g_hi_db / 20.0);
        let fc_hi = level.cutoff_hz * 2f64.powf((g_lo_db - 20.0 * g_hi.log10()) / 12.0);

        let x = (-2.0 * PI * level.cutoff_hz / sample_rate).exp();
        let (a0_lo, b1_lo) = (g_lo * (1.0 - x), x);
        let x = (-2.0 * PI * fc_hi / sample_rate).exp();
        let (a0_hi, a1_hi, b1_hi) = (1.0 - g_hi * (1.0 - x), -x, x);
        Self {
            a0_lo,
            b1_lo,
            a0_hi,
            a1_hi,
            b1_hi,
            gain: 1.0 / (1.0 - g_hi + g_lo),
            lo: [0.0; 2],
            hi: [0.0; 2],
            asis: [0.0; 2],
        }
    }

    pub fn reset(&mut self) {
        self.lo = [0.0; 2];
        self.hi = [0.0; 2];
        self.asis = [0.0; 2];
    }

    /// Stereo interleaved only; anything else passes through.
    pub fn process(&mut self, interleaved: &mut [f64], channels: usize) {
        if channels != 2 {
            return;
        }
        for frame in interleaved.chunks_exact_mut(2) {
            for ch in 0..2 {
                let x = frame[ch];
                self.lo[ch] = self.a0_lo * x + self.b1_lo * self.lo[ch];
                self.hi[ch] = self.a0_hi * x + self.a1_hi * self.asis[ch] + self.b1_hi * self.hi[ch];
                self.asis[ch] = x;
            }
            frame[0] = (self.hi[0] + self.lo[1]) * self.gain;
            frame[1] = (self.hi[1] + self.lo[0]) * self.gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_content_keeps_its_level() {
        // Identical L/R (centre image) must come out at the same level — crossfeed only
        // changes how *different* the channels are.
        let mut cf = Crossfeed::new(CrossfeedLevel::DEFAULT, 48_000.0);
        let mut x: Vec<f64> = (0..96_000).flat_map(|i| {
            let s = (2.0 * PI * 200.0 * i as f64 / 48_000.0).sin() * 0.5;
            [s, s]
        }).collect();
        cf.process(&mut x, 2);
        let peak = x[48_000..].iter().fold(0.0f64, |m, v| m.max(v.abs()));
        assert!((peak - 0.5).abs() < 0.02, "got {peak}");
    }

    #[test]
    fn hard_left_bass_leaks_into_right() {
        let mut cf = Crossfeed::new(CrossfeedLevel::DEFAULT, 48_000.0);
        let mut x: Vec<f64> = (0..96_000).flat_map(|i| [(2.0 * PI * 100.0 * i as f64 / 48_000.0).sin() * 0.5, 0.0]).collect();
        cf.process(&mut x, 2);
        let right = x[48_000..].iter().skip(1).step_by(2).fold(0.0f64, |m, v| m.max(v.abs()));
        assert!(right > 0.05 && right < 0.4, "right channel got {right}");
    }
}
