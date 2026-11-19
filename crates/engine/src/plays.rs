//! Listening statistics: when a track counts as listened to, and what is kept about it.
//!
//! One rule, the one Last.fm asks of scrobblers, serves both the play count kept on the device
//! and scrobbling: the track is longer than 30 seconds, and it has played for half its length
//! or for four minutes, whichever comes first. Only time the track actually sounded counts:
//! a pause adds nothing, and neither does jumping ahead.

use serde::{Deserialize, Serialize};

/// Shorter tracks never count.
pub const MIN_TRACK_SECS: f64 = 30.0;
/// This much of a long track is enough.
pub const ENOUGH_SECS: f64 = 240.0;
/// A track that has sounded this long is announced as playing now (skipping through a queue
/// announces nothing).
pub const ANNOUNCE_AFTER_SECS: f64 = 3.0;

/// How long a track of `duration` seconds must sound to count as listened to. None: it is too
/// short to count at all. A track of unknown length (0) counts after four minutes.
pub fn needed_secs(duration: f64) -> Option<f64> {
    if duration <= 0.0 {
        return Some(ENOUGH_SECS);
    }
    (duration > MIN_TRACK_SECS).then(|| (duration / 2.0).min(ENOUGH_SECS))
}

/// What is kept about a track: how often it was listened to, and when last (Unix seconds).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Played {
    pub count: u32,
    pub last: u64,
}

/// What a tick of [`Listen`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The track has sounded long enough to say it is playing now.
    Announce,
    /// The track counts as listened to (once per listen).
    Counted,
    /// A counted track was started over from its beginning: a new listen begins.
    Again,
}

/// One listen of one track, fed with the player's state as it plays.
#[derive(Debug, Clone)]
pub struct Listen {
    needed: Option<f64>,
    /// Seconds the track has sounded.
    heard: f64,
    /// Wall time spent playing since the position last moved.
    pending: f64,
    position: Option<f64>,
    announced: bool,
    counted: bool,
}

/// The longest stretch of wall time one step of the position may account for: a player that
/// reports its position rarely (a network player, once a second or so) still counts in full,
/// a machine that slept does not count the night.
const MAX_PENDING: f64 = 5.0;
/// Back at the start: within this many seconds of it, having been at least `REWOUND_BY` later.
const START_WITHIN: f64 = 3.0;
const REWOUND_BY: f64 = 10.0;

impl Listen {
    pub fn new(duration: f64) -> Self {
        Self { needed: needed_secs(duration), heard: 0.0, pending: 0.0, position: None, announced: false, counted: false }
    }

    /// Seconds the track has sounded in this listen.
    pub fn heard_secs(&self) -> f64 {
        self.heard
    }

    /// The player's state now: whether sound is coming out (not paused, waiting or ended),
    /// where in the track it is, and how much wall time passed since the last call.
    ///
    /// Time counts only when both the clock and the position moved forward, and never more
    /// than the smaller of the two: a seek ahead moves the position without the clock, a
    /// stalled output moves the clock without the position.
    pub fn tick(&mut self, playing: bool, position: f64, dt: f64) -> Option<Event> {
        let before = self.position.replace(position);
        if self.counted && position < START_WITHIN && before.is_some_and(|b| b - position >= REWOUND_BY) {
            *self = Self { needed: self.needed, position: Some(position), ..Self::new(0.0) };
            return Some(Event::Again);
        }
        if !playing {
            self.pending = 0.0;
            return None;
        }
        self.pending = (self.pending + dt.max(0.0)).min(MAX_PENDING);
        let moved = before.map_or(0.0, |b| position - b);
        if moved < 0.0 {
            // A jump back: what waited belongs to no stretch of the track.
            self.pending = 0.0;
        } else if moved > 0.0 {
            let credit = moved.min(self.pending);
            self.heard += credit;
            self.pending -= credit;
        }
        if !self.counted && self.needed.is_some_and(|n| self.heard >= n) {
            self.counted = true;
            // (Counted without having been announced can't happen: 15 s > 3 s. Still.)
            self.announced = true;
            return Some(Event::Counted);
        }
        if !self.announced && self.heard >= ANNOUNCE_AFTER_SECS {
            self.announced = true;
            return Some(Event::Announce);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Play from `from` for `secs` at 25 ticks a second; the events on the way.
    fn play(l: &mut Listen, from: f64, secs: f64) -> Vec<Event> {
        let dt = 0.04;
        let mut events = Vec::new();
        let mut at = from;
        l.tick(true, at, 0.0);
        for _ in 0..(secs / dt).round() as usize {
            at += dt;
            events.extend(l.tick(true, at, dt));
        }
        events
    }

    #[test]
    fn half_the_track_or_four_minutes() {
        assert_eq!(needed_secs(200.0), Some(100.0));
        assert_eq!(needed_secs(1200.0), Some(240.0));
        assert_eq!(needed_secs(480.0), Some(240.0));
        assert_eq!(needed_secs(31.0), Some(15.5));
        assert_eq!(needed_secs(30.0), None);
        assert_eq!(needed_secs(12.0), None);
        // A stream that doesn't know its length yet.
        assert_eq!(needed_secs(0.0), Some(240.0));
    }

    #[test]
    fn counts_once_when_half_has_played() {
        let mut l = Listen::new(200.0);
        assert_eq!(play(&mut l, 0.0, 99.0), [Event::Announce]);
        assert_eq!(play(&mut l, 99.0, 2.0), [Event::Counted]);
        // The rest of the track adds no second count.
        assert_eq!(play(&mut l, 101.0, 99.0), []);
    }

    #[test]
    fn a_short_track_is_announced_but_never_counts() {
        let mut l = Listen::new(25.0);
        assert_eq!(play(&mut l, 0.0, 25.0), [Event::Announce]);
    }

    #[test]
    fn a_pause_adds_nothing() {
        let mut l = Listen::new(200.0);
        play(&mut l, 0.0, 60.0);
        // Ten minutes paused at the same place.
        for _ in 0..15_000 {
            assert_eq!(l.tick(false, 60.0, 0.04), None);
        }
        assert!((l.heard_secs() - 60.0).abs() < 0.1);
        // A player that says it plays while its position stands still adds nothing either.
        for _ in 0..15_000 {
            assert_eq!(l.tick(true, 60.0, 0.04), None);
        }
        assert!((l.heard_secs() - 60.0).abs() < 0.1);
        assert_eq!(play(&mut l, 60.0, 41.0), [Event::Counted]);
    }

    #[test]
    fn seeking_adds_nothing() {
        let mut l = Listen::new(200.0);
        play(&mut l, 0.0, 10.0);
        // Jumping around the whole track, again and again.
        for i in 0..500 {
            assert_eq!(l.tick(true, if i % 2 == 0 { 190.0 } else { 5.0 }, 0.04), None);
        }
        assert!(l.heard_secs() < 31.0, "{}", l.heard_secs());
        // To the end and through it: 10 s at the start and 10 s at the end are not half.
        assert_eq!(play(&mut l, 190.0, 10.0), []);
    }

    #[test]
    fn listening_in_pieces_adds_up() {
        let mut l = Listen::new(200.0);
        play(&mut l, 0.0, 40.0);
        play(&mut l, 100.0, 40.0);
        assert_eq!(play(&mut l, 150.0, 21.0), [Event::Counted]);
    }

    #[test]
    fn a_player_that_reports_its_position_once_a_second_counts_in_full() {
        let mut l = Listen::new(200.0);
        let mut events = Vec::new();
        l.tick(true, 0.0, 0.0);
        for tick in 1..=(101 * 25) {
            let position = (tick / 25) as f64;
            events.extend(l.tick(true, position, 0.04));
        }
        assert_eq!(events, [Event::Announce, Event::Counted]);
    }

    #[test]
    fn a_sleeping_machine_does_not_count_the_night() {
        let mut l = Listen::new(1200.0);
        play(&mut l, 0.0, 10.0);
        // Eight hours pass between two ticks; the position moved on by a minute meanwhile.
        l.tick(true, 70.0, 8.0 * 3600.0);
        assert!(l.heard_secs() < 16.0, "{}", l.heard_secs());
    }

    #[test]
    fn starting_a_counted_track_over_is_a_new_listen() {
        let mut l = Listen::new(100.0);
        assert_eq!(play(&mut l, 0.0, 60.0), [Event::Announce, Event::Counted]);
        assert_eq!(l.tick(true, 0.0, 0.04), Some(Event::Again));
        assert_eq!(play(&mut l, 0.0, 60.0), [Event::Announce, Event::Counted]);
        // Before it has counted, going back to the start begins nothing new.
        let mut l = Listen::new(100.0);
        play(&mut l, 0.0, 30.0);
        assert_eq!(l.tick(true, 0.0, 0.04), None);
        assert_eq!(play(&mut l, 0.0, 21.0), [Event::Counted]);
    }
}
