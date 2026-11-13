//! Song lyrics: from an `.lrc` file beside the track, from the file's own tags, or (fetched
//! by the app, when the listener allows it) from LRCLIB.
//!
//! Timed lyrics are LRC text — `[01:23.45]a line` — wherever they come from: a sidecar file,
//! a LYRICS / USLT / ©lyr tag that taggers fill with LRC, or LRCLIB's `syncedLyrics`. Text
//! without timestamps is shown as it is, without following the music.

use std::path::Path;

use lofty::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Line {
    /// Seconds into the track the line starts at; `None` for untimed lyrics.
    pub time: Option<f64>,
    /// Empty: an instrumental break between verses.
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    /// An `.lrc` file next to the track.
    Sidecar,
    /// The file's tags.
    Embedded,
    /// LRCLIB (lrclib.net).
    Online,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lyrics {
    /// In time order when `synced`.
    pub lines: Vec<Line>,
    /// Every line has a time.
    pub synced: bool,
    pub source: Source,
}

/// Lyrics kept with the track itself: an `.lrc` beside it (it wins: it's what the listener
/// added on purpose), else the tags. `None` for a track of a CUE image or SACD ISO (one file
/// holds the whole album) and for files without any.
pub fn read_local(path: &Path) -> Option<Lyrics> {
    let s = path.to_str()?;
    if crate::source_file(s) != s {
        return None;
    }
    sidecar(path).or_else(|| embedded(path))
}

/// `Song.lrc` for `Song.flac` (and `Song.flac.lrc`, which some tools write).
fn sidecar(path: &Path) -> Option<Lyrics> {
    let candidates = [path.with_extension("lrc"), path.with_extension(format!("{}.lrc", path.extension()?.to_str()?))];
    candidates.iter().find_map(|p| {
        // Lyrics are a few kB; anything big isn't lyrics.
        if std::fs::metadata(p).ok()?.len() > 512 << 10 {
            return None;
        }
        let bytes = std::fs::read(p).ok()?;
        parse(&decode_text(&bytes), Source::Sidecar)
    })
}

/// UTF-8 (with or without BOM), UTF-16 with a BOM, or else the Windows code page old LRC
/// files were saved in.
fn decode_text(bytes: &[u8]) -> String {
    let (enc, bom) = encoding_rs::Encoding::for_bom(bytes).unwrap_or((encoding_rs::UTF_8, 0));
    let body = &bytes[bom..];
    if enc == encoding_rs::UTF_8 {
        if let Ok(s) = std::str::from_utf8(body) {
            return s.to_string();
        }
        return encoding_rs::WINDOWS_1252.decode(body).0.into_owned();
    }
    enc.decode_without_bom_handling(body).0.into_owned()
}

fn embedded(path: &Path) -> Option<Lyrics> {
    let file = lofty::read_from_path(path).ok()?;
    file.tags().iter().find_map(|tag| {
        [ItemKey::Lyrics, ItemKey::UnsyncLyrics]
            .into_iter()
            .filter_map(|k| tag.get_string(k))
            .find_map(|text| parse(text, Source::Embedded))
    })
}

/// Lyrics from text: LRC when any line carries a timestamp, plain lines otherwise. `None`
/// when there are no words at all.
pub fn parse(text: &str, source: Source) -> Option<Lyrics> {
    let text = text.trim_start_matches('\u{feff}');
    let mut offset = 0.0;
    let mut timed: Vec<(f64, String)> = Vec::new();
    let mut plain: Vec<String> = Vec::new();
    for raw in text.lines() {
        let mut rest = raw.trim();
        let mut times = Vec::new();
        let mut tagged = false;
        // Leading `[..]` tags: timestamps, or ID tags like [ar:Artist] / [offset:+250].
        while let Some(inner) = rest.strip_prefix('[').and_then(|r| r.split_once(']')) {
            let (tag, after) = inner;
            if let Some(t) = timestamp(tag) {
                times.push(t);
            } else if let Some((key, value)) = tag.split_once(':')
                && key.trim().chars().all(|c| c.is_ascii_alphabetic())
                && !key.trim().is_empty()
            {
                if key.trim().eq_ignore_ascii_case("offset") {
                    // Milliseconds; positive shows the words earlier.
                    offset = value.trim().trim_start_matches('+').parse::<f64>().unwrap_or(0.0) / 1000.0;
                }
            } else {
                break; // a bracket that is part of the words ("[Chorus]")
            }
            tagged = true;
            rest = after.trim_start();
        }
        let words = strip_word_times(rest);
        if !times.is_empty() {
            timed.extend(times.into_iter().map(|t| (t, words.clone())));
        } else if !tagged {
            plain.push(words);
        }
    }
    let lines: Vec<Line> = if !timed.is_empty() {
        // Several stamps on one line ([00:12][01:40]chorus) put it at each; sort them in.
        timed.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut lines: Vec<Line> = Vec::new();
        for (t, text) in timed {
            let time = (t - offset).max(0.0);
            // One break marker between verses, and none before the first words.
            if text.is_empty() && lines.last().is_none_or(|l| l.text.is_empty()) {
                continue;
            }
            lines.push(Line { time: Some(time), text });
        }
        while lines.last().is_some_and(|l| l.text.is_empty()) {
            lines.pop();
        }
        lines
    } else {
        let mut lines: Vec<Line> = Vec::new();
        for text in plain {
            if text.is_empty() && lines.last().is_none_or(|l| l.text.is_empty()) {
                continue;
            }
            lines.push(Line { time: None, text });
        }
        while lines.last().is_some_and(|l| l.text.is_empty()) {
            lines.pop();
        }
        lines
    };
    if lines.iter().all(|l| l.text.is_empty()) {
        return None;
    }
    let synced = lines.first().is_some_and(|l| l.time.is_some());
    Some(Lyrics { lines, synced, source })
}

/// `mm:ss`, `mm:ss.xx`, `mm:ss:xx` (some editors) or `hh:mm:ss.xx`, in seconds.
fn timestamp(tag: &str) -> Option<f64> {
    let tag = tag.trim();
    if !tag.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let (main, frac) = match tag.rsplit_once('.') {
        Some((m, f)) => (m, Some(f)),
        None => (tag, None),
    };
    let parts: Vec<&str> = main.split(':').collect();
    let num = |s: &str| -> Option<f64> { (!s.is_empty() && s.chars().all(|c| c.is_ascii_digit())).then(|| s.parse().ok())? };
    let (h, m, s, frac) = match (parts.as_slice(), frac) {
        ([m, s], f) => (0.0, num(m)?, num(s)?, f),
        // mm:ss:xx (some editors write hundredths after a colon): songs are shorter than an hour.
        ([m, s, x], None) => (0.0, num(m)?, num(s)?, Some(*x)),
        ([h, m, s], Some(f)) => (num(h)?, num(m)?, num(s)?, Some(f)),
        _ => return None,
    };
    if s >= 60.0 {
        return None;
    }
    let frac = match frac {
        Some(f) => num(f)? / 10f64.powi(f.len() as i32),
        None => 0.0,
    };
    Some(h * 3600.0 + m * 60.0 + s + frac)
}

/// Enhanced LRC times each word (`<00:12.30>Hello <00:12.80>world`): keep the words.
fn strip_word_times(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('<') {
        match rest[start + 1..].find('>') {
            Some(end) if timestamp(&rest[start + 1..start + 1 + end]).is_some() => {
                out.push_str(&rest[..start]);
                rest = &rest[start + 1 + end + 1..];
            }
            _ => {
                out.push_str(&rest[..=start]);
                rest = &rest[start + 1..];
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One of LRCLIB's answers (`/api/get` gives one, `/api/search` a list).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LrclibRecord {
    #[serde(default)]
    pub track_name: Option<String>,
    #[serde(default)]
    pub artist_name: Option<String>,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub instrumental: bool,
    #[serde(default)]
    pub plain_lyrics: Option<String>,
    #[serde(default)]
    pub synced_lyrics: Option<String>,
}

impl LrclibRecord {
    /// Timed lyrics when the record has them, else the plain text.
    pub fn lyrics(&self) -> Option<Lyrics> {
        if self.instrumental {
            return None;
        }
        self.synced_lyrics
            .as_deref()
            .and_then(|t| parse(t, Source::Online))
            .filter(|l| l.synced)
            .or_else(|| self.plain_lyrics.as_deref().and_then(|t| parse(t, Source::Online)))
    }
}

/// The search result that is this recording: its length within a few seconds of ours (a
/// live take or a radio edit of the same song has other words, or the same at other times),
/// and the closest one, timed lyrics first.
pub fn best_match(records: &[LrclibRecord], duration: Option<f64>) -> Option<Lyrics> {
    let close = |r: &LrclibRecord| match (duration, r.duration) {
        (Some(a), Some(b)) => (a - b).abs(),
        _ => 0.0,
    };
    let mut fits: Vec<&LrclibRecord> = records.iter().filter(|r| close(r) <= 3.0 && r.lyrics().is_some()).collect();
    fits.sort_by(|a, b| {
        let synced = |r: &LrclibRecord| r.synced_lyrics.as_deref().is_some_and(|s| !s.trim().is_empty());
        synced(b).cmp(&synced(a)).then(close(a).total_cmp(&close(b)))
    });
    fits.first().and_then(|r| r.lyrics())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(l: &Lyrics) -> Vec<&str> {
        l.lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn reads_timestamps_in_every_spelling() {
        assert_eq!(timestamp("01:23.45"), Some(83.45));
        assert_eq!(timestamp("01:23.4"), Some(83.4));
        assert_eq!(timestamp("01:23.456"), Some(83.456));
        assert_eq!(timestamp("01:23"), Some(83.0));
        assert_eq!(timestamp("01:23:45"), Some(83.45));
        assert_eq!(timestamp("1:01:23.5"), Some(3683.5));
        assert_eq!(timestamp("ar:Someone"), None);
        assert_eq!(timestamp("00:75.00"), None);
        assert_eq!(timestamp("Chorus"), None);
    }

    #[test]
    fn parses_lrc_with_tags_offset_and_repeats() {
        let lrc = "\u{feff}[ar:Artist]\n[ti:Song]\n[offset:+500]\n[00:10.00]First\n[00:20.00][01:00.00]Chorus\n[00:30.00]\n[00:40.00]Second\n";
        let l = parse(lrc, Source::Sidecar).unwrap();
        assert!(l.synced);
        assert_eq!(texts(&l), ["First", "Chorus", "", "Second", "Chorus"]);
        assert_eq!(l.lines[0].time, Some(9.5));
        assert_eq!(l.lines[4].time, Some(59.5));
    }

    #[test]
    fn drops_leading_trailing_and_repeated_breaks() {
        let lrc = "[00:00.00]\n[00:05.00]Words\n[00:09.00]\n[00:10.00]\n[00:12.00]More\n[00:20.00]\n";
        let l = parse(lrc, Source::Embedded).unwrap();
        assert_eq!(texts(&l), ["Words", "", "More"]);
    }

    #[test]
    fn keeps_words_of_enhanced_lrc_and_brackets_in_text() {
        let l = parse("[00:01.00]<00:01.00>Hello <00:01.50>world\n[00:03.00][Chorus] la", Source::Online).unwrap();
        assert_eq!(texts(&l), ["Hello world", "[Chorus] la"]);
    }

    #[test]
    fn plain_text_stays_untimed() {
        let l = parse("\n\nOne\nTwo\n\n\n\nThree\n\n", Source::Embedded).unwrap();
        assert!(!l.synced);
        assert_eq!(texts(&l), ["One", "Two", "", "Three"]);
        assert!(l.lines.iter().all(|l| l.time.is_none()));
    }

    #[test]
    fn nothing_to_show_is_none() {
        assert!(parse("", Source::Embedded).is_none());
        assert!(parse("[ar:x]\n[00:01.00]\n", Source::Embedded).is_none());
    }

    #[test]
    fn decodes_old_code_pages_and_utf16() {
        assert_eq!(decode_text(b"caf\xe9"), "café");
        assert_eq!(decode_text(&[0xff, 0xfe, b'h', 0, b'i', 0]), "hi");
    }

    #[test]
    fn finds_sidecar_beside_the_track() {
        let dir = std::env::temp_dir().join(format!("bitspring-lyrics-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let track = dir.join("01 Song.flac");
        std::fs::write(&track, b"not really flac").unwrap();
        assert!(read_local(&track).is_none());
        std::fs::write(dir.join("01 Song.lrc"), "[00:01.00]Hi").unwrap();
        let l = read_local(&track).unwrap();
        assert_eq!(l.source, Source::Sidecar);
        assert_eq!(texts(&l), ["Hi"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_lyrics_from_the_files_tags() {
        use crate::net::encode::{Codec, Encoder, Format};
        use lofty::config::WriteOptions;
        use lofty::tag::{Tag, TagType};
        let dir = std::env::temp_dir().join(format!("bitspring-lyrics-tag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A small real FLAC file from our own encoder, with a LYRICS comment in LRC.
        let mut enc = Encoder::new(Format { codec: Codec::Flac, rate: 44_100, channels: 2, bits: 16 });
        let mut bytes = enc.header();
        let samples: Vec<i32> = (0..44_100 * 2).map(|i| ((i * 37) % 2000) - 1000).collect();
        for block in samples.chunks(4096 * 2) {
            enc.encode(block, &mut bytes);
        }
        let track = dir.join("02 Tagged.flac");
        std::fs::write(&track, bytes).unwrap();
        let mut tag = Tag::new(TagType::VorbisComments);
        tag.insert_text(ItemKey::Lyrics, "[00:00.50]From the tag\n[00:01.20]Second line".into());
        tag.save_to_path(&track, WriteOptions::default()).unwrap();

        let l = read_local(&track).unwrap();
        assert_eq!(l.source, Source::Embedded);
        assert!(l.synced);
        assert_eq!(texts(&l), ["From the tag", "Second line"]);
        // A sidecar wins over the tags.
        std::fs::write(dir.join("02 Tagged.lrc"), "Beside it").unwrap();
        assert_eq!(read_local(&track).unwrap().source, Source::Sidecar);
        // A track of a CUE image has neither.
        assert!(read_local(Path::new(&format!("{}#cue:0-44100", track.display()))).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn picks_the_recording_of_the_same_length_timed_first() {
        let rec = |d: f64, synced: Option<&str>, plain: Option<&str>| LrclibRecord {
            track_name: None,
            artist_name: None,
            duration: Some(d),
            instrumental: false,
            plain_lyrics: plain.map(str::to_string),
            synced_lyrics: synced.map(str::to_string),
        };
        let records = [rec(300.0, Some("[00:01.00]live"), None), rec(201.0, None, Some("plain")), rec(199.0, Some("[00:01.00]studio"), None)];
        assert_eq!(texts(&best_match(&records, Some(200.0)).unwrap()), ["studio"]);
        assert!(best_match(&records[..1], Some(200.0)).is_none());
    }
}
