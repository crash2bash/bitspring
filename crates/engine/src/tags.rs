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
    if crate::source_file(path) != path {
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
    rewrite(path, &Change::Fields(&|tag| apply_to(tag, edit)), &|_| Ok(()))
}

/// ReplayGain values to keep in a file's tags, so every player levels it the same way.
/// Gains are towards −18 LUFS (ReplayGain 2.0), peaks linear with 1.0 = full scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GainTags {
    pub track_db: f64,
    pub track_peak: f64,
    pub album_db: f64,
    pub album_peak: f64,
}

/// Files we write ReplayGain tags into: those whose tags we edit, less Opus (its gain lives
/// in R128 tags and the stream header, which we leave to its own tools) and less WAV and
/// AIFF (the player doesn't read such tags from their chunks).
pub fn can_write_gain(path: &str) -> bool {
    let ext = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    can_edit(path) && !matches!(ext.as_str(), "opus" | "wav" | "aif" | "aiff") && !crate::lossless::is_lossless_extra(Path::new(path))
}

/// The ReplayGain / R128 values in a file's tags, as the player reads them when it opens it.
pub fn read_gain(path: &Path) -> Result<crate::dsp::loudness::TagGain> {
    let mss = MediaSourceStream::new(Box::new(File::open(path)?), Default::default());
    let mut hint = Hint::new();
    if let Some(e) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(e);
    }
    let mut format = symphonia::default::get_probe().probe(&hint, mss, FormatOptions::default(), MetadataOptions::default()).context("unreadable audio")?;
    Ok(crate::decode::read_meta(&mut *format).gain)
}

/// Store `gain` in the tags of the file at `path`, as safely as any tag edit — and only if
/// the player then reads the same values back: a file it can't is left as it was.
pub fn write_gain(path: &Path, gain: &GainTags) -> Result<()> {
    if !can_write_gain(&path.to_string_lossy()) {
        bail!("loudness can't be stored in the tags of this kind of file");
    }
    let db = |v: f64| format!("{v:.2} dB");
    let peak = |v: f64| format!("{v:.6}");
    let values = [
        ("REPLAYGAIN_TRACK_GAIN", db(gain.track_db)),
        ("REPLAYGAIN_TRACK_PEAK", peak(gain.track_peak)),
        ("REPLAYGAIN_ALBUM_GAIN", db(gain.album_db)),
        ("REPLAYGAIN_ALBUM_PEAK", peak(gain.album_peak)),
    ];
    let check = |copy: &Path| {
        let read = read_gain(copy)?;
        let same = |a: Option<f64>, b: f64, step: f64| a.is_some_and(|a| (a - b).abs() <= step);
        let ok = same(read.track_db, gain.track_db, 0.006)
            && same(read.track_peak, gain.track_peak, 1e-6)
            && same(read.album_db, gain.album_db, 0.006)
            && same(read.album_peak, gain.album_peak, 1e-6);
        if !ok {
            bail!("the loudness tags didn't read back from this file; it was left as it was");
        }
        Ok(())
    };
    rewrite(path, &Change::Gain(&values), &check)
}

/// What a write does to a file's tags.
enum Change<'a> {
    /// Edit the fields the tag editor shows, on every tag the file has.
    Fields(&'a dyn Fn(&mut Tag) -> Result<()>),
    /// Set these four ReplayGain fields (name, value) and touch nothing else: this runs over
    /// whole libraries, so not even the spelling of another field's name may change.
    Gain(&'a [(&'static str, String); 4]),
}

/// A tag in its own form that can hold the ReplayGain fields. (The kinds that can't — ID3v1,
/// RIFF INFO, AIFF text — keep the default: nothing.)
trait GainFields {
    fn set_gain(&mut self, _values: &[(&'static str, String); 4]) {}
}

impl GainFields for lofty::ogg::tag::VorbisComments {
    fn set_gain(&mut self, values: &[(&'static str, String); 4]) {
        for (name, value) in values {
            // (Names of Vorbis comments don't tell upper case from lower.)
            self.remove(name).for_each(drop);
            self.push(name.to_string(), value.clone());
        }
    }
}

impl GainFields for lofty::id3::v2::Id3v2Tag {
    fn set_gain(&mut self, values: &[(&'static str, String); 4]) {
        use lofty::id3::v2::Frame;
        for (name, value) in values {
            self.retain(|f| !matches!(f, Frame::UserText(t) if t.description.eq_ignore_ascii_case(name)));
            self.insert_user_text(name.to_string(), value.clone());
        }
    }
}

impl GainFields for lofty::mp4::Ilst {
    fn set_gain(&mut self, values: &[(&'static str, String); 4]) {
        use lofty::mp4::{Atom, AtomData, AtomIdent};
        const MEAN: &str = "com.apple.iTunes";
        for (name, value) in values {
            self.retain(|a| !matches!(a.ident(), AtomIdent::Freeform { mean, name: have } if mean == MEAN && have.eq_ignore_ascii_case(name)));
            // Lower case, as other taggers write them in MP4 files.
            let ident = AtomIdent::Freeform { mean: MEAN.into(), name: name.to_ascii_lowercase().into() };
            self.insert(Atom::new(ident, AtomData::UTF8(value.clone())));
        }
    }
}

impl GainFields for lofty::id3::v1::Id3v1Tag {}
impl GainFields for lofty::ape::ApeTag {}
impl GainFields for lofty::iff::wav::RiffInfoList {}
impl GainFields for lofty::iff::aiff::AiffTextChunks {}

/// Change the tags of the file at `path`, the safe way: in a copy that takes the original's
/// place only once its audio is found identical and `check` accepts it.
fn rewrite(path: &Path, change: &Change, check: &dyn Fn(&Path) -> Result<()>) -> Result<()> {
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
        apply(&mut copy, change)?;
        verify(path, &tmp)?;
        check(&tmp)?;
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

/// Make `change` to every tag the file in `handle` has (a WAV may carry RIFF INFO and ID3:
/// other players read either), making the kind the format usually carries if it has none,
/// and write the result back.
///
/// Each tag is read in its own form, and an edit lifts out of it only the fields the editor
/// knows: the ones the library has no name for (a UPC, a web address, the credits) stay
/// where they are. Reading a file's tags in the library's common form instead drops them.
fn apply(handle: &mut File, change: &Change) -> Result<()> {
    use lofty::config::ParseOptions;
    use lofty::file::{AudioFile, FileType};
    use lofty::tag::{MergeTag, SplitTag};
    use std::io::{Seek, SeekFrom};

    fn edited<T>(mut tag: T, change: &Change) -> Result<T>
    where
        T: SplitTag + GainFields,
        T::Remainder: MergeTag<Merged = T>,
    {
        match change {
            Change::Fields(edit) => {
                let (rest, mut common) = tag.split_tag();
                edit(&mut common)?;
                // Two spellings of one field ("ALBUM ARTIST" and "ALBUMARTIST") come out
                // under one name: once is enough when they say the same.
                let mut seen: Vec<(ItemKey, lofty::tag::ItemValue)> = Vec::new();
                common.retain(|item| {
                    let this = (item.key(), item.value().clone());
                    let new = !seen.contains(&this);
                    if new {
                        seen.push(this);
                    }
                    new
                });
                Ok(rest.merge_tag(common))
            }
            Change::Gain(values) => {
                tag.set_gain(values);
                Ok(tag)
            }
        }
    }
    // An ID3v2.3 tag stays version 2.3 (older players and Windows read no other).
    fn options(id3: Option<&lofty::id3::v2::Id3v2Tag>) -> WriteOptions {
        let old = id3.is_some_and(|t| t.original_version() != lofty::id3::v2::Id3v2Version::V4);
        WriteOptions::default().use_id3v23(old)
    }

    handle.seek(SeekFrom::Start(0))?;
    let kind = lofty::probe::Probe::new(std::io::BufReader::new(&mut *handle))
        .guess_file_type()
        .context("couldn't read the file")?
        .file_type()
        .ok_or_else(|| anyhow!("couldn't tell what kind of file this is"))?;
    handle.seek(SeekFrom::Start(0))?;
    // For the loudness values the tags are taken as they are written: no tidying on the way
    // in ("09" stays "09", "9/12" stays one field).
    let parse = ParseOptions::new().implicit_conversions(!matches!(change, Change::Gain(_)));
    // `$first`: the tag made when the file has none; `$more`: the others, edited if there.
    macro_rules! through {
        ($file:ty, $options:expr, $first:ident / $set_first:ident $(, $more:ident / $set_more:ident)*) => {{
            let mut file = <$file>::read_from(handle, parse).context("couldn't read the file's tags")?;
            let options: fn(&$file) -> WriteOptions = $options;
            let options = options(&file);
            let first = file.$first().unwrap_or_default();
            file.$set_first(edited(first, change)?);
            $(
                if let Some(tag) = file.$more() {
                    file.$set_more(edited(tag, change)?);
                }
            )*
            handle.seek(SeekFrom::Start(0))?;
            file.save_to(handle, options).context("couldn't write the tags")?;
        }};
    }
    // Ogg files always have their comment header.
    macro_rules! through_ogg {
        ($file:ty) => {{
            let mut file = <$file>::read_from(handle, parse).context("couldn't read the file's tags")?;
            let tag = file.remove_vorbis_comments();
            file.set_vorbis_comments(edited(tag, change)?);
            handle.seek(SeekFrom::Start(0))?;
            file.save_to(handle, WriteOptions::default()).context("couldn't write the tags")?;
        }};
    }
    match kind {
        FileType::Flac => through!(lofty::flac::FlacFile, |_| WriteOptions::default(), remove_vorbis_comments / set_vorbis_comments),
        FileType::Mpeg => through!(lofty::mpeg::MpegFile, |f| options(f.id3v2()), remove_id3v2 / set_id3v2, remove_id3v1 / set_id3v1, remove_ape / set_ape),
        FileType::Mp4 => through!(lofty::mp4::Mp4File, |_| WriteOptions::default(), remove_ilst / set_ilst),
        FileType::Aac => through!(lofty::aac::AacFile, |f| options(f.id3v2()), remove_id3v2 / set_id3v2, remove_id3v1 / set_id3v1),
        FileType::Wav => through!(lofty::iff::wav::WavFile, |f| options(f.id3v2()), remove_id3v2 / set_id3v2, remove_riff_info / set_riff_info),
        FileType::Aiff => through!(lofty::iff::aiff::AiffFile, |f| options(f.id3v2()), remove_id3v2 / set_id3v2, remove_text_chunks / set_text_chunks),
        FileType::Ape => through!(lofty::ape::ApeFile, |f| options(f.id3v2()), remove_ape / set_ape, remove_id3v1 / set_id3v1, remove_id3v2 / set_id3v2),
        FileType::WavPack => through!(lofty::wavpack::WavPackFile, |_| WriteOptions::default(), remove_ape / set_ape, remove_id3v1 / set_id3v1),
        FileType::Vorbis => through_ogg!(lofty::ogg::VorbisFile),
        FileType::Opus => through_ogg!(lofty::ogg::OpusFile),
        FileType::Speex => through_ogg!(lofty::ogg::SpeexFile),
        _ => bail!("tags of this kind of file can't be edited"),
    }
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

    /// ReplayGain tags, as a scanner (foobar2000, rsgain) leaves them, reach the track.
    #[test]
    fn replaygain_tags_are_read_for_levelling() {
        let d = tmpdir("gain");
        let p = flac_file(&d);
        assert!(crate::decode::open_track(&p).unwrap().meta.gain.is_empty());
        let mut file = lofty::read_from_path(&p).unwrap();
        file.insert_tag(Tag::new(file.primary_tag_type()));
        let tag = file.primary_tag_mut().unwrap();
        tag.insert_text(ItemKey::ReplayGainTrackGain, "-7.25 dB".into());
        tag.insert_text(ItemKey::ReplayGainTrackPeak, "0.988553".into());
        tag.insert_text(ItemKey::ReplayGainAlbumGain, "-6.10 dB".into());
        tag.insert_text(ItemKey::ReplayGainAlbumPeak, "1.000000".into());
        file.save_to_path(&p, lofty::config::WriteOptions::default()).unwrap();
        let gain = crate::decode::open_track(&p).unwrap().meta.gain;
        assert_eq!((gain.track_db, gain.track_peak), (Some(-7.25), Some(0.988553)));
        assert_eq!((gain.album_db, gain.album_peak), (Some(-6.1), Some(1.0)));
        // A CUE track of that file: the file's values describe the whole image.
        let image = gain.of_image();
        assert_eq!((image.track_db, image.album_db, image.album_peak), (None, Some(-6.1), Some(1.0)));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Measured loudness goes into the tags the way a scanner would leave it, the audio
    /// stays as it was, and the player reads the values back.
    #[test]
    fn loudness_tags_are_written_and_read_back() {
        let d = tmpdir("gainwrite");
        let p = flac_file(&d);
        write(&p, &TagEdit { title: Some("Hands Open".into()), ..Default::default() }).unwrap();
        let before = audio_fingerprint(&p).unwrap();
        let gain = GainTags { track_db: -7.254, track_peak: 1.023_456_7, album_db: -6.1, album_peak: 1.023_456_7 };
        write_gain(&p, &gain).unwrap();
        assert_eq!(audio_fingerprint(&p).unwrap(), before);
        let read = read_gain(&p).unwrap();
        assert_eq!((read.track_db, read.track_peak), (Some(-7.25), Some(1.023457)));
        assert_eq!((read.album_db, read.album_peak), (Some(-6.1), Some(1.023457)));
        // What a track opened for playing gets, and the other tags are still there.
        assert_eq!(crate::decode::open_track(&p).unwrap().meta.gain, read);
        assert_eq!(crate::library::scan_file(&p).unwrap().0.title, "Hands Open");
        // Written again (the file was measured anew): the values are replaced, not doubled.
        write_gain(&p, &GainTags { track_db: -3.0, ..gain }).unwrap();
        assert_eq!(read_gain(&p).unwrap().track_db, Some(-3.0));
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);
        // Where the values wouldn't serve: refused before anything is touched.
        assert!(can_write_gain("/m/a.flac") && can_write_gain("/m/a.mp3") && can_write_gain("/m/a.m4a"));
        assert!(!can_write_gain("/m/a.opus") && !can_write_gain("/m/a.wav") && !can_write_gain("/m/a.dsf") && !can_write_gain("/m/a.ape"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A file's tags hold more than the editor shows — a barcode, a web address, the credits,
    /// whatever its shop or ripper put there. Neither an edit nor the loudness values may
    /// cost the file any of it.
    #[test]
    fn fields_the_editor_does_not_know_stay_in_the_file() {
        use lofty::file::AudioFile;
        let d = tmpdir("keep");
        let p = flac_file(&d);
        let other = [
            ("UPC", "0075679659873"),
            ("URL", "https://example.com/album"),
            ("INVOLVEDPEOPLE", "Jeremy Lister, Backing Vocal"),
            ("ITUNESADVISORY", "0"),
            ("MEDIATYPE", "album"),
            ("MY OWN FIELD", "kept"),
            ("COMMENT", "first"),
            ("COMMENT", "second"),
        ];
        let read = |p: &Path| lofty::flac::FlacFile::read_from(&mut File::open(p).unwrap(), lofty::config::ParseOptions::new()).unwrap();
        let mut file = read(&p);
        let mut tag = lofty::ogg::tag::VorbisComments::default();
        tag.push("TITLE".into(), "Back Door".into());
        for (k, v) in other {
            tag.push(k.into(), v.into());
        }
        file.set_vorbis_comments(tag);
        file.save_to_path(&p, WriteOptions::default()).unwrap();
        let before = audio_fingerprint(&p).unwrap();

        write(&p, &TagEdit { title: Some("Front Door".into()), year: Some("2025".into()), ..Default::default() }).unwrap();
        write_gain(&p, &GainTags { track_db: -7.25, track_peak: 0.98, album_db: -6.1, album_peak: 1.0 }).unwrap();

        let after = read(&p);
        let tag = after.vorbis_comments().unwrap();
        for (k, v) in other {
            assert!(tag.get_all(k).any(|have| have == v), "{k}={v} was lost; the file has {:?}", tag.items().collect::<Vec<_>>());
        }
        assert_eq!(tag.get("TITLE"), Some("Front Door"));
        assert_eq!(tag.get("REPLAYGAIN_TRACK_GAIN"), Some("-7.25 dB"));
        assert_eq!(tag.items().len(), other.len() + 1 + 1 + 4, "{:?}", tag.items().collect::<Vec<_>>());
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

    /// Only the loudness values, into every file in $BITSPRING_TAG_SAMPLES that takes them
    /// (to compare the rest of their tags before and after with another tool).
    #[test]
    #[ignore]
    fn loudness_only() {
        let dir = std::env::var("BITSPRING_TAG_SAMPLES").expect("BITSPRING_TAG_SAMPLES");
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if !can_write_gain(&p.to_string_lossy()) {
                continue;
            }
            let before = audio_fingerprint(&p).unwrap();
            write_gain(&p, &GainTags { track_db: -7.25, track_peak: 0.988553, album_db: -6.1, album_peak: 1.0 }).unwrap_or_else(|e| panic!("{}: {e:#}", p.display()));
            assert_eq!(audio_fingerprint(&p).unwrap(), before, "{}", p.display());
            eprintln!("ok {}", p.display());
        }
    }

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
            if can_write_gain(&p.to_string_lossy()) {
                let gain = GainTags { track_db: -7.25, track_peak: 0.988553, album_db: -6.1, album_peak: 1.0 };
                write_gain(&p, &gain).unwrap_or_else(|e| panic!("{}: {e:#}", p.display()));
                assert_eq!(audio_fingerprint(&p).unwrap(), before, "{}", p.display());
                let read = crate::decode::open_track(&p).unwrap().meta.gain;
                assert_eq!((read.track_db, read.track_peak, read.album_db, read.album_peak), (Some(-7.25), Some(0.988553), Some(-6.1), Some(1.0)), "{}", p.display());
                assert_eq!(crate::library::scan_file(&p).unwrap().0.title, "Новое название", "{}", p.display());
            }
            eprintln!("ok {}", p.display());
        }
    }
}
