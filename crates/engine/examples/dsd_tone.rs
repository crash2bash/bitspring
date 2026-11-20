//! A stereo DSF of a test signal made by the PCM → DSD encoder, for checking it with an
//! independent decoder (ffmpeg, a DAC). Left: 1 kHz at −6 dBFS PCM; right: 1 kHz at −60 dBFS
//! plus 19 kHz at −20 dBFS. PCM full scale is 50 % modulation (the SACD reference level).
//! `cargo run --release -p hifi-engine --example dsd_tone -- <out.dsf> <64|128|256> <5|7> [seconds]`
use hifi_engine::dsd::write_dsf;
use hifi_engine::dsp::dsd_encode::{DsdOut, DsdRate, Encoder, ModOrder};
use std::f64::consts::PI;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: dsd_tone <out.dsf> <64|128|256> <5|7> [seconds]";
    let (Some(path), Some(rate), Some(order)) = (args.first(), args.get(1), args.get(2)) else {
        eprintln!("{usage}");
        std::process::exit(2);
    };
    let rate = rate.parse().ok().and_then(DsdRate::from_multiple).unwrap_or_else(|| panic!("{usage}"));
    let order = order.parse().ok().and_then(ModOrder::from_number).unwrap_or_else(|| panic!("{usage}"));
    let secs: f64 = args.get(3).map_or(10.0, |s| s.parse().expect(usage));

    let wire = rate.wire_rate() as f64;
    let db = |d: f64| 10f64.powf(d / 20.0);
    let pcm: Vec<f32> = (0..(secs * wire) as usize)
        .flat_map(|i| {
            let t = i as f64 / wire;
            let left = db(-6.0) * (2.0 * PI * 1000.0 * t).sin();
            let right = db(-60.0) * (2.0 * PI * 1000.0 * t).sin() + db(-20.0) * (2.0 * PI * 19_000.0 * t).sin();
            [left as f32, right as f32]
        })
        .collect();
    let mut enc = Encoder::new(DsdOut { rate, order }, 2);
    let mut words = Vec::with_capacity(pcm.len());
    for block in pcm.chunks(2 * 4096) {
        enc.process(block, &mut words);
    }
    // Words are 16 bits per channel per frame, oldest bit in the MSB: two bytes, high first.
    let channels: Vec<Vec<u8>> = (0..2).map(|c| words.iter().skip(c).step_by(2).flat_map(|w| w.to_be_bytes()).collect()).collect();
    write_dsf(Path::new(path), rate.hz(), &channels).expect("write the DSF");
    println!(
        "{path}: DSD{} ({} Hz), order {}, {secs} s, latency {} wire frames, {} overloads, {} clipped samples",
        rate.multiple(),
        rate.hz(),
        order.get(),
        enc.latency_frames(),
        enc.overloads(),
        enc.clips()
    );
}
