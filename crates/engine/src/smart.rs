//! Smart playlists: rules over the library ("genre contains jazz, hi-res, added in the last
//! 30 days"), evaluated whenever the playlist is opened, so it follows the library.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::library::TrackInfo;
use crate::plays::Played;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmartRules {
    /// Every rule must hold (else any one).
    pub match_all: bool,
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub sort: Sort,
    /// At most this many tracks.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    pub field: Field,
    pub op: Op,
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Field {
    Title,
    Artist,
    Album,
    AlbumArtist,
    Genre,
    /// Number.
    Year,
    /// FLAC, ALAC, MP3…
    Format,
    /// In kHz.
    SampleRate,
    Bits,
    /// In minutes.
    Duration,
    /// When the file last changed (what "recently added" goes by), in days.
    Added,
    HiRes,
    Lossless,
    /// The quality check's word: "genuine", "suspect" or "unchecked".
    Quality,
    /// How many times the track was listened to (0: never).
    Plays,
    /// When it was last listened to, in days.
    LastPlayed,
    /// Dynamic range (the DR figure), where it has been measured.
    Dr,
    /// Loudness range (LRA) in LU, where it has been measured.
    Lra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Op {
    Contains,
    NotContains,
    Is,
    IsNot,
    StartsWith,
    Greater,
    Less,
    /// Added (or listened to) within the last N days.
    WithinDays,
    /// Not listened to within the last N days — or ever.
    NotWithinDays,
    IsTrue,
    IsFalse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Sort {
    /// Album by album, in track order.
    #[default]
    Album,
    Artist,
    Title,
    /// Oldest first.
    Year,
    /// Newest first.
    Added,
    /// Longest first.
    Duration,
    Random,
    /// Listened to most often first.
    MostPlayed,
    /// Listened to most recently first.
    RecentlyPlayed,
    /// Longest not listened to first: never played, then the oldest listen.
    LongestUnplayed,
}

/// What the rules ask about a track that the library index doesn't hold.
pub struct Facts<'a> {
    /// The quality check's word for a track.
    pub quality: &'a dyn Fn(&TrackInfo) -> &'static str,
    /// How often and when it was listened to.
    pub played: &'a dyn Fn(&TrackInfo) -> Played,
    /// Its dynamic range (DR) and loudness range (LRA, in LU), when measured.
    pub dynamics: &'a dyn Fn(&TrackInfo) -> (Option<f64>, Option<f64>),
    /// The time, in Unix seconds.
    pub now: u64,
}

/// The library tracks the rules pick, in their order (indices into `tracks`).
pub fn select(tracks: &[TrackInfo], rules: &SmartRules, facts: &Facts) -> Vec<usize> {
    let mut picked: Vec<usize> = tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            if rules.rules.is_empty() {
                return true;
            }
            let mut hits = rules.rules.iter().map(|r| matches(t, r, facts));
            if rules.match_all { hits.all(|h| h) } else { hits.any(|h| h) }
        })
        .map(|(i, _)| i)
        .collect();
    let key_text = |s: &Option<String>| s.as_deref().unwrap_or("").to_lowercase();
    let album_order = |a: &TrackInfo, b: &TrackInfo| {
        let artist = |t: &TrackInfo| key_text(&t.album_artist.clone().or_else(|| t.artist.clone()));
        artist(a)
            .cmp(&artist(b))
            .then_with(|| key_text(&a.album).cmp(&key_text(&b.album)))
            .then_with(|| a.disc_number.cmp(&b.disc_number))
            .then_with(|| a.track_number.cmp(&b.track_number))
            .then_with(|| a.path.cmp(&b.path))
    };
    match rules.sort {
        Sort::Album => picked.sort_by(|&a, &b| album_order(&tracks[a], &tracks[b])),
        Sort::Artist => picked.sort_by(|&a, &b| {
            key_text(&tracks[a].artist).cmp(&key_text(&tracks[b].artist)).then_with(|| album_order(&tracks[a], &tracks[b]))
        }),
        Sort::Title => picked.sort_by(|&a, &b| tracks[a].title.to_lowercase().cmp(&tracks[b].title.to_lowercase())),
        Sort::Year => picked.sort_by(|&a, &b| year(&tracks[a]).cmp(&year(&tracks[b])).then_with(|| album_order(&tracks[a], &tracks[b]))),
        Sort::Added => picked.sort_by(|&a, &b| tracks[b].mtime.cmp(&tracks[a].mtime).then_with(|| album_order(&tracks[a], &tracks[b]))),
        Sort::Duration => picked.sort_by(|&a, &b| tracks[b].duration.partial_cmp(&tracks[a].duration).unwrap_or(Ordering::Equal)),
        Sort::Random => shuffle(&mut picked),
        Sort::MostPlayed | Sort::RecentlyPlayed | Sort::LongestUnplayed => {
            let played: Vec<Played> = tracks.iter().map(facts.played).collect();
            picked.sort_by(|&a, &b| {
                let (pa, pb) = (played[a], played[b]);
                match rules.sort {
                    Sort::MostPlayed => pb.count.cmp(&pa.count).then_with(|| pb.last.cmp(&pa.last)),
                    Sort::RecentlyPlayed => pb.last.cmp(&pa.last),
                    // (Never listened to has no time: 0, before everything.)
                    _ => pa.last.cmp(&pb.last),
                }
                .then_with(|| album_order(&tracks[a], &tracks[b]))
            });
        }
    }
    if let Some(n) = rules.limit {
        picked.truncate(n);
    }
    picked
}

fn year(t: &TrackInfo) -> Option<u32> {
    t.year.as_deref().and_then(|y| y.get(..4)).and_then(|y| y.parse().ok())
}

fn matches(t: &TrackInfo, r: &Rule, facts: &Facts) -> bool {
    let (quality, now) = (facts.quality, facts.now);
    let value = r.value.trim();
    let text = |s: Option<&str>| {
        let s = s.unwrap_or("").to_lowercase();
        let v = value.to_lowercase();
        match r.op {
            Op::Contains => s.contains(&v),
            Op::NotContains => !s.contains(&v),
            Op::Is => s == v,
            Op::IsNot => s != v,
            Op::StartsWith => s.starts_with(&v),
            _ => false,
        }
    };
    let number = |n: Option<f64>| {
        let (Some(n), Ok(v)) = (n, value.replace(',', ".").parse::<f64>()) else { return false };
        match r.op {
            Op::Is => (n - v).abs() < 1e-6,
            Op::IsNot => (n - v).abs() >= 1e-6,
            Op::Greater => n > v,
            Op::Less => n < v,
            _ => false,
        }
    };
    let flag = |b: bool| match r.op {
        Op::IsTrue => b,
        Op::IsFalse => !b,
        _ => false,
    };
    match r.field {
        Field::Title => text(Some(&t.title)),
        Field::Artist => text(t.artist.as_deref()),
        Field::Album => text(t.album.as_deref()),
        Field::AlbumArtist => text(t.album_artist.as_deref().or(t.artist.as_deref())),
        Field::Genre => text(t.genre.as_deref()),
        Field::Format => text(Some(&t.format)),
        Field::Quality => text(Some(quality(t))),
        Field::Year => number(year(t).map(f64::from)),
        Field::SampleRate => number(Some(t.sample_rate as f64 / 1000.0)),
        Field::Bits => number(t.bits.map(f64::from)),
        Field::Duration => number(Some(t.duration / 60.0)),
        Field::Added => match (r.op, value.parse::<u64>()) {
            (Op::WithinDays, Ok(days)) => now.saturating_sub(t.mtime) <= days * 86_400,
            _ => false,
        },
        Field::Plays => number(Some(f64::from((facts.played)(t).count))),
        Field::LastPlayed => {
            let played = (facts.played)(t);
            let within = |days: u64| played.count > 0 && now.saturating_sub(played.last) <= days * 86_400;
            match (r.op, value.parse::<u64>()) {
                (Op::WithinDays, Ok(days)) => within(days),
                (Op::NotWithinDays, Ok(days)) => !within(days),
                _ => false,
            }
        }
        // (A track nobody measured matches no rule about its dynamics. The DR figure is
        // compared as it is shown: rounded.)
        Field::Dr => number((facts.dynamics)(t).0.map(f64::round)),
        Field::Lra => number((facts.dynamics)(t).1),
        Field::HiRes => flag(t.hi_res()),
        Field::Lossless => flag(t.lossless),
    }
}

fn shuffle(v: &mut [usize]) {
    use std::hash::{BuildHasher, Hasher};
    let mut seed = std::collections::hash_map::RandomState::new().build_hasher().finish() | 1;
    for i in (1..v.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        v.swap(i, (seed % (i as u64 + 1)) as usize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn track(path: &str, artist: &str, album: &str, genre: &str, year: &str, rate: u32, bits: u32, n: u64, mtime: u64) -> TrackInfo {
        TrackInfo {
            path: path.into(),
            title: format!("Song {n}"),
            artist: Some(artist.into()),
            album_artist: None,
            album: Some(album.into()),
            genre: Some(genre.into()),
            year: Some(year.into()),
            track_number: Some(n),
            disc_number: None,
            duration: 200.0 + n as f64,
            sample_rate: rate,
            bits: Some(bits),
            channels: 2,
            format: "FLAC".into(),
            lossless: true,
            has_cover: false,
            size: 1,
            mtime,
        }
    }

    fn lib() -> Vec<TrackInfo> {
        vec![
            track("/b/2.flac", "Miles Davis", "Kind of Blue", "Jazz", "1959", 44_100, 16, 2, 1_000),
            track("/b/1.flac", "Miles Davis", "Kind of Blue", "Jazz", "1959", 44_100, 16, 1, 1_000),
            track("/a/1.flac", "Radiohead", "OK Computer", "Rock", "1997", 96_000, 24, 1, 2_000_000),
            track("/c/1.flac", "Bill Evans", "Waltz for Debby", "Cool Jazz", "1961", 192_000, 24, 1, 1_990_000),
        ]
    }

    const NOW: u64 = 2_000_000;

    fn rules(match_all: bool, rules: Vec<(Field, Op, &str)>, sort: Sort) -> SmartRules {
        SmartRules { match_all, rules: rules.into_iter().map(|(field, op, v)| Rule { field, op, value: v.into() }).collect(), sort, limit: None }
    }

    fn never(_: &TrackInfo) -> Played {
        Played::default()
    }

    fn unmeasured(_: &TrackInfo) -> (Option<f64>, Option<f64>) {
        (None, None)
    }

    fn paths(lib: &[TrackInfo], r: &SmartRules) -> Vec<String> {
        select(lib, r, &Facts { quality: &|_| "unchecked", played: &never, dynamics: &unmeasured, now: NOW }).into_iter().map(|i| lib[i].path.clone()).collect()
    }

    /// The clock of the listening tests (years after the files were added).
    const LATER: u64 = 100_000_000;

    /// The library with listens: /b/1 three times (long ago), /a/1 once (an hour ago),
    /// /c/1 twice (ten days ago), /b/2 never.
    fn played(t: &TrackInfo) -> Played {
        match t.path.as_str() {
            "/b/1.flac" => Played { count: 3, last: LATER - 400 * 86_400 },
            "/a/1.flac" => Played { count: 1, last: LATER - 3_600 },
            "/c/1.flac" => Played { count: 2, last: LATER - 10 * 86_400 },
            _ => Played::default(),
        }
    }

    fn listened(lib: &[TrackInfo], r: &SmartRules) -> Vec<String> {
        select(lib, r, &Facts { quality: &|_| "unchecked", played: &played, dynamics: &unmeasured, now: LATER }).into_iter().map(|i| lib[i].path.clone()).collect()
    }

    #[test]
    fn genre_and_hi_res_together() {
        let l = lib();
        let r = rules(true, vec![(Field::Genre, Op::Contains, "jazz"), (Field::HiRes, Op::IsTrue, "")], Sort::Album);
        assert_eq!(paths(&l, &r), ["/c/1.flac"]);
        let r = rules(false, vec![(Field::Genre, Op::Is, "rock"), (Field::SampleRate, Op::Greater, "100")], Sort::Album);
        assert_eq!(paths(&l, &r), ["/c/1.flac", "/a/1.flac"]);
    }

    #[test]
    fn albums_play_in_track_order() {
        let l = lib();
        let r = rules(true, vec![(Field::Artist, Op::StartsWith, "miles")], Sort::Album);
        assert_eq!(paths(&l, &r), ["/b/1.flac", "/b/2.flac"]);
    }

    #[test]
    fn recently_added_newest_first_and_limited() {
        let l = lib();
        let mut r = rules(true, vec![(Field::Added, Op::WithinDays, "1")], Sort::Added);
        assert_eq!(paths(&l, &r), ["/a/1.flac", "/c/1.flac"]);
        r.limit = Some(1);
        assert_eq!(paths(&l, &r), ["/a/1.flac"]);
    }

    #[test]
    fn years_compare_as_numbers() {
        let l = lib();
        let r = rules(true, vec![(Field::Year, Op::Less, "1960")], Sort::Year);
        assert_eq!(paths(&l, &r), ["/b/1.flac", "/b/2.flac"]);
    }

    #[test]
    fn the_quality_word_is_a_field() {
        let l = lib();
        let r = rules(true, vec![(Field::Quality, Op::Is, "suspect")], Sort::Album);
        let word = |t: &TrackInfo| if t.path == "/a/1.flac" { "suspect" } else { "genuine" };
        let picked: Vec<usize> = select(&l, &r, &Facts { quality: &word, played: &never, dynamics: &unmeasured, now: NOW });
        assert_eq!(picked, [2]);
    }

    /// "Squashed" and "wide" playlists: rules on the DR figure (as shown, rounded) and on
    /// the loudness range. A track nobody measured is in neither.
    #[test]
    fn dynamics_are_fields_where_measured() {
        let l = lib();
        let dynamics = |t: &TrackInfo| match t.path.as_str() {
            "/a/1.flac" => (Some(5.6), Some(3.2)),
            "/c/1.flac" => (Some(13.4), Some(14.0)),
            "/b/1.flac" => (Some(6.4), None),
            _ => (None, None),
        };
        let picked = |r: &SmartRules| -> Vec<String> {
            select(&l, r, &Facts { quality: &|_| "unchecked", played: &never, dynamics: &dynamics, now: NOW }).into_iter().map(|i| l[i].path.clone()).collect()
        };
        assert_eq!(picked(&rules(true, vec![(Field::Dr, Op::Less, "7")], Sort::Album)), ["/b/1.flac", "/a/1.flac"]);
        assert_eq!(picked(&rules(true, vec![(Field::Dr, Op::Is, "6")], Sort::Album)), ["/b/1.flac", "/a/1.flac"]);
        assert_eq!(picked(&rules(true, vec![(Field::Dr, Op::Greater, "12")], Sort::Album)), ["/c/1.flac"]);
        assert_eq!(picked(&rules(true, vec![(Field::Lra, Op::Greater, "10")], Sort::Album)), ["/c/1.flac"]);
        assert_eq!(picked(&rules(true, vec![(Field::Lra, Op::Less, "10")], Sort::Album)), ["/a/1.flac"]);
        assert!(paths(&l, &rules(true, vec![(Field::Dr, Op::Less, "99")], Sort::Album)).is_empty());
    }

    #[test]
    fn never_listened_to() {
        let l = lib();
        let r = rules(true, vec![(Field::Plays, Op::Is, "0")], Sort::Album);
        assert_eq!(listened(&l, &r), ["/b/2.flac"]);
        // Nothing has been listened to yet: everything is "never".
        assert_eq!(paths(&l, &r).len(), 4);
    }

    #[test]
    fn most_played_first() {
        let l = lib();
        let r = rules(true, vec![(Field::Plays, Op::Greater, "0")], Sort::MostPlayed);
        assert_eq!(listened(&l, &r), ["/b/1.flac", "/c/1.flac", "/a/1.flac"]);
        let r = rules(true, vec![(Field::Plays, Op::Greater, "1")], Sort::MostPlayed);
        assert_eq!(listened(&l, &r), ["/b/1.flac", "/c/1.flac"]);
    }

    #[test]
    fn not_listened_to_for_a_long_time() {
        let l = lib();
        // Not in the last 30 days: the one from long ago, and the one never played — which
        // comes first, having waited longest.
        let r = rules(true, vec![(Field::LastPlayed, Op::NotWithinDays, "30")], Sort::LongestUnplayed);
        assert_eq!(listened(&l, &r), ["/b/2.flac", "/b/1.flac"]);
        // Listened to before, but not lately.
        let r = rules(true, vec![(Field::LastPlayed, Op::NotWithinDays, "30"), (Field::Plays, Op::Greater, "0")], Sort::LongestUnplayed);
        assert_eq!(listened(&l, &r), ["/b/1.flac"]);
    }

    #[test]
    fn listened_to_lately_most_recent_first() {
        let l = lib();
        let r = rules(true, vec![(Field::LastPlayed, Op::WithinDays, "30")], Sort::RecentlyPlayed);
        assert_eq!(listened(&l, &r), ["/a/1.flac", "/c/1.flac"]);
        let r = rules(true, vec![(Field::LastPlayed, Op::WithinDays, "1")], Sort::RecentlyPlayed);
        assert_eq!(listened(&l, &r), ["/a/1.flac"]);
        // A track never listened to was not listened to lately, whatever the clock says.
        let r = rules(true, vec![(Field::LastPlayed, Op::WithinDays, "100000")], Sort::Album);
        assert_eq!(listened(&l, &r).len(), 3);
    }

    #[test]
    fn listening_rules_round_trip_as_json() {
        let r = rules(true, vec![(Field::LastPlayed, Op::NotWithinDays, "90"), (Field::Plays, Op::Less, "3")], Sort::LongestUnplayed);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""field":"lastPlayed""#) && json.contains(r#""op":"notWithinDays""#) && json.contains(r#""sort":"longestUnplayed""#), "{json}");
        assert_eq!(serde_json::from_str::<SmartRules>(&json).unwrap(), r);
    }

    #[test]
    fn rules_round_trip_as_json() {
        let r = rules(true, vec![(Field::AlbumArtist, Op::NotContains, "various")], Sort::Random);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""field":"albumArtist""#) && json.contains(r#""op":"notContains""#), "{json}");
        assert_eq!(serde_json::from_str::<SmartRules>(&json).unwrap(), r);
    }
}
