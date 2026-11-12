//! Smart playlists: rules over the library ("genre contains jazz, hi-res, added in the last
//! 30 days"), evaluated whenever the playlist is opened, so it follows the library.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::library::TrackInfo;

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
    /// Added within the last N days.
    WithinDays,
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
}

/// The library tracks the rules pick, in their order (indices into `tracks`). `quality`
/// answers the quality check's word for a track; `now` is the time in Unix seconds.
pub fn select(tracks: &[TrackInfo], rules: &SmartRules, quality: &dyn Fn(&TrackInfo) -> &'static str, now: u64) -> Vec<usize> {
    let mut picked: Vec<usize> = tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            if rules.rules.is_empty() {
                return true;
            }
            let mut hits = rules.rules.iter().map(|r| matches(t, r, quality, now));
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
    }
    if let Some(n) = rules.limit {
        picked.truncate(n);
    }
    picked
}

fn year(t: &TrackInfo) -> Option<u32> {
    t.year.as_deref().and_then(|y| y.get(..4)).and_then(|y| y.parse().ok())
}

fn matches(t: &TrackInfo, r: &Rule, quality: &dyn Fn(&TrackInfo) -> &'static str, now: u64) -> bool {
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

    fn paths(lib: &[TrackInfo], r: &SmartRules) -> Vec<String> {
        select(lib, r, &|_| "unchecked", NOW).into_iter().map(|i| lib[i].path.clone()).collect()
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
        let picked: Vec<usize> = select(&l, &r, &|t| if t.path == "/a/1.flac" { "suspect" } else { "genuine" }, NOW);
        assert_eq!(picked, [2]);
    }

    #[test]
    fn rules_round_trip_as_json() {
        let r = rules(true, vec![(Field::AlbumArtist, Op::NotContains, "various")], Sort::Random);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""field":"albumArtist""#) && json.contains(r#""op":"notContains""#), "{json}");
        assert_eq!(serde_json::from_str::<SmartRules>(&json).unwrap(), r);
    }
}
