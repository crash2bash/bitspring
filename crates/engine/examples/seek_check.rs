//! Hardware check of the processed path: start mid-track, pause, seek, run to the end.
//! Usage: seek_check <device-name-part> <file>
use std::sync::Arc;
use std::time::Duration;

use hifi_engine::output::{self, PlayOptions, Player};
use hifi_engine::dsp::crossfeed::CrossfeedLevel;
use hifi_engine::dsp::{DspSettings, Upsample};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (dev, file) = (args.next().unwrap().to_lowercase(), args.next().unwrap());
    let device = output::output_devices()?.into_iter().find(|d| d.name.to_lowercase().contains(&dev)).expect("device");
    let track = Arc::new(hifi_engine::load_track(std::path::Path::new(&file))?);
    let dsp = DspSettings { upsample: Upsample::UpTo(384_000.0), eq: None, crossfeed: Some(CrossfeedLevel::CHU_MOY), ..Default::default() };
    let sleep = |ms| std::thread::sleep(Duration::from_millis(ms));

    let p = Player::start(device.device, track.clone(), &PlayOptions { dsp, start_at: 1.0, ..Default::default() })?;
    println!("rate {} → {}", p.path.source_rate, p.path.device_rate);
    sleep(30);
    println!("start_at 1.0      → pos {:.2}", p.position_secs());
    sleep(300);
    p.set_paused(true);
    let a = p.position_secs();
    sleep(300);
    println!("paused 300 ms     → moved {:.3} s (expect 0)", p.position_secs() - a);
    p.seek(0.2);
    p.set_paused(false);
    sleep(60);
    println!("seek 0.2 + 60 ms  → pos {:.2}", p.position_secs());
    while !p.is_finished() {
        sleep(20);
    }
    println!("finished at {:.2} / {:.2} s, underruns {}", p.position_secs(), p.duration_secs(), p.underruns());
    Ok(())
}
