//! Scan a folder and print the albums the library would show.
use hifi_engine::library::{group_albums, scan};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("usage: scan_library <folder>"));
    let covers = AtomicUsize::new(0);
    let t0 = Instant::now();
    let r = scan(&[dir.clone()], &[], |_, _| {}, |_, _| { covers.fetch_add(1, Ordering::Relaxed); });
    let cold = t0.elapsed();
    let t1 = Instant::now();
    let again = scan(&[dir], &r.tracks, |_, _| {}, |_, _| {});
    println!("{} tracks, {} covers, cold {:.0?}, rescan {:.0?}, failed {:?}\n", r.tracks.len(), covers.load(Ordering::Relaxed), cold, t1.elapsed(), r.failed);
    assert_eq!(again.tracks, r.tracks);
    for a in group_albums(&r.tracks) {
        println!("{} — {} ({}) [{}] {} {}/{} {}  cover:{}  {:.0}s",
            a.artist, a.title, a.year.as_deref().unwrap_or("?"), a.genre.as_deref().unwrap_or("-"),
            a.format, a.max_bits.map_or("?".into(), |b| b.to_string()), a.max_rate, if a.hi_res { "HI-RES" } else { "" },
            a.cover_track.is_some(), a.duration);
        for &i in &a.tracks {
            let t = &r.tracks[i];
            println!("    {:>2} {:<18} {:>5.1}s", t.track_number.map_or("-".into(), |n| n.to_string()), t.title, t.duration);
        }
    }
}
