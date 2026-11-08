//! Hardware check of progressive decoding: playback starts while the decoder still runs,
//! on both the bit-perfect and the processed path. Usage: progressive_check <device-part> <file>
use std::time::{Duration, Instant};

use hifi_engine::output::{self, PlayOptions, Player};
use hifi_engine::dsp::{DspSettings, Upsample};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (dev, file) = (args.next().unwrap().to_lowercase(), args.next().unwrap());
    let device = output::output_devices()?.into_iter().find(|d| d.name.to_lowercase().contains(&dev)).expect("device");
    for (name, dsp) in [
        ("direct", DspSettings::default()),
        ("processed", DspSettings { upsample: Upsample::UpTo(96_000.0), ..Default::default() }),
    ] {
        let t0 = Instant::now();
        let track = hifi_engine::open_track(std::path::Path::new(&file))?;
        let opened = t0.elapsed();
        let p = Player::start(device.device, track.clone(), &PlayOptions { dsp, ..Default::default() })?;
        println!(
            "[{name}] opened in {:.1} ms, playing after {:.1} ms with {:.2} s decoded of {:.2} s (complete: {})",
            opened.as_secs_f64() * 1e3,
            t0.elapsed().as_secs_f64() * 1e3,
            track.ready_frames() as f64 / track.sample_rate as f64,
            track.duration_secs(),
            track.is_complete()
        );
        std::thread::sleep(Duration::from_millis(400));
        p.seek(p.duration_secs() - 1.0);
        while !p.is_finished() {
            std::thread::sleep(Duration::from_millis(20));
        }
        println!(
            "[{name}] finished at {:.2} / {:.2} s, underruns {}, buffering {}",
            p.position_secs(),
            p.duration_secs(),
            p.underruns(),
            p.is_buffering()
        );
    }
    Ok(())
}
