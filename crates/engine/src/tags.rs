//! Writing tags (the tag editor).
//!
//! A music file is the user's own copy of something they may not be able to get again, so a
//! write never touches it directly: the new tags go into a copy next to it, the copy is
//! checked — every compressed audio frame identical to the original's, the format the same,
//! the new tags readable — and only then does the copy take the original's place, in one
//! rename. Anything that goes wrong leaves the original as it was.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use lofty::config::WriteOptions;
use lofty::prelude::*;
use lofty::tag::Tag;
use serde::{Deserialize, Serialize};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// What to change. `None`: leave as it is; `Some("")`: remove; otherwise set.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct TagEdit {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub year: Option<String>,
    pub track_number: Option<String>,
    pub disc_number: Option<String>,
}

impl TagEdit {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Formats whose tags we write. Not DSD (DSF/DFF tags are rare and lofty doesn't write
/// them) and not the tracks of a CUE image (their names live in the sheet).
pub fn can_edit(path: &str) -> bool {
    if crate::cue::split(path).1.is_some() {
        return false;
    }
    let ext = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(), "flac" | "mp3" | "m4a" | "mp4" | "alac" | "aac" | "wav" | "aif" | "aiff" | "ogg" | "opus" | "ape" | "wv")
}

/// WAV and AIFF keep their tags in chunks the decoder doesn't read (RIFF INFO, ID3): fill
/// what `meta` lacks from them.
pub(crate) fn fill_from_file(path: &Path, meta: &mut crate::TrackMeta) {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if !matches!(ext.as_str(), "wav" | "aif" | "aiff") || meta.title.is_some() {
        return;
    }
    let Ok(file) = lofty::read_from_path(path) else { return };
    let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) else { return };
    let text = |k: ItemKey| tag.get_string(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    meta.title = meta.title.take().or_else(|| tag.title().map(|s| s.to_string()));
    meta.artist = meta.artist.take().or_else(|| tag.artist().map(|s| s.to_string()));
    meta.album = meta.album.take().or_else(|| tag.album().map(|s| s.to_string()));
    meta.genre = meta.genre.take().or_else(|| tag.genre().map(|s| s.to_string()));
    meta.album_artist = meta.album_artist.take().or_else(|| text(ItemKey::AlbumArtist));
    meta.year = meta.year.take().or_else(|| text(ItemKey::RecordingDate).or_else(|| text(ItemKey::Year)).map(|y| y.chars().take(4).collect()));
    meta.track_number = meta.track_number.or_else(|| tag.track().map(u64::from));
    meta.disc_number = meta.disc_number.or_else(|| tag.disk().map(u64::from));
}

/// Apply `edit` to the file at `path`, safely (see the module notes).
pub fn write(path: &Path, edit: &TagEdit) -> Result<()> {
    if edit.is_empty() {
        return Ok(());
    }
    if !can_edit(&path.to_string_lossy()) {
        bail!("tags of this kind of file can't be edited");
    }
    let name = path.file_name().ok_or_else(|| anyhow!("not a file"))?.to_string_lossy().into_owned();
    // Hidden, with a random part, and the original's extension (the decoder that checks it
    // goes by the extension). Created new or not at all: whatever already has the name (a
    // link planted there) is never followed or written through.
    let tmp: PathBuf = path.with_file_name(format!(".bitspring-{}-{name}", &crate::net::http::random_hex()[..12]));
    let original_permissions = std::fs::metadata(path).context("couldn't read the file")?.permissions();
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    // Only the user may read the copy until it takes the original's place (and permissions).
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut copy = options.open(&tmp).context("couldn't make a copy to write the tags into")?;
    let result = (|| {
        std::io::copy(&mut File::open(path)?, &mut copy).context("couldn't copy the file to write its tags")?;
        apply(&mut copy, edit)?;
        verify(path, &tmp)?;
        // The original's permissions; then the copy takes its place in one step.
        std::fs::set_permissions(&tmp, original_permissions).context("couldn't give the copy the file's permissions")?;
        std::fs::rename(&tmp, path).context("couldn't replace the file")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn apply(handle: &mut File, edit: &TagEdit) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    handle.seek(SeekFrom::Start(0))?;
    let mut file = lofty::read_from(handle).context("couldn't read the file's tags")?;
    if file.primary_tag_mut().is_none() {
        let kind = file.primary_tag_type();
        file.insert_tag(Tag::new(kind));
    }
    // Every tag the file has (a WAV may carry RIFF INFO and ID3): other players read either.
    let kinds: Vec<_> = file.tags().iter().map(|t| t.tag_type()).collect();
    for kind in kinds {
        if let Some(tag) = file.tag_mut(kind) {
            apply_to(tag, edit)?;
        }
    }
    handle.seek(SeekFrom::Start(0))?;
    file.save_to(handle, WriteOptions::default()).context("couldn't write the tags")?;
    handle.sync_all()?;
    Ok(())
}

fn apply_to(tag: &mut Tag, edit: &TagEdit) -> Result<()> {
    fn text(v: &Option<String>) -> Option<Option<String>> {
        v.as_ref().map(|s| Some(s.trim().to_string()).filter(|s| !s.is_empty()))
    }
    if let Some(v) = text(&edit.title) {
        match v {
            Some(v) => tag.set_title(v),
            None => tag.remove_title(),
        }
    }
    if let Some(v) = text(&edit.artist) {
        match v {
            Some(v) => tag.set_artist(v),
            None => tag.remove_artist(),
        }
    }
    if let Some(v) = text(&edit.album) {
        match v {
            Some(v) => tag.set_album(v),
            None => tag.remove_album(),
        }
    }
    if let Some(v) = text(&edit.genre) {
        match v {
            Some(v) => tag.set_genre(v),
            None => tag.remove_genre(),
        }
    }
    if let Some(v) = text(&edit.album_artist) {
        tag.remove_key(ItemKey::AlbumArtist);
        if let Some(v) = v {
            tag.insert_text(ItemKey::AlbumArtist, v);
        }
    }
    if let Some(v) = text(&edit.year) {
        tag.remove_key(ItemKey::RecordingDate);
        tag.remove_key(ItemKey::Year);
        if let Some(v) = v {
            tag.insert_text(ItemKey::RecordingDate, v);
        }
    }
    let number = |v: &str, what: &str| -> Result<u32> {
        // "3" or "3/12"
        v.split('/').next().unwrap_or("").trim().parse().map_err(|_| anyhow!("{what} must be a number: {v:?}"))
    };
    if let Some(v) = text(&edit.track_number) {
        match v {
            Some(v) => tag.set_track(number(&v, "track number")?),
            None => tag.remove_track(),
        }
    }
    if let Some(v) = text(&edit.disc_number) {
        match v {
            Some(v) => tag.set_disk(number(&v, "disc number")?),
            None => tag.remove_disk(),
        }
    }
    Ok(())
}

/// The copy carries exactly the original's audio: the same codec setup, and every packet
/// byte for byte (the compressed frames, so this costs no decoding).
fn verify(original: &Path, copy: &Path) -> Result<()> {
    let (a, b) = (audio_fingerprint(original)?, audio_fingerprint(copy)?);
    if a != b {
        bail!("writing the tags would have changed the audio; the file was left as it was");
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
struct Fingerprint {
    rate: Option<u32>,
    channels: Option<usize>,
    bits: Option<u32>,
    packets: u64,
    bytes: u64,
    hash: u64,
}

fn audio_fingerprint(path: &Path) -> Result<Fingerprint> {
    use std::hash::Hasher;
    let mss = MediaSourceStream::new(Box::new(File::open(path)?), Default::default());
    let mut hint = Hint::new();
    if let Some(e) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(e);
    }
    let mut format = symphonia::default::get_probe().probe(&hint, mss, FormatOptions::default(), MetadataOptions::default()).context("unreadable audio")?;
    let track = format.default_track(TrackType::Audio).ok_or_else(|| anyhow!("no audio track"))?;
    let id = track.id;
    let audio = track.codec_params.as_ref().and_then(|p| p.audio());
    let mut fp = Fingerprint {
        rate: audio.and_then(|a| a.sample_rate),
        channels: audio.and_then(|a| a.channels.as_ref().map(|c| c.count())),
        bits: audio.and_then(|a| a.bits_per_sample),
        packets: 0,
        bytes: 0,
        hash: 0,
    };
    // FNV-1a over every packet's bytes, in order.
    let mut h = Fnv(0xcbf2_9ce4_8422_2325);
    while let Ok(Some(p)) = format.next_packet() {
        if p.track_id != id {
            continue;
        }
        h.write(&p.data);
        fp.packets += 1;
        fp.bytes += p.data.len() as u64;
    }
    fp.hash = h.finish();
    Ok(fp)
}

struct Fnv(u64);

impl std::hash::Hasher for Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= *b as u64;
            self.0 = self.0.wrapping_mul(0x100_0000_01b3);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small real FLAC file made by our own encoder.
    fn flac_file(dir: &Path) -> PathBuf {
        use crate::net::encode::{Codec, Encoder, Format};
        let mut enc = Encoder::new(Format { codec: Codec::Flac, rate: 44_100, channels: 2, bits: 16 });
        let mut bytes = enc.header();
        let samples: Vec<i32> = (0..44_100 * 2).map(|i| ((i * 37) % 2000) - 1000).collect();
        for block in samples.chunks(4096 * 2) {
            enc.encode(block, &mut bytes);
        }
        let p = dir.join("01 test.flac");
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bitspring-tags-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn tags_change_and_the_audio_does_not() {
        let d = tmpdir("write");
        let p = flac_file(&d);
        let before = audio_fingerprint(&p).unwrap();
        let edit = TagEdit {
            title: Some("Hands Open".into()),
            artist: Some("Snow Patrol".into()),
            album: Some("Eyes Open".into()),
            album_artist: Some("Snow Patrol".into()),
            genre: Some("Rock".into()),
            year: Some("2006".into()),
            track_number: Some("2/12".into()),
            disc_number: Some("1".into()),
        };
        write(&p, &edit).unwrap();
        assert_eq!(audio_fingerprint(&p).unwrap(), before);
        let (info, _) = crate::library::scan_file(&p).unwrap();
        assert_eq!(info.title, "Hands Open");
        assert_eq!(info.artist.as_deref(), Some("Snow Patrol"));
        assert_eq!(info.album.as_deref(), Some("Eyes Open"));
        assert_eq!(info.album_artist.as_deref(), Some("Snow Patrol"));
        assert_eq!(info.genre.as_deref(), Some("Rock"));
        assert_eq!(info.year.as_deref(), Some("2006"));
        assert_eq!(info.track_number, Some(2));
        assert_eq!(info.disc_number, Some(1));
        // No copy left behind.
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);

        // Removing and leaving alone.
        write(&p, &TagEdit { genre: Some(String::new()), title: Some("Chasing Cars".into()), ..Default::default() }).unwrap();
        let (info, _) = crate::library::scan_file(&p).unwrap();
        assert_eq!((info.title.as_str(), info.genre), ("Chasing Cars", None));
        assert_eq!(info.album.as_deref(), Some("Eyes Open"));
        assert_eq!(audio_fingerprint(&p).unwrap(), before);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_failed_write_leaves_the_file_alone() {
        let d = tmpdir("fail");
        let p = flac_file(&d);
        let bytes = std::fs::read(&p).unwrap();
        let err = write(&p, &TagEdit { track_number: Some("two".into()), ..Default::default() }).unwrap_err();
        assert!(format!("{err:#}").contains("track number"), "{err:#}");
        assert_eq!(std::fs::read(&p).unwrap(), bytes);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_keeps_its_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmpdir("perm");
        let p = flac_file(&d);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o640)).unwrap();
        write(&p, &TagEdit { title: Some("X".into()), ..Default::default() }).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o640);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn only_files_we_can_write_are_editable() {
        assert!(can_edit("/m/a.flac"));
        assert!(can_edit("/m/a.M4A"));
        assert!(!can_edit("/m/a.dsf"));
    }
}

#[cfg(test)]
mod formats {
    use super::*;

    /// Files in $BITSPRING_TAG_SAMPLES (made with ffmpeg in every format we write): tags change,
    /// audio doesn't. Ignored by default (needs the samples).
    #[test]
    #[ignore]
    fn every_format_round_trips() {
        let dir = std::env::var("BITSPRING_TAG_SAMPLES").expect("BITSPRING_TAG_SAMPLES");
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let before = audio_fingerprint(&p).unwrap();
            let edit = TagEdit { title: Some("Новое название".into()), album: Some("Album".into()), year: Some("1999".into()), track_number: Some("4".into()), ..Default::default() };
            write(&p, &edit).unwrap_or_else(|e| panic!("{}: {e:#}", p.display()));
            assert_eq!(audio_fingerprint(&p).unwrap(), before, "{}", p.display());
            let (info, _) = crate::library::scan_file(&p).unwrap();
            assert_eq!(info.title, "Новое название", "{}", p.display());
            assert_eq!(info.album.as_deref(), Some("Album"), "{}", p.display());
            assert_eq!(info.track_number, Some(4), "{}", p.display());
            assert_eq!(info.year.as_deref(), Some("1999"), "{}", p.display());
            eprintln!("ok {}", p.display());
        }
    }
}
