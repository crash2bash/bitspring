//! How much faster than realtime the resampler runs, for the heaviest Q7 cases.
use hifi_engine::dsp::resample::{ResampleQuality, Resampler};
use std::time::Instant;

fn main() {
    for (from, to) in [(44_100u32, 48_000u32), (44_100, 176_400), (44_100, 352_800), (96_000, 384_000), (44_100, 705_600)] {
        let secs = 10.0;
        let input = vec![0.1f64; (from as f64 * secs) as usize * 2];
        let mut r = Resampler::new(from, to, 2, ResampleQuality::HIGH);
        let mut out = Vec::with_capacity((to as f64 * secs) as usize * 2 + 4096);
        let t = Instant::now();
        for chunk in input.chunks(4096) {
            r.process(chunk, &mut out);
        }
        let took = t.elapsed().as_secs_f64();
        println!("{:>6.1} → {:>6.1} kHz stereo: {:>6.1}× realtime", from as f64 / 1e3, to as f64 / 1e3, secs / took);
    }
}
