//! CUE sheets: a whole album ripped to one file ("image + cue", the way EAC rips a CD), played
//! track by track.
//!
//! Each track of such an album becomes a library entry of its own whose path names the image
//! and the part of it that is the track: `<image>#cue:<start>-<end>`, in CD frames (1/75 s,
//! the unit the sheet itself counts in, exact at every common sample rate). The end is empty
//! for the last track, which runs to the end of the file. Queues, playlists and the player pass
//! that path around like any other; [`split`] turns it back into the file and the range.
//!
//! A track starts at its INDEX 01 and runs to the next track's INDEX 01, so a pregap
//! (INDEX 00) plays at the end of the track before, as on the CD and as other players do.
//! Sheets that list one file per track (made next to an already split rip) change nothing.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::decode::TrackMeta;

/// CD frames per second.
pub const FRAMES_PER_SEC: u64 = 75;

const MARK: &str = "#cue:";

/// The part of an image file one track is, in CD frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: u64,
    /// None: to the end of the file.
    pub end: Option<u64>,
}

impl Range {
    /// First sample frame at `rate`.
    pub fn start_at(self, rate: u32) -> usize {
        (self.start * rate as u64 / FRAMES_PER_SEC) as usize
    }

    /// Sample frames the track has at `rate`; None when it runs to the end of the file.
    pub fn len_at(self, rate: u32) -> Option<usize> {
        self.end.map(|e| Range { start: e, end: None }.start_at(rate).saturating_sub(self.start_at(rate)))
    }
}

/// The library path of a track that is `range` of `file`.
pub fn track_path(file: &str, range: Range) -> String {
    match range.end {
        Some(e) => format!("{file}{MARK}{}-{e}", range.start),
        None => format!("{file}{MARK}{}-", range.start),
    }
}

/// A library path, split into the file it reads and the range of it (None: the whole file).
pub fn split(path: &str) -> (&str, Option<Range>) {
    let Some(at) = path.rfind(MARK) else { return (path, None) };
    let (file, rest) = (&path[..at], &path[at + MARK.len()..]);
    let Some((a, b)) = rest.split_once('-') else { return (path, None) };
    let Ok(start) = a.parse() else { return (path, None) };
    let end = match b {
        "" => None,
        b => match b.parse() {
            Ok(e) if e > start => Some(e),
            _ => return (path, None),
        },
    };
    // A real file whose name happens to end like this stays a file.
    if Path::new(path).exists() {
        return (path, None);
    }
    (file, Some(Range { start, end }))
}

/// One track as the sheet describes it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CueTrack {
    pub number: u64,
    pub title: Option<String>,
    pub performer: Option<String>,
    /// INDEX 01, in CD frames from the start of the file.
    pub start: u64,
}

/// The tracks a sheet places in one file, and the album they belong to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CueFile {
    /// The file name as written in the sheet.
    pub name: String,
    pub album: Option<String>,
    pub performer: Option<String>,
    pub genre: Option<String>,
    pub date: Option<String>,
    pub disc: Option<u64>,
    pub tracks: Vec<CueTrack>,
}

/// "mm:ss:ff" → CD frames.
fn msf(s: &str) -> Option<u64> {
    let mut it = s.split(':').map(|p| p.trim().parse::<u64>().ok());
    let (m, s, f) = (it.next()??, it.next()??, it.next()??);
    (s < 60 && f < FRAMES_PER_SEC && it.next().is_none()).then_some((m * 60 + s) * FRAMES_PER_SEC + f)
}

/// The value after a keyword: `"quoted text"` or a bare word.
fn value(rest: &str) -> String {
    let rest = rest.trim();
    match rest.strip_prefix('"') {
        Some(r) => r.rfind('"').map_or(r, |e| &r[..e]).to_string(),
        None => rest.to_string(),
    }
}

/// Read a sheet (any encoding [`crate::playlist::decode_text`] knows: rippers write UTF-8,
/// or the Windows code page of the day).
pub fn parse(bytes: &[u8]) -> Vec<CueFile> {
    let text = crate::playlist::decode_text(bytes);
    let mut head = CueFile::default();
    let mut files: Vec<CueFile> = Vec::new();
    // Whether a TRACK has begun: TITLE/PERFORMER before it are the album's.
    let mut in_track = false;
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{FEFF}');
        let (key, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let nonempty = |v: String| (!v.trim().is_empty()).then(|| v.trim().to_string());
        match key.to_ascii_uppercase().as_str() {
            "FILE" => {
                // The file type (WAVE, MP3…) follows the name.
                let rest = rest.trim();
                let name = if rest.starts_with('"') { value(rest) } else { rest.rsplit_once(' ').map_or(rest, |(n, _)| n).to_string() };
                files.push(CueFile { name, tracks: Vec::new(), ..head.clone() });
                in_track = false;
            }
            "TRACK" => {
                let Some(file) = files.last_mut() else { continue };
                let number = rest.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(file.tracks.len() as u64 + 1);
                // Data tracks (enhanced CDs) have no audio to play.
                let audio = rest.split_whitespace().nth(1).is_none_or(|k| k.eq_ignore_ascii_case("AUDIO"));
                if audio {
                    file.tracks.push(CueTrack { number, start: u64::MAX, ..Default::default() });
                    in_track = true;
                } else {
                    in_track = false;
                }
            }
            "INDEX" => {
                let mut p = rest.split_whitespace();
                let (Some(n), Some(at)) = (p.next(), p.next()) else { continue };
                let Some(t) = files.last_mut().and_then(|f| f.tracks.last_mut()).filter(|_| in_track) else { continue };
                if n.parse::<u32>() == Ok(1) && let Some(at) = msf(at) {
                    t.start = at;
                }
            }
            "TITLE" | "PERFORMER" => {
                let v = nonempty(value(rest));
                let title = key.eq_ignore_ascii_case("TITLE");
                match files.last_mut().and_then(|f| f.tracks.last_mut()).filter(|_| in_track) {
                    Some(t) if title => t.title = v,
                    Some(t) => t.performer = v,
                    None => {
                        let target = match files.last_mut() {
                            Some(f) => f,
                            None => &mut head,
                        };
                        if title { target.album = v } else { target.performer = v }
                    }
                }
            }
            "REM" if !in_track => {
                let (k, v) = rest.trim().split_once(char::is_whitespace).unwrap_or((rest.trim(), ""));
                let v = nonempty(value(v));
                let target = match files.last_mut() {
                    Some(f) => f,
                    None => &mut head,
                };
                match k.to_ascii_uppercase().as_str() {
                    "GENRE" => target.genre = v,
                    "DATE" => target.date = v,
                    "DISCNUMBER" => target.disc = v.and_then(|d| d.split('/').next()?.trim().parse().ok()),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    for f in &mut files {
        f.tracks.retain(|t| t.start != u64::MAX);
        f.tracks.sort_by_key(|t| t.start);
    }
    files
}

impl CueFile {
    /// Each track's range: up to the next track's start, the last one to the end.
    pub fn ranges(&self) -> Vec<Range> {
        let starts: Vec<u64> = self.tracks.iter().map(|t| t.start).collect();
        starts.iter().enumerate().map(|(i, &s)| Range { start: s, end: starts.get(i + 1).copied().filter(|&e| e > s) }).collect()
    }
}

impl CueFile {
    /// The tags of track `i` as the sheet gives them (the image's own tags fill the gaps).
    pub fn meta(&self, i: usize) -> TrackMeta {
        let t = &self.tracks[i];
        TrackMeta {
            title: t.title.clone(),
            artist: t.performer.clone().or_else(|| self.performer.clone()),
            album: self.album.clone(),
            album_artist: self.performer.clone(),
            genre: self.genre.clone(),
            year: self.date.clone(),
            track_number: Some(t.number),
            disc_number: self.disc,
            cover: None,
        }
    }
}

/// The CUE sheets in `dir` (not in its subfolders).
pub fn sheets_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| is_sheet(p)).collect()
}

/// A `.cue` file (macOS's `._` resource-fork shadows on other drives excluded).
pub fn is_sheet(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("cue"))
        && !p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.'))
}

/// What the sheet next to `file` says about its track at `range` (for the player, which gets
/// only the path).
pub fn track_meta(file: &Path, range: Range) -> Option<TrackMeta> {
    let image = images(&[file.to_path_buf()], &sheets_in(file.parent()?)).into_iter().next()?;
    let i = image.sheet.tracks.iter().position(|t| t.start == range.start)?;
    Some(image.sheet.meta(i))
}

/// `files` (in order) with each image replaced by its tracks: what dropping a folder queues.
/// Returns the paths, and the sheet's tags for the tracks that came from one.
pub fn expand(files: Vec<PathBuf>) -> Vec<(String, Option<TrackMeta>)> {
    let mut dirs: Vec<&Path> = files.iter().filter_map(|f| f.parent()).collect();
    dirs.sort();
    dirs.dedup();
    let sheets: Vec<PathBuf> = dirs.iter().flat_map(|d| sheets_in(d)).collect();
    let images = images(&files, &sheets);
    let mut out = Vec::new();
    for f in &files {
        let name = f.to_string_lossy();
        match images.iter().find(|i| &i.file == f) {
            Some(img) => {
                for (i, r) in img.sheet.ranges().into_iter().enumerate() {
                    out.push((track_path(&name, r), Some(img.sheet.meta(i))));
                }
            }
            None => out.push((name.into_owned(), None)),
        }
    }
    out
}

/// An image file a sheet splits into tracks.
#[derive(Debug, Clone)]
pub struct Image {
    pub file: PathBuf,
    pub sheet: CueFile,
}

/// The image files the sheets among `files` split into tracks. A sheet's file is found by its
/// name, or else by its name with another audio extension (a sheet written for the WAV that
/// was compressed to FLAC afterwards). Only files a sheet gives two or more tracks count; when
/// two sheets describe the same file (EAC's plain and ReplayGain ones), the first one wins.
pub fn images(files: &[PathBuf], sheets: &[PathBuf]) -> Vec<Image> {
    let present: HashSet<&Path> = files.iter().map(PathBuf::as_path).collect();
    // dir → (lowercased stem → audio files)
    let mut by_stem: HashMap<(&Path, String), Vec<&PathBuf>> = HashMap::new();
    for f in files {
        if let (Some(dir), Some(stem)) = (f.parent(), f.file_stem()) {
            by_stem.entry((dir, stem.to_string_lossy().to_lowercase())).or_default().push(f);
        }
    }
    let mut out: Vec<Image> = Vec::new();
    let mut sheets = sheets.to_vec();
    // Plain sheets before their variants ("Cue.cue" before "Cue_RepG.cue").
    sheets.sort_by_key(|p| (p.parent().map(Path::to_path_buf), p.as_os_str().len(), p.clone()));
    for sheet in &sheets {
        let Ok(bytes) = std::fs::read(sheet) else { continue };
        // A sheet is text of a few kilobytes; anything big is something else.
        if bytes.len() > 1 << 20 {
            continue;
        }
        let Some(dir) = sheet.parent() else { continue };
        for f in parse(&bytes) {
            if f.tracks.len() < 2 {
                continue;
            }
            let named = dir.join(f.name.replace('\\', "/"));
            let file = if present.contains(named.as_path()) {
                Some(named)
            } else {
                let stem = Path::new(&f.name.replace('\\', "/")).file_stem().map(|s| s.to_string_lossy().to_lowercase());
                stem.and_then(|s| by_stem.get(&(dir, s))).and_then(|c| c.first()).map(|p| (*p).clone())
            };
            let Some(file) = file.filter(|f| !crate::dsd::is_dsd_path(f)) else { continue };
            if out.iter().any(|i| i.file == file) {
                continue;
            }
            out.push(Image { file, sheet: f });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const EAC: &str = "REM GENRE Rock\r\nREM DATE 2001\r\nREM DISCID B30D6C0C\r\nPERFORMER \"Би-2\"\r\nTITLE \"Мяу Кисс Ми\"\r\nFILE \"Image_CD.flac\" WAVE\r\n  TRACK 01 AUDIO\r\n    TITLE \"Мяу Кисс Ми\"\r\n    PERFORMER \"Би-2\"\r\n    INDEX 01 00:00:00\r\n  TRACK 02 AUDIO\r\n    TITLE \"Моя любовь\"\r\n    INDEX 01 03:50:16\r\n  TRACK 03 AUDIO\r\n    TITLE \"Ёлочный сок\"\r\n    INDEX 00 14:06:62\r\n    INDEX 01 14:07:62\r\n";

    #[test]
    fn reads_an_eac_sheet_in_windows_1251() {
        let (bytes, _, _) = encoding_rs::WINDOWS_1251.encode(EAC);
        let files = parse(&bytes);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.name, "Image_CD.flac");
        assert_eq!(f.album.as_deref(), Some("Мяу Кисс Ми"));
        assert_eq!(f.performer.as_deref(), Some("Би-2"));
        assert_eq!((f.genre.as_deref(), f.date.as_deref()), (Some("Rock"), Some("2001")));
        assert_eq!(f.tracks.len(), 3);
        assert_eq!(f.tracks[1].title.as_deref(), Some("Моя любовь"));
        assert_eq!(f.tracks[1].performer, None);
        assert_eq!(f.tracks[1].start, (3 * 60 + 50) * 75 + 16);
        // The pregap (INDEX 00) belongs to the track before.
        assert_eq!(f.tracks[2].start, (14 * 60 + 7) * 75 + 62);
        let r = f.ranges();
        assert_eq!(r[1], Range { start: f.tracks[1].start, end: Some(f.tracks[2].start) });
        assert_eq!(r[2].end, None);
    }

    #[test]
    fn one_file_per_track_sheets_name_each_file() {
        let s = "FILE \"01 - A.wav\" WAVE\n TRACK 01 AUDIO\n  INDEX 01 00:00:00\nFILE \"02 - B.wav\" WAVE\n TRACK 02 AUDIO\n  INDEX 00 00:00:00\n  INDEX 01 00:01:00\n";
        let files = parse(s.as_bytes());
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.tracks.len() == 1));
    }

    #[test]
    fn paths_round_trip() {
        let r = Range { start: 17266, end: Some(39553) };
        let p = track_path("/m/a #1/Image.flac", r);
        assert_eq!(p, "/m/a #1/Image.flac#cue:17266-39553");
        assert_eq!(split(&p), ("/m/a #1/Image.flac", Some(r)));
        let last = track_path("/m/Image.flac", Range { start: 5, end: None });
        assert_eq!(split(&last), ("/m/Image.flac", Some(Range { start: 5, end: None })));
        assert_eq!(split("/m/plain.flac"), ("/m/plain.flac", None));
        assert_eq!(split("/m/x#cue:abc"), ("/m/x#cue:abc", None));
    }

    #[test]
    fn ranges_in_samples() {
        let r = Range { start: 75, end: Some(150) };
        assert_eq!(r.start_at(44_100), 44_100);
        assert_eq!(r.len_at(44_100), Some(44_100));
        assert_eq!(r.len_at(96_000), Some(96_000));
        assert_eq!(Range { start: 75, end: None }.len_at(44_100), None);
    }

    #[test]
    fn finds_images_by_name_or_by_stem() {
        let dir = std::env::temp_dir().join(format!("bitspring-cue-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let flac = dir.join("Album.flac");
        // Written for the WAV; the FLAC sits there instead.
        let sheet = "TITLE \"A\"\nFILE \"Album.wav\" WAVE\n TRACK 01 AUDIO\n  INDEX 01 00:00:00\n TRACK 02 AUDIO\n  INDEX 01 01:00:00\n";
        std::fs::write(dir.join("Album.cue"), sheet).unwrap();
        std::fs::write(dir.join("Album_RepG.cue"), sheet).unwrap();
        let found = images(std::slice::from_ref(&flac), &[dir.join("Album_RepG.cue"), dir.join("Album.cue")]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].file, flac);
        assert_eq!(found[0].sheet.tracks.len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
