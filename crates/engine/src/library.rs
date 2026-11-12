//! Music library: scan folders for audio files, read tags without decoding, group into albums.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use symphonia::core::codecs::audio::AudioCodecId;
use symphonia::core::codecs::audio::well_known::{
    CODEC_ID_AAC, CODEC_ID_ALAC, CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3, CODEC_ID_OPUS, CODEC_ID_VORBIS,
};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::decode::read_meta;

pub const AUDIO_EXTENSIONS: [&str; 12] = ["flac", "wav", "aif", "aiff", "m4a", "alac", "mp3", "ogg", "caf", "mka", "dsf", "dff"];

pub fn is_audio(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Everything the library knows about one file. Cheap to (de)serialise for the on-disk index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackInfo {
    pub path: String,
    pub title: String,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub year: Option<String>,
    pub track_number: Option<u64>,
    pub disc_number: Option<u64>,
    pub duration: f64,
    pub sample_rate: u32,
    pub bits: Option<u32>,
    pub channels: usize,
    /// Container/codec as the user knows it: FLAC, ALAC, WAV, AIFF, MP3, AAC…
    pub format: String,
    pub lossless: bool,
    pub has_cover: bool,
    /// For incremental rescans: unchanged size + mtime ⇒ reuse this entry.
    pub size: u64,
    pub mtime: u64,
}

impl TrackInfo {
    /// Above CD quality: more than 16 bits or faster than 48 kHz.
    pub fn hi_res(&self) -> bool {
        self.lossless && (self.sample_rate > 48_000 || self.bits.is_some_and(|b| b > 16))
        // (DSD: its 1-bit rate is in the MHz, so it always counts.)
    }
}

pub type Cover = (String, Vec<u8>);

fn file_stamp(p: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(p).ok()?;
    let mtime = m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some((m.len(), mtime))
}

/// Read tags and stream parameters only — no audio is decoded, so this takes milliseconds.
pub fn scan_file(path: &Path) -> Result<(TrackInfo, Option<Cover>)> {
    let (size, mtime) = file_stamp(path).ok_or_else(|| anyhow!("cannot stat {}", path.display()))?;
    if crate::dsd::is_dsd_path(path) {
        return scan_dsd(path, size, mtime);
    }
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    hint.with_extension(&ext);
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .context("unrecognised format")?;
    let mut meta = read_meta(&mut *format);
    crate::tags::fill_from_file(path, &mut meta);
    let track = format.default_track(TrackType::Audio).ok_or_else(|| anyhow!("no audio track"))?;
    let audio = track.codec_params.as_ref().and_then(|p| p.audio()).ok_or_else(|| anyhow!("no audio params"))?;
    let sample_rate = audio.sample_rate.unwrap_or(0);
    let bits = audio.bits_per_sample.or_else(|| {
        (audio.codec == CODEC_ID_ALAC).then(|| alac_bit_depth(audio.extra_data.as_deref()?)).flatten()
    });
    let duration = match (track.num_frames, sample_rate) {
        (Some(n), r) if r > 0 => n as f64 / r as f64,
        _ => 0.0,
    };

    // MP4 containers hold either ALAC (lossless) or AAC; only the codec id tells them apart.
    let (format_name, lossless) = match ext.as_str() {
        _ if audio.codec == CODEC_ID_ALAC => ("ALAC", true),
        _ if audio.codec == CODEC_ID_AAC => ("AAC", false),
        "flac" => ("FLAC", true),
        "wav" => ("WAV", true),
        "aif" | "aiff" => ("AIFF", true),
        "caf" => ("CAF", bits.is_some()),
        "mp3" => ("MP3", false),
        "ogg" => ("Vorbis", false),
        "mka" => ("MKA", bits.is_some()),
        _ => ("?", false),
    };

    let title = meta.title.clone().unwrap_or_else(|| {
        path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    });
    let info = TrackInfo {
        path: path.to_string_lossy().into_owned(),
        title,
        artist: meta.artist,
        album_artist: meta.album_artist,
        album: meta.album,
        genre: meta.genre,
        year: meta.year,
        track_number: meta.track_number,
        disc_number: meta.disc_number,
        duration,
        sample_rate,
        bits,
        channels: audio.channels.as_ref().map_or(2, |c| c.count()),
        format: format_name.to_string(),
        lossless,
        has_cover: meta.cover.is_some(),
        size,
        mtime,
    };
    Ok((info, meta.cover))
}

/// DSD files: the rate is the 1-bit rate (2 822 400 for DSD64) and `bits` is 1, which the UI
/// shows as "DSD64".
fn scan_dsd(path: &Path, size: u64, mtime: u64) -> Result<(TrackInfo, Option<Cover>)> {
    let h = crate::dsd::probe(path)?;
    let meta = h.meta.clone();
    let title = meta.title.clone().unwrap_or_else(|| {
        path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    });
    let info = TrackInfo {
        path: path.to_string_lossy().into_owned(),
        title,
        artist: meta.artist,
        album_artist: meta.album_artist,
        album: meta.album,
        genre: meta.genre,
        year: meta.year,
        track_number: meta.track_number,
        disc_number: meta.disc_number,
        duration: h.duration_secs(),
        sample_rate: h.rate,
        bits: Some(1),
        channels: h.channels,
        format: h.container.to_string(),
        lossless: true,
        has_cover: meta.cover.is_some(),
        size,
        mtime,
    };
    Ok((info, meta.cover))
}

/// Bit depth from an ALAC magic cookie (Apple's ALACSpecificConfig: frameLength u32,
/// compatibleVersion u8, bitDepth u8, …), optionally wrapped in an `alac` atom header.
/// Symphonia parses this too but doesn't expose it before decoding.
fn alac_bit_depth(cookie: &[u8]) -> Option<u32> {
    let config = match cookie.get(4..8) {
        Some(b"alac") => cookie.get(12..)?,
        _ => cookie,
    };
    let depth = *config.get(5)? as u32;
    matches!(depth, 16 | 20 | 24 | 32).then_some(depth)
}

/// All audio files under `folders`, recursively, skipping hidden entries.
pub fn find_audio_files(folders: &[PathBuf]) -> Vec<PathBuf> {
    find_files(folders).0
}

/// All audio files and CUE sheets under `folders`, recursively, skipping hidden entries.
fn find_files(folders: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    fn walk(dir: &Path, audio: &mut Vec<PathBuf>, sheets: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for p in entries.filter_map(|e| e.ok().map(|e| e.path())) {
            if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.')) {
                continue;
            }
            if p.is_dir() {
                walk(&p, audio, sheets);
            } else if is_audio(&p) {
                audio.push(p);
            } else if crate::cue::is_sheet(&p) {
                sheets.push(p);
            }
        }
    }
    let (mut audio, mut sheets) = (Vec::new(), Vec::new());
    for f in folders {
        walk(f, &mut audio, &mut sheets);
    }
    for v in [&mut audio, &mut sheets] {
        v.sort();
        v.dedup();
    }
    (audio, sheets)
}

/// The tracks a CUE sheet makes of one image file: the image's stream parameters and tags,
/// with the sheet's titles, numbers and lengths.
fn image_tracks(image: &crate::cue::Image, file: &TrackInfo) -> Vec<TrackInfo> {
    let sheet = &image.sheet;
    let total = (file.duration * crate::cue::FRAMES_PER_SEC as f64).round() as u64;
    sheet
        .ranges()
        .into_iter()
        .enumerate()
        .filter(|(_, r)| r.start < total || total == 0)
        .map(|(i, r)| {
            let m = sheet.meta(i);
            let end = r.end.unwrap_or(total).max(r.start);
            TrackInfo {
                path: crate::cue::track_path(&file.path, r),
                title: m.title.unwrap_or_else(|| format!("{} {:02}", file.title, sheet.tracks[i].number)),
                artist: m.artist.or_else(|| file.artist.clone()),
                album_artist: m.album_artist.or_else(|| file.album_artist.clone()),
                album: m.album.or_else(|| file.album.clone()),
                genre: m.genre.or_else(|| file.genre.clone()),
                year: m.year.or_else(|| file.year.clone()),
                track_number: m.track_number,
                disc_number: m.disc_number.or(file.disc_number),
                duration: (end - r.start) as f64 / crate::cue::FRAMES_PER_SEC as f64,
                ..file.clone()
            }
        })
        .collect()
}

pub struct ScanResult {
    pub tracks: Vec<TrackInfo>,
    /// Files that couldn't be read, with the reason.
    pub failed: Vec<(String, String)>,
}

/// Scan `folders`, reusing entries from `previous` whose size and mtime are unchanged.
/// Runs on all cores. `on_progress(done, total)` is called as files complete; `on_cover` gets
/// the embedded artwork of each newly scanned file that has one.
pub fn scan(
    folders: &[PathBuf],
    previous: &[TrackInfo],
    on_progress: impl Fn(usize, usize) + Sync,
    on_cover: impl Fn(&TrackInfo, Cover) + Sync,
) -> ScanResult {
    let (files, sheets) = find_files(folders);
    // Albums ripped to one file with a CUE sheet: the file gives way to the sheet's tracks.
    let images = crate::cue::images(&files, &sheets);
    let files: Vec<PathBuf> = files.into_iter().filter(|f| !images.iter().any(|i| &i.file == f)).collect();
    let known: HashMap<&str, &TrackInfo> = previous.iter().map(|t| (t.path.as_str(), t)).collect();
    // An image scanned before (same size and time) needs no new look for its cover.
    let known_images: std::collections::HashSet<(&str, u64, u64)> =
        previous.iter().filter_map(|t| match crate::cue::split(&t.path) {
            (file, Some(_)) => Some((file, t.size, t.mtime)),
            _ => None,
        }).collect();
    let total = files.len() + images.len();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let results: Mutex<Vec<Vec<TrackInfo>>> = Mutex::new(vec![Vec::new(); total]);
    let failed = Mutex::new(Vec::new());
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);

    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= total {
                    break;
                }
                let Some(path) = files.get(i) else {
                    // An image: its header (milliseconds), then the sheet's tracks.
                    let image = &images[i - files.len()];
                    let key = image.file.to_string_lossy();
                    let entry = match scan_file(&image.file) {
                        Ok((t, cover)) => {
                            let tracks = image_tracks(image, &t);
                            if let (Some(c), Some(first)) = (cover, tracks.first())
                                && !known_images.contains(&(key.as_ref(), t.size, t.mtime))
                            {
                                on_cover(first, c);
                            }
                            tracks
                        }
                        Err(e) => {
                            failed.lock().unwrap().push((key.into_owned(), format!("{e:#}")));
                            Vec::new()
                        }
                    };
                    results.lock().unwrap()[i] = entry;
                    on_progress(done.fetch_add(1, Ordering::Relaxed) + 1, total);
                    continue;
                };
                let key = path.to_string_lossy();
                let reused = known
                    .get(key.as_ref())
                    .filter(|t| file_stamp(path) == Some((t.size, t.mtime)))
                    .map(|t| (*t).clone());
                let entry = match reused {
                    Some(t) => Some(t),
                    None => match scan_file(path) {
                        Ok((t, cover)) => {
                            if let Some(c) = cover {
                                on_cover(&t, c);
                            }
                            Some(t)
                        }
                        Err(e) => {
                            failed.lock().unwrap().push((key.into_owned(), format!("{e:#}")));
                            None
                        }
                    },
                };
                results.lock().unwrap()[i] = entry.into_iter().collect();
                on_progress(done.fetch_add(1, Ordering::Relaxed) + 1, total);
            });
        }
    });

    ScanResult {
        tracks: results.into_inner().unwrap().into_iter().flatten().collect(),
        failed: failed.into_inner().unwrap(),
    }
}

// ---------------------------------------------------------------------------------------------
// Albums
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Album {
    /// Stable id derived from artist + title (survives rescans).
    pub id: String,
    pub title: String,
    pub artist: String,
    pub year: Option<String>,
    pub genre: Option<String>,
    /// Indices into the track list, in play order (disc, track number, path).
    pub tracks: Vec<usize>,
    pub duration: f64,
    /// Best format on the album, e.g. 24 bit / 96 kHz.
    pub max_rate: u32,
    pub max_bits: Option<u32>,
    pub format: String,
    pub hi_res: bool,
    /// Path of a track that has embedded art, if any.
    pub cover_track: Option<String>,
}

/// FNV-1a: tiny, stable across runs and platforms (unlike std's randomised hasher).
fn stable_id(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

pub const UNKNOWN_ARTIST: &str = "Unknown artist";

/// Album identity: album artist (or artist), and album title — or, for untagged files, the
/// folder they sit in, so a folder of loose files still reads as one album.
fn album_key(t: &TrackInfo) -> (String, String) {
    let artist = t.album_artist.clone().or_else(|| t.artist.clone()).unwrap_or_else(|| UNKNOWN_ARTIST.into());
    let title = t.album.clone().unwrap_or_else(|| {
        Path::new(&t.path)
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    (artist, title)
}

/// Id of the album a track belongs to (same as `Album::id`).
pub fn album_id(t: &TrackInfo) -> String {
    let (artist, title) = album_key(t);
    stable_id(&format!("{}\u{1f}{}", artist.to_lowercase(), title.to_lowercase()))
}

/// Picture files rippers commonly leave next to the tracks.
pub fn folder_image(track_path: &Path) -> Option<PathBuf> {
    let dir = track_path.parent()?;
    ["cover", "folder", "front", "Cover", "Folder", "Front", "AlbumArt"]
        .iter()
        .flat_map(|n| ["jpg", "jpeg", "png"].map(|e| dir.join(format!("{n}.{e}"))))
        .find(|p| p.is_file())
}

pub fn group_albums(tracks: &[TrackInfo]) -> Vec<Album> {
    let mut groups: HashMap<String, (String, String, Vec<usize>)> = HashMap::new();
    for (i, t) in tracks.iter().enumerate() {
        let (artist, title) = album_key(t);
        let key = format!("{}\u{1f}{}", artist.to_lowercase(), title.to_lowercase());
        groups.entry(key).or_insert_with(|| (artist, title, Vec::new())).2.push(i);
    }
    let mut albums: Vec<Album> = groups
        .into_iter()
        .map(|(key, (artist, title, mut idx))| {
            idx.sort_by(|&a, &b| {
                let (ta, tb) = (&tracks[a], &tracks[b]);
                (ta.disc_number.unwrap_or(1), ta.track_number.unwrap_or(u64::MAX), &ta.path)
                    .cmp(&(tb.disc_number.unwrap_or(1), tb.track_number.unwrap_or(u64::MAX), &tb.path))
            });
            let ts: Vec<&TrackInfo> = idx.iter().map(|&i| &tracks[i]).collect();
            let best = ts.iter().max_by_key(|t| (t.bits.unwrap_or(0), t.sample_rate)).unwrap();
            let mut formats: Vec<&str> = ts.iter().map(|t| t.format.as_str()).collect();
            formats.sort();
            formats.dedup();
            Album {
                id: stable_id(&key),
                title,
                artist,
                year: ts.iter().find_map(|t| t.year.clone()),
                genre: ts.iter().find_map(|t| t.genre.clone()),
                duration: ts.iter().map(|t| t.duration).sum(),
                max_rate: best.sample_rate,
                max_bits: best.bits,
                format: formats.join("/"),
                hi_res: ts.iter().any(|t| t.hi_res()),
                cover_track: ts.iter().find(|t| t.has_cover).map(|t| t.path.clone()),
                tracks: idx,
            }
        })
        .collect();
    albums.sort_by(|a, b| {
        (a.artist.to_lowercase(), a.year.clone(), a.title.to_lowercase())
            .cmp(&(b.artist.to_lowercase(), b.year.clone(), b.title.to_lowercase()))
    });
    albums
}

/// Embedded artwork of one file (front cover preferred).
pub fn read_cover(path: &Path) -> Option<Cover> {
    scan_file(path).ok()?.1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(path: &str, artist: Option<&str>, album: Option<&str>, n: Option<u64>) -> TrackInfo {
        TrackInfo {
            path: path.into(),
            title: path.into(),
            artist: artist.map(Into::into),
            album_artist: None,
            album: album.map(Into::into),
            genre: None,
            year: None,
            track_number: n,
            disc_number: None,
            duration: 100.0,
            sample_rate: 44_100,
            bits: Some(16),
            channels: 2,
            format: "FLAC".into(),
            lossless: true,
            has_cover: false,
            size: 0,
            mtime: 0,
        }
    }

    #[test]
    fn groups_by_artist_and_album_in_track_order() {
        let tracks = vec![
            t("/m/kob/02.flac", Some("Miles Davis"), Some("Kind of Blue"), Some(2)),
            t("/m/kob/01.flac", Some("Miles Davis"), Some("Kind of Blue"), Some(1)),
            t("/m/other/01.flac", Some("Bill Evans"), Some("Kind of Blue"), Some(1)),
        ];
        let albums = group_albums(&tracks);
        assert_eq!(albums.len(), 2);
        let kob = albums.iter().find(|a| a.artist == "Miles Davis").unwrap();
        assert_eq!(kob.tracks, vec![1, 0]);
        assert_eq!(kob.duration, 200.0);
    }

    #[test]
    fn untagged_files_group_by_folder() {
        let tracks = vec![t("/m/Loose Rip/a.wav", None, None, None), t("/m/Loose Rip/b.wav", None, None, None)];
        let albums = group_albums(&tracks);
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].title, "Loose Rip");
        assert_eq!(albums[0].artist, UNKNOWN_ARTIST);
    }

    #[test]
    fn album_ids_are_stable_and_case_insensitive() {
        let a = group_albums(&[t("/x/1.flac", Some("Miles Davis"), Some("Kind of Blue"), None)]);
        let b = group_albums(&[t("/y/1.flac", Some("miles davis"), Some("KIND OF BLUE"), None)]);
        assert_eq!(a[0].id, b[0].id);
    }

    #[test]
    fn reads_alac_bit_depth_from_cookie() {
        let bare = [0, 0, 16, 0, 0, 24, 40, 10, 14, 2];
        assert_eq!(alac_bit_depth(&bare), Some(24));
        let mut wrapped = vec![0, 0, 0, 36];
        wrapped.extend_from_slice(b"alac");
        wrapped.extend_from_slice(&[0, 0, 0, 0]);
        wrapped.extend_from_slice(&[0, 0, 16, 0, 0, 16, 40, 10, 14, 2]);
        assert_eq!(alac_bit_depth(&wrapped), Some(16));
        assert_eq!(alac_bit_depth(&[1, 2]), None);
    }

    #[test]
    fn an_image_with_a_sheet_becomes_its_tracks() {
        let dir = std::env::temp_dir().join(format!("bitspring-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::decode::tests::ramp_wav(&dir.join("Image_CD.wav"), 44_100, 44_100 * 10);
        std::fs::write(dir.join("loose.txt"), "").unwrap();
        let sheet = "PERFORMER \"Band\"\nTITLE \"Record\"\nREM DATE 2001\nFILE \"Image_CD.wav\" WAVE\n TRACK 01 AUDIO\n  TITLE \"One\"\n  INDEX 01 00:00:00\n TRACK 02 AUDIO\n  TITLE \"Two\"\n  PERFORMER \"Guest\"\n  INDEX 00 00:03:00\n  INDEX 01 00:04:00\n";
        std::fs::write(dir.join("Cue.cue"), sheet).unwrap();
        std::fs::write(dir.join("Cue_RepG.cue"), sheet).unwrap();
        let r = scan(std::slice::from_ref(&dir), &[], |_, _| {}, |_, _| {});
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        let ts = &r.tracks;
        assert_eq!(ts.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(), ["One", "Two"]);
        assert_eq!((ts[0].duration, ts[1].duration), (4.0, 6.0));
        assert_eq!(ts[1].artist.as_deref(), Some("Guest"));
        assert_eq!(ts[1].album_artist.as_deref(), Some("Band"));
        assert_eq!((ts[1].album.as_deref(), ts[1].year.as_deref(), ts[1].track_number), (Some("Record"), Some("2001"), Some(2)));
        assert_eq!((ts[0].sample_rate, ts[0].format.as_str()), (44_100, "WAV"));
        let albums = group_albums(ts);
        assert_eq!(albums.len(), 1);
        assert_eq!((albums[0].title.as_str(), albums[0].artist.as_str(), albums[0].duration), ("Record", "Band", 10.0));
        // A track plays as its part of the image.
        let t = crate::open_file(Path::new(&ts[1].path), crate::dsd::DsdMode::Pcm).unwrap();
        assert!(t.wait_for(usize::MAX, std::time::Duration::from_secs(10)));
        assert_eq!(t.frames(), 6 * 44_100);
        assert_eq!(t.meta.title.as_deref(), Some("Two"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hi_res_needs_lossless_and_more_than_cd() {
        let mut x = t("/a.flac", None, None, None);
        assert!(!x.hi_res());
        x.bits = Some(24);
        assert!(x.hi_res());
        x.lossless = false;
        assert!(!x.hi_res());
    }
}

/// Perceptual codecs: they discard audio, and their decoded peaks can overshoot full scale.
pub fn codec_is_lossy(codec: AudioCodecId) -> bool {
    [CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3, CODEC_ID_AAC, CODEC_ID_VORBIS, CODEC_ID_OPUS].contains(&codec)
}
