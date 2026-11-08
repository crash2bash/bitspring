//! Write DSD test files: `make_dsd <out.dsf|out.dff> [dsd-multiple=1] [secs=10]`.
//! Left: 1 kHz, right: 3 kHz, both at −6 dB SACD (50 % modulation).
use std::path::PathBuf;

use hifi_engine::dsd::{DSD64, modulate, write_dff, write_dsf};

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let out = PathBuf::from(args.get(1).expect("usage: make_dsd <out.dsf|out.dff> [multiple] [secs]"));
    let mult: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
    let secs: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10.0);
    let rate = DSD64 * mult;
    let bits = modulate(rate, 2, secs, |c, t| {
        let f = if c == 0 { 1000.0 } else { 3000.0 };
        0.5 * (2.0 * std::f64::consts::PI * f * t).sin()
    });
    if out.extension().is_some_and(|e| e == "dff") { write_dff(&out, rate, &bits) } else { write_dsf(&out, rate, &bits) }
}
