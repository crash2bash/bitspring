//! Check files the way the library's quality check does: `quality_check <file or folder>…`.
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let args: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    let r = hifi_engine::library::scan(&args, &[], |_, _| {}, |_, _| {});
    for t in &r.tracks {
        let t0 = Instant::now();
        match hifi_engine::quality::analyze(std::path::Path::new(&t.path)) {
            // SPECTRUM=json: one line per file with the whole report, for working on the rules.
            Ok(q) if std::env::var("SPECTRUM").as_deref() == Ok("json") => {
                println!("{}", serde_json::json!({ "path": t.path, "report": q }));
            }
            Ok(q) if std::env::var("SPECTRUM").is_ok() => {
                let step = q.sample_rate as f32 / 2.0 / q.spectrum.len() as f32;
                let line: Vec<String> = q.spectrum.iter().enumerate().step_by(10).map(|(i, d)| format!("{:.0}k:{:.0}", i as f32 * step / 1000.0, d)).collect();
                println!("{}\n  {}", t.path, line.join(" "));
            }
            Ok(q) => println!(
                "{:<28} {:>6}/{:<3} cutoff {:>6} wall {:<5} bits {:?} peak {:>6.2} clip {:>3}  {:.1}s  {}",
                format!("{:?}", q.verdict), q.sample_rate, q.bits.unwrap_or(0), q.cutoff_hz.map_or("full".into(), |c| c.to_string()),
                q.wall, q.effective_bits, q.peak_db, q.clipped, t0.elapsed().as_secs_f32(), hifi_engine::cue::split(&t.path).0.rsplit('/').next().unwrap_or("")
            ),
            Err(e) => println!("ERROR {e:#}  {}", t.path),
        }
    }
}
