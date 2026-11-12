//! Super Audio CD disc images (.iso), read with the scarletbook crate (DST decoded there).
//!
//! An image is an album: its stereo area's tracks show up in the library as
//! `<image>#sacd:<n>`, and play through the same DSD path as DSF/DFF files (DoP to a DAC
//! that takes it, else converted to PCM). Discs without a stereo area offer their
//! multichannel one instead.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use scarletbook::{Area, Disc, Slot};

use crate::decode::TrackMeta;
use crate::library::TrackInfo;

pub const MARK: &str = "#sacd:";

/// DSD bytes per channel in one SACD frame (1/75 s at DSD64).
pub const FRAME_BYTES: u64 = 4704;

pub fn is_iso(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("iso"))
}

/// `<image>#sacd:<n>` → (image, track number).
pub fn split(path: &str) -> (&str, Option<u8>) {
    match path.rfind(MARK) {
        Some(at) => match path[at + MARK.len()..].parse() {
            Ok(n) => (&path[..at], Some(n)),
            Err(_) => (path, None),
        },
        None => (path, None),
    }
}

pub fn track_path(image: &Path, number: u8) -> String {
    format!("{}{MARK}{number}", image.to_string_lossy())
}

fn open(image: &str) -> Result<Disc<BufReader<File>>> {
    let f = File::open(image).with_context(|| format!("open {image}"))?;
    Disc::open(BufReader::new(f)).map_err(|e| anyhow!("SACD image: {e}"))
}

/// The area we play: stereo when the disc has one.
fn area(disc: &Disc<BufReader<File>>) -> Result<&Area> {
    let areas = &disc.toc().areas;
    areas
        .iter()
        .find(|a| a.slot == Slot::TwoChannel)
        .or_else(|| areas.first())
        .ok_or_else(|| anyhow!("SACD image without audio"))
}

/// DSD64 is the only rate SACD has.
const RATE: u32 = 2_822_400;

fn meta(disc: &Disc<BufReader<File>>, track: &scarletbook::Track) -> TrackMeta {
    let m = &disc.toc().master;
    let clean = |s: &Option<String>| s.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    TrackMeta {
        title: clean(&track.text.title),
        artist: clean(&track.text.performer).or_else(|| clean(&m.text.album_artist)),
        album: clean(&m.text.album_title).or_else(|| clean(&m.text.disc_title)),
        album_artist: clean(&m.text.album_artist).or_else(|| clean(&m.text.disc_artist)),
        genre: None,
        year: (m.date.year > 0).then(|| m.date.year.to_string()),
        track_number: Some(track.number as u64),
        disc_number: (m.album_set_size > 1).then_some(m.album_sequence_number as u64),
        cover: None,
    }
}

/// Every track of an image, for the library.
pub fn scan(image: &Path, size: u64, mtime: u64) -> Result<Vec<TrackInfo>> {
    let disc = open(&image.to_string_lossy())?;
    let area = area(&disc)?;
    Ok(area
        .tracks
        .iter()
        .map(|t| {
            let m = meta(&disc, t);
            TrackInfo {
                path: track_path(image, t.number),
                title: m.title.clone().unwrap_or_else(|| format!("Track {}", t.number)),
                artist: m.artist,
                album_artist: m.album_artist,
                album: m.album.or_else(|| image.file_stem().map(|s| s.to_string_lossy().into_owned())),
                genre: m.genre,
                year: m.year,
                track_number: m.track_number,
                disc_number: m.disc_number,
                duration: t.duration_frames as f64 / 75.0,
                sample_rate: RATE,
                bits: Some(1),
                channels: area.channel_count as usize,
                format: "SACD".into(),
                lossless: true,
                has_cover: false,
                size,
                mtime,
            }
        })
        .collect())
}

/// What a track holds, for the DSD path.
pub struct TrackHead {
    pub rate: u32,
    pub channels: usize,
    /// 1-bit samples per channel.
    pub samples: u64,
    pub meta: TrackMeta,
}

pub fn probe(path: &str) -> Result<TrackHead> {
    let (image, Some(n)) = split(path) else { return Err(anyhow!("not a SACD track")) };
    let disc = open(image)?;
    let area = area(&disc)?;
    let t = area.tracks.iter().find(|t| t.number == n).ok_or_else(|| anyhow!("SACD image has no track {n}"))?;
    let mut meta = meta(&disc, t);
    meta.cover = crate::decode::folder_cover(Path::new(image));
    Ok(TrackHead { rate: RATE, channels: area.channel_count as usize, samples: t.duration_frames as u64 * FRAME_BYTES * 8, meta })
}

/// Read a track's DSD from byte `from` of each channel (a multiple of [`FRAME_BYTES`]), as
/// per-channel runs (MSB = oldest bit). `sink` returns false to stop.
pub fn read_bitstream(path: &str, from: u64, mut sink: impl FnMut(&[Vec<u8>]) -> bool) -> Result<()> {
    let (image, Some(n)) = split(path) else { return Err(anyhow!("not a SACD track")) };
    let mut disc = open(image)?;
    let (slot, first, end, ch) = {
        let area = area(&disc)?;
        let t = area.tracks.iter().find(|t| t.number == n).ok_or_else(|| anyhow!("SACD image has no track {n}"))?;
        (area.slot, t.start_frame + (from / FRAME_BYTES) as u32, t.end_frame(), area.channel_count as usize)
    };
    if first >= end {
        return Ok(());
    }
    let mut out: Vec<Vec<u8>> = vec![Vec::with_capacity(FRAME_BYTES as usize); ch];
    for frame in disc.range_audio(slot, first, end).map_err(|e| anyhow!("SACD: {e}"))? {
        let frame = frame.map_err(|e| anyhow!("SACD: {e}"))?;
        for (c, o) in out.iter_mut().enumerate() {
            o.clear();
            o.extend(frame.data.iter().skip(c).step_by(ch));
        }
        if !sink(&out) {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A synthetic disc image, after the builder in scarletbook's own tests
    // (tests/common/mod.rs, MIT OR Apache-2.0): a master TOC, a 2-channel area TOC with
    // titled tracks, and plain (not DST) DSD frames packed into audio sectors.
    const S: usize = 2048;
    const AREA_TOC_LSN: usize = 540;
    const AREA_TOC_SECTORS: usize = 6;
    const AUDIO_LSN: usize = AREA_TOC_LSN + AREA_TOC_SECTORS;

    fn put(img: &mut [u8], at: usize, bytes: &[u8]) {
        img[at..at + bytes.len()].copy_from_slice(bytes);
    }
    fn put32(img: &mut [u8], at: usize, v: u32) {
        put(img, at, &v.to_be_bytes());
    }
    fn put16(img: &mut [u8], at: usize, v: u16) {
        put(img, at, &v.to_be_bytes());
    }
    fn tc(n: u32) -> [u8; 3] {
        [(n / 4500) as u8, (n / 75 % 60) as u8, (n % 75) as u8]
    }

    /// Plain DSD frames, one per audio sector group (each frame spans several packets).
    fn pack(frames: &[(u32, Vec<u8>)]) -> Vec<[u8; S]> {
        let mut out = Vec::new();
        for (number, data) in frames {
            let mut off = 0;
            let mut first = true;
            while off < data.len() {
                let mut sec = [0u8; S];
                // One packet per sector, up to 2047 bytes; a frame header on its first.
                let starts = usize::from(first);
                let h = 1 + 2 + starts * 3;
                let n = (data.len() - off).min(S - h).min(2047);
                sec[0] = ((starts as u8) << 2) | (1 << 5);
                put16(&mut sec, 1, (u16::from(first) << 15) | (2 << 11) | n as u16);
                let mut at = 3;
                if first {
                    put(&mut sec, at, &tc(*number));
                    at += 3;
                }
                put(&mut sec, at, &data[off..off + n]);
                off += n;
                first = false;
                out.push(sec);
            }
        }
        out
    }

    fn image(tracks: &[(u32, u32)], frames: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let audio = pack(frames);
        let playtime: u32 = tracks.iter().map(|t| t.0 + t.1).max().unwrap_or(0);
        let total = AUDIO_LSN + audio.len().max(1);
        let mut img = vec![0u8; total * S];
        let m = 510 * S;
        put(&mut img, m, b"SACDMTOC");
        img[m + 8] = 1;
        img[m + 9] = 20;
        put16(&mut img, m + 16, 1);
        put16(&mut img, m + 18, 1);
        put(&mut img, m + 24, b"CAT-1");
        put32(&mut img, m + 64, AREA_TOC_LSN as u32);
        put16(&mut img, m + 84, AREA_TOC_SECTORS as u16);
        img[m + 128] = 1;
        put(&mut img, m + 136, b"en\x02");
        let t = m + S;
        put(&mut img, t, b"SACDText");
        put16(&mut img, t + 16, 48);
        put(&mut img, t + 48, b"Synthetic\0");
        put(&mut img, m + 9 * S, b"SACD_Man");
        let master = img[m..m + 10 * S].to_vec();
        for copy in [520, 530] {
            put(&mut img, copy * S, &master);
        }
        let a = AREA_TOC_LSN * S;
        put(&mut img, a, b"TWOCHTOC");
        img[a + 8] = 1;
        img[a + 9] = 20;
        img[a + 20] = 4;
        img[a + 32] = 2;
        put(&mut img, a + 64, &tc(playtime));
        img[a + 69] = tracks.len() as u8;
        put32(&mut img, a + 72, AUDIO_LSN as u32);
        put32(&mut img, a + 76, (AUDIO_LSN + audio.len().max(1) - 1) as u32);
        img[a + 80] = 1;
        put(&mut img, a + 88, b"en\x02");
        let ttxt = a + S;
        put(&mut img, ttxt, b"SACDTTxt");
        let igl = a + 2 * S;
        put(&mut img, igl, b"SACD_IGL");
        let trl1 = a + 4 * S;
        put(&mut img, trl1, b"SACDTRL1");
        let trl2 = a + 5 * S;
        put(&mut img, trl2, b"SACDTRL2");
        let mut text_at = 600;
        for (i, (start, duration)) in tracks.iter().enumerate() {
            put32(&mut img, trl1 + 8 + 4 * i, AUDIO_LSN as u32);
            put32(&mut img, trl1 + 1028 + 4 * i, 1);
            put(&mut img, trl2 + 8 + 4 * i, &tc(*start));
            put(&mut img, trl2 + 1028 + 4 * i, &tc(*duration));
            put(&mut img, igl + 8 + 12 * i, b"XXAAA2600001");
            put(&mut img, igl + 3072 + 4 * i, &[1, 0, 0, 5]);
            put16(&mut img, ttxt + 8 + 2 * i, text_at as u16);
            put(&mut img, ttxt + text_at, &[1, 0, 0, 0, 1, 0x20]);
            put(&mut img, ttxt + text_at + 6, format!("T{}\0", i + 1).as_bytes());
            text_at += 16;
        }
        for (i, s) in audio.iter().enumerate() {
            put(&mut img, (AUDIO_LSN + i) * S, s);
        }
        img
    }

    /// Frame `n`'s DSD, interleaved: every byte says which frame, channel and position.
    fn frame(n: u32) -> Vec<u8> {
        (0..2 * FRAME_BYTES as usize).map(|i| (n as usize * 31 + (i % 2) * 101 + i / 2 * 7) as u8).collect()
    }

    fn channel(n: u32, c: usize) -> Vec<u8> {
        frame(n).into_iter().skip(c).step_by(2).collect()
    }

    #[test]
    fn a_disc_image_reads_track_by_track() {
        let frames: Vec<(u32, Vec<u8>)> = (0..10).map(|n| (n, frame(n))).collect();
        let dir = std::env::temp_dir().join(format!("bitspring-sacd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let iso = dir.join("Disc.iso");
        std::fs::write(&iso, image(&[(0, 4), (4, 6)], &frames)).unwrap();

        let tracks = scan(&iso, 1, 1).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!((tracks[0].title.as_str(), tracks[1].title.as_str()), ("T1", "T2"));
        assert!((tracks[1].duration - 6.0 / 75.0).abs() < 1e-9);
        assert_eq!(tracks[1].path, format!("{}#sacd:2", iso.display()));
        assert!(crate::dsd::is_dsd_path(Path::new(&tracks[1].path)));

        let read = |path: &str, from: u64| {
            let mut got: Vec<Vec<u8>> = vec![Vec::new(); 2];
            read_bitstream(path, from, |runs| {
                for (g, r) in got.iter_mut().zip(runs) {
                    g.extend_from_slice(r);
                }
                true
            })
            .unwrap();
            got
        };
        // Track two from its start, then from its third frame.
        let got = read(&tracks[1].path, 0);
        for c in 0..2 {
            let want: Vec<u8> = (4..10).flat_map(|n| channel(n, c)).collect();
            assert!(got[c] == want, "channel {c}");
        }
        let got = read(&tracks[1].path, 2 * FRAME_BYTES);
        let want: Vec<u8> = (6..10).flat_map(|n| channel(n, 0)).collect();
        assert!(got[0] == want);

        let head = crate::dsd::probe(Path::new(&tracks[0].path)).unwrap();
        assert_eq!((head.rate, head.channels, head.samples), (2_822_400, 2, 4 * FRAME_BYTES * 8));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn track_paths_round_trip() {
        let p = track_path(Path::new("/m/Kind of Blue #2.iso"), 3);
        assert_eq!(p, "/m/Kind of Blue #2.iso#sacd:3");
        assert_eq!(split(&p), ("/m/Kind of Blue #2.iso", Some(3)));
        assert_eq!(split("/m/a.iso"), ("/m/a.iso", None));
        assert_eq!(split("/m/a.iso#sacd:x"), ("/m/a.iso#sacd:x", None));
    }
}
