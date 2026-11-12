//! Play one CUE track of an image and print its first samples as 16-bit integers, to compare
//! with another decoder: `cue_check <image> <start frames> <end frames>`.
use std::path::Path;

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let path = hifi_engine::cue::track_path(&a[1], hifi_engine::cue::Range { start: a[2].parse()?, end: Some(a[3].parse()?) });
    let t = hifi_engine::open_file(Path::new(&path), hifi_engine::dsd::DsdMode::Pcm)?;
    t.wait_for(usize::MAX, std::time::Duration::from_secs(60));
    println!("frames {} title {:?}", t.frames(), t.meta.title);
    for f in [0, 50_000, 1_234_567, 5_000_000, t.frames() - 50_000, t.frames() - 1] {
        print!("{f}:{} ", (t.sample(f, 0) * 32768.0) as i32);
    }
    println!();
    Ok(())
}
