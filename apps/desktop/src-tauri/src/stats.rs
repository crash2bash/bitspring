//! Listening statistics: how often each track was listened to and when last, kept in
//! plays.json next to the library index — on this device only, sent nowhere.
//!
//! The file is its own, keyed by the track's path (a streaming track: its id), so a rescan of
//! the library — which rebuilds the index from the files — leaves it alone.
//!
//! The queue thread feeds [`Tracker`] the player's state; when a track has been listened to
//! (see hifi_engine::plays for the rule) it is counted here and, if the user connected an
//! account, handed to scrobbling — the same moment, the same rule.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use hifi_engine::plays::{Event, Listen, Played};
use tauri::{AppHandle, Manager, State};

use crate::scrobble::{Scrobbling, Song};

#[derive(Default)]
pub struct Plays {
    map: Mutex<HashMap<String, Played>>,
    file: Option<PathBuf>,
    /// Bumped with every listen counted, so the UI knows to fetch the numbers again.
    rev: AtomicU64,
}

impl Plays {
    pub fn load(app: &AppHandle) -> Self {
        Self::at(app.path().app_data_dir().ok().map(|d| d.join("plays.json")))
    }

    /// The statistics kept in `file` (None: in memory only).
    pub fn at(file: Option<PathBuf>) -> Self {
        let map = file.as_deref().and_then(hifi_engine::store::load_json).unwrap_or_default();
        Self { map: Mutex::new(map), file, rev: AtomicU64::new(0) }
    }

    /// `path` was listened to at `when` (Unix seconds). Written out at once: it happens once
    /// per track played, and a listen shouldn't be lost to a crash a minute later.
    pub fn record(&self, path: &str, when: u64) -> Played {
        let mut map = self.map.lock().unwrap();
        let p = map.entry(path.to_string()).or_default();
        p.count = p.count.saturating_add(1);
        p.last = p.last.max(when);
        let now = *p;
        if let Some(f) = &self.file {
            // Best effort: a failed write costs the listens since the last good one.
            let _ = hifi_engine::store::save_json(f, &*map);
        }
        drop(map);
        self.rev.fetch_add(1, Ordering::AcqRel);
        now
    }

    /// What is known about `path` (never listened to: zero).
    pub fn of(&self, path: &str) -> Played {
        self.map.lock().unwrap().get(path).copied().unwrap_or_default()
    }

    pub fn rev(&self) -> u64 {
        self.rev.load(Ordering::Acquire)
    }
}

/// Every track listened to so far: path → (count, last time).
#[tauri::command]
pub fn plays(plays: State<'_, Arc<Plays>>) -> HashMap<String, Played> {
    plays.map.lock().unwrap().clone()
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// What the player is doing, as the queue thread sees it on one tick.
pub struct Seen {
    /// Changes when another track starts (`AppState::now_rev`).
    pub rev: u64,
    /// Sound is coming out: not paused, not waiting for audio, not at the end.
    pub playing: bool,
    pub position: f64,
}

/// The track a listen is of.
#[derive(Clone)]
pub struct Heard {
    pub path: String,
    pub duration: f64,
    /// Its tags, when it has the two a scrobble needs (artist and title).
    pub song: Option<Song>,
}

struct Current {
    rev: u64,
    heard: Heard,
    listen: Listen,
    /// When it first sounded (Unix seconds): the time a scrobble carries.
    started: Option<u64>,
}

/// Follows what plays and counts each track once it has been listened to.
pub struct Tracker {
    plays: Arc<Plays>,
    scrobbling: Scrobbling,
    current: Option<Current>,
    last_tick: Instant,
}

impl Tracker {
    pub fn new(plays: Arc<Plays>, scrobbling: Scrobbling) -> Self {
        Self { plays, scrobbling, current: None, last_tick: Instant::now() }
    }

    /// The start (`Seen::rev`) of the track being followed.
    pub fn following(&self) -> Option<u64> {
        self.current.as_ref().map(|c| c.rev)
    }

    /// One look at the player. `seen`: None when nothing is loaded (stopped, the channel
    /// test). `heard` is asked for the track only when a new one has started.
    pub fn tick(&mut self, seen: Option<Seen>, heard: impl FnOnce() -> Option<Heard>) {
        let dt = self.last_tick.elapsed().as_secs_f64();
        self.last_tick = Instant::now();
        self.step(seen, heard, dt, unix_now());
    }

    /// [`Tracker::tick`] with the clock given: `dt` seconds since the last step, `now` in
    /// Unix seconds.
    pub fn step(&mut self, seen: Option<Seen>, heard: impl FnOnce() -> Option<Heard>, dt: f64, now: u64) {
        let Some(seen) = seen else {
            self.current = None;
            return;
        };
        if self.current.as_ref().is_none_or(|c| c.rev != seen.rev) {
            self.current = heard().map(|heard| Current { rev: seen.rev, listen: Listen::new(heard.duration), heard, started: None });
        }
        let Some(c) = self.current.as_mut() else { return };
        if seen.playing && c.started.is_none() {
            c.started = Some(now);
        }
        match c.listen.tick(seen.playing, seen.position, dt) {
            Some(Event::Announce) => {
                if let Some(song) = &c.heard.song {
                    self.scrobbling.now_playing(song);
                }
            }
            Some(Event::Counted) => {
                let started = c.started.unwrap_or(now);
                self.plays.record(&c.heard.path, started);
                if let Some(song) = &c.heard.song {
                    self.scrobbling.listened(song, started);
                }
            }
            Some(Event::Again) => c.started = Some(now),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bitspring-stats-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn heard(path: &str, duration: f64) -> Heard {
        Heard { path: path.into(), duration, song: None }
    }

    /// Play track `rev` (`path`, `duration` s long) from `from` for `secs`.
    fn play(t: &mut Tracker, rev: u64, path: &str, duration: f64, from: f64, secs: f64, now: u64) {
        let mut at = from;
        for _ in 0..=(secs * 25.0).round() as usize {
            t.step(Some(Seen { rev, playing: true, position: at }), || Some(heard(path, duration)), 0.04, now);
            at += 0.04;
        }
    }

    #[test]
    fn counts_survive_a_restart_and_keep_the_latest_time() {
        let d = tmpdir("restart");
        let file = d.join("plays.json");
        let p = Plays::at(Some(file.clone()));
        assert_eq!(p.of("/m/a.flac"), Played::default());
        p.record("/m/a.flac", 1_000);
        p.record("/m/a.flac", 5_000);
        // A listen recorded late (an older time) raises the count, not the date.
        assert_eq!(p.record("/m/a.flac", 2_000), Played { count: 3, last: 5_000 });
        p.record("qobuz:track:42", 7_000);
        assert_eq!(p.rev(), 4);
        // The app starts again — and the library was rescanned meanwhile, which this file
        // has no part in.
        let again = Plays::at(Some(file));
        assert_eq!(again.of("/m/a.flac"), Played { count: 3, last: 5_000 });
        assert_eq!(again.of("qobuz:track:42"), Played { count: 1, last: 7_000 });
        assert_eq!(again.of("/m/never.flac"), Played::default());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_track_counts_once_it_was_listened_to_and_not_before() {
        let plays = Arc::new(Plays::default());
        let mut t = Tracker::new(plays.clone(), Scrobbling::off());
        // Skipped after 20 s of a 200 s track.
        play(&mut t, 1, "/m/skipped.flac", 200.0, 0.0, 20.0, 100);
        // The next one plays through: counted with the time it started.
        play(&mut t, 2, "/m/heard.flac", 200.0, 0.0, 200.0, 130);
        // A 20 s interlude plays through: too short to count.
        play(&mut t, 3, "/m/short.flac", 20.0, 0.0, 20.0, 340);
        assert_eq!(plays.of("/m/skipped.flac").count, 0);
        assert_eq!(plays.of("/m/heard.flac"), Played { count: 1, last: 130 });
        assert_eq!(plays.of("/m/short.flac").count, 0);
    }

    #[test]
    fn pausing_and_seeking_do_not_run_up_the_count() {
        let plays = Arc::new(Plays::default());
        let mut t = Tracker::new(plays.clone(), Scrobbling::off());
        play(&mut t, 1, "/m/a.flac", 200.0, 0.0, 30.0, 100);
        // Paused for an hour, then dragged back and forth along the whole track.
        for _ in 0..90_000 {
            t.step(Some(Seen { rev: 1, playing: false, position: 30.0 }), || None, 0.04, 100);
        }
        for i in 0..2_000 {
            let position = if i % 2 == 0 { 180.0 } else { 10.0 };
            t.step(Some(Seen { rev: 1, playing: true, position }), || None, 0.01, 100);
        }
        assert_eq!(plays.of("/m/a.flac").count, 0);
        // Played to the end from the middle: now it has sounded for half its length. Once.
        play(&mut t, 1, "/m/a.flac", 200.0, 100.0, 100.0, 100);
        assert_eq!(plays.of("/m/a.flac").count, 1);
    }

    #[test]
    fn the_same_track_twice_in_a_row_counts_twice() {
        let plays = Arc::new(Plays::default());
        let mut t = Tracker::new(plays.clone(), Scrobbling::off());
        play(&mut t, 1, "/m/a.flac", 100.0, 0.0, 100.0, 100);
        // Queued again: another start, another listen.
        play(&mut t, 2, "/m/a.flac", 100.0, 0.0, 100.0, 300);
        // "Previous" at its end starts it over without a new start: a listen all the same.
        play(&mut t, 2, "/m/a.flac", 100.0, 0.0, 100.0, 500);
        assert_eq!(plays.of("/m/a.flac"), Played { count: 3, last: 500 });
    }

    #[test]
    fn stopping_ends_the_listen() {
        let plays = Arc::new(Plays::default());
        let mut t = Tracker::new(plays.clone(), Scrobbling::off());
        play(&mut t, 1, "/m/a.flac", 100.0, 0.0, 40.0, 100);
        t.step(None, || None, 0.04, 100);
        // Started again from the top (the same start number can't come back, but even so):
        // the 40 s from before the stop are gone.
        play(&mut t, 1, "/m/a.flac", 100.0, 0.0, 40.0, 200);
        assert_eq!(plays.of("/m/a.flac").count, 0);
    }
}
