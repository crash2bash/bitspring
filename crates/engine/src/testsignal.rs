//! Built-in test signals, generated in memory (no files needed).

use crate::decode::Track;

/// Left/right identification: 440 Hz in the left channel only, then 660 Hz in the right only.
/// −20 dBFS with 20 ms fades — safe on headphones, no clicks. Low tone first = left.
pub fn channel_test(sample_rate: u32) -> Track {
    let rate = sample_rate as usize;
    let half = rate * 3 / 2; // 1.5 s per side
    let fade = rate / 50;
    let amp = 10f32.powf(-20.0 / 20.0);
    let mut samples = vec![0.0f32; half * 2 * 2];
    for side in 0..2 {
        let freq = if side == 0 { 440.0 } else { 660.0 };
        for j in 0..half {
            let env = 1f32.min(j as f32 / fade as f32).min((half - j) as f32 / fade as f32);
            let s = amp * env * (std::f32::consts::TAU * freq * j as f32 / sample_rate as f32).sin();
            samples[(side * half + j) * 2 + side] = s;
        }
    }
    Track::from_samples(sample_rate, 2, Some(24), &samples, Default::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_then_right() {
        let t = channel_test(48_000);
        let n = t.frames();
        let samples = t.to_vec();
        let peak = |range: std::ops::Range<usize>, ch: usize| {
            range.map(|f| samples[f * 2 + ch].abs()).fold(0.0f32, f32::max)
        };
        assert!(peak(0..n / 2, 0) > 0.09 && peak(0..n / 2, 1) == 0.0);
        assert!(peak(n / 2..n, 1) > 0.09 && peak(n / 2..n, 0) == 0.0);
    }
}
