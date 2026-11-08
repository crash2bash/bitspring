//! Decode a file and print per-channel peak level for each half — checks channel order.
fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("usage: chan_peaks <file>");
    let t = hifi_engine::load_track(std::path::Path::new(&path))?;
    let (n, ch) = (t.frames(), t.channels);
    let samples = t.to_vec();
    for (name, range) in [("first half ", 0..n / 2), ("second half", n / 2..n)] {
        let peaks: Vec<String> = (0..ch)
            .map(|c| {
                let p = range.clone().map(|f| samples[f * ch + c].abs()).fold(0.0f32, f32::max);
                format!("ch{c}={:.3}", p)
            })
            .collect();
        println!("decoded: {name} {}", peaks.join(" "));
    }
    Ok(())
}
