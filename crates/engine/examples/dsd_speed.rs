//! Speed of PCM → DSD, stereo, one thread. First the encoder alone: for each rate and
//! modulator order, 30 s of a 997 Hz tone at the wire rate fed in 2048-frame blocks. Then the
//! whole chain the render thread runs: 44.1 kHz in 2048-frame blocks, upsampled to the wire
//! rate (the standard filter, or the longest linear one), clamped, encoded. Real-time factor
//! over the whole run, and the longest single call against the time that block lasts.
//! `cargo run --release -p hifi-engine --example dsd_speed`
use hifi_engine::dsp::dsd_encode::{DsdOut, DsdRate, Encoder, ModOrder};
use hifi_engine::dsp::upsample::{FilterKind, FilterSpec};
use hifi_engine::dsp::{Chain, DspSettings};
use std::time::{Duration, Instant};

const BLOCK: usize = 2048;

fn main() {
    encoder();
    chain();
}

fn encoder() {
    let secs = 30.0;
    println!("Encoder alone, stereo, {secs} s:");
    println!("{:<8} {:>6} {:>11} {:>14} {:>12}", "rate", "order", "real time", "longest call", "block lasts");
    for rate in DsdRate::ALL {
        for order in ModOrder::ALL {
            let wire = rate.wire_rate() as f64;
            let frames = (secs * wire) as usize;
            // Designed (and cached) before the clock starts, as the player would.
            let mut enc = Encoder::new(DsdOut { rate, order }, 2);
            let mut pcm = Vec::with_capacity(2 * BLOCK);
            let mut words = Vec::with_capacity(2 * BLOCK);
            let (mut total, mut longest) = (Duration::ZERO, Duration::ZERO);
            let mut i = 0;
            while i < frames {
                let n = BLOCK.min(frames - i);
                pcm.clear();
                pcm.extend((i..i + n).flat_map(|f| {
                    let v = (0.5 * (2.0 * std::f64::consts::PI * 997.0 * f as f64 / wire).sin()) as f32;
                    [v, -v]
                }));
                words.clear();
                let t = Instant::now();
                enc.process(&pcm, &mut words);
                let took = t.elapsed();
                assert_eq!(words.len(), pcm.len());
                total += took;
                longest = longest.max(took);
                i += n;
            }
            assert_eq!(enc.overloads(), 0);
            println!(
                "{:<8} {:>6} {:>10.1}× {:>11.3} ms {:>9.2} ms",
                format!("DSD{}", rate.multiple()),
                order.get(),
                secs / total.as_secs_f64(),
                longest.as_secs_f64() * 1000.0,
                BLOCK as f64 / wire * 1000.0
            );
        }
    }
}

/// What the render thread does when it encodes: the chain from 44.1 kHz to the wire rate, the
/// chain's f64 output to f32, the clamp to full scale (where the EQ would run), the encoder.
/// The longest call while playing (a long filter's burst) and while letting the tail out, each
/// of which the ring (~0.5 s) has to cover.
fn chain() {
    const FROM: u32 = 44_100;
    let secs = 30.0;
    let filters = [(None, "standard"), (Some(1 << 20), "Linear 2^20")];
    println!("\nWhole chain, stereo, {secs} s of 44.1 kHz in {BLOCK}-frame blocks ({:.1} ms each):", BLOCK as f64 / FROM as f64 * 1000.0);
    println!("{:<8} {:>6} {:<12} {:>11} {:>14} {:>14}", "rate", "order", "filter", "real time", "longest call", "longest tail");
    let frames = (FROM as f64 * secs) as usize;
    let input: Vec<f32> = (0..frames)
        .flat_map(|i| {
            let v = (0.5 * (2.0 * std::f64::consts::PI * 997.0 * i as f64 / FROM as f64).sin()) as f32;
            [v, -v]
        })
        .collect();
    for (taps, name) in filters {
        for rate in DsdRate::ALL {
            for order in ModOrder::ALL {
                let out = DsdOut { rate, order };
                let settings = DspSettings {
                    upsample_filter: taps.map(|taps| FilterSpec { kind: FilterKind::Linear, taps }),
                    dsd_out: Some(out),
                    ..Default::default()
                };
                // Designed (and cached) before the clock starts, as `Playback::new` does it.
                let mut chain = Chain::new(&settings, None, None, FROM, rate.wire_rate(), 2);
                let mut enc = Encoder::new(out, 2);
                let (mut pcm, mut words) = (Vec::new(), Vec::new());
                let (mut total, mut longest, mut tail) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
                let mut out_frames = 0;
                let mut blocks = input.chunks(BLOCK * 2);
                loop {
                    let block = blocks.next();
                    let t = Instant::now();
                    let y = chain.process(block.unwrap_or(&[]), 1.0, block.is_none());
                    pcm.clear();
                    pcm.extend(y.iter().map(|&v| (v as f32).clamp(-1.0, 1.0)));
                    words.clear();
                    enc.process(&pcm, &mut words);
                    let took = t.elapsed();
                    out_frames += words.len() / 2;
                    total += took;
                    match block {
                        Some(_) => longest = longest.max(took),
                        None => tail = tail.max(took),
                    }
                    if block.is_none() && !chain.finishing() {
                        break;
                    }
                }
                let want = frames as f64 * rate.wire_rate() as f64 / FROM as f64;
                assert!(out_frames as f64 >= want * 0.999, "{out_frames} of {want}");
                assert_eq!(enc.overloads(), 0);
                println!(
                    "{:<8} {:>6} {:<12} {:>10.1}× {:>11.1} ms {:>11.1} ms",
                    format!("DSD{}", rate.multiple()),
                    order.get(),
                    name,
                    secs / total.as_secs_f64(),
                    longest.as_secs_f64() * 1000.0,
                    tail.as_secs_f64() * 1000.0
                );
            }
        }
    }
}
