//! hifi-engine — the audio core: decoding, device control and bit-perfect output.
//!
//! UI-agnostic: the CLI, the Tauri desktop app and (later) the phone remote all drive it.

pub mod cue;
pub mod decode;
pub mod dsd;
pub mod dsp;
pub mod error;
pub mod fpu;
pub mod library;
pub mod net;
pub mod output;
pub mod player;
pub mod playlist;
pub mod quality;
pub mod rate;
pub mod ring;
pub mod store;
pub mod tags;
pub mod testsignal;

#[cfg(target_os = "macos")]
pub mod coreaudio;
#[cfg(windows)]
pub mod wasapi_out;
#[cfg(target_os = "ios")]
pub mod ios_out;
#[cfg(target_os = "android")]
pub mod android_out;

pub use error::UserError;
pub use decode::{DsdSource, Track, TrackMeta, load_track, open_source, open_stream, open_range_at, open_track, open_track_at};
pub use symphonia::core::formats::probe::Hint;
pub use symphonia::core::io::MediaSource;

use std::path::Path;
use std::sync::Arc;

/// Open any supported file for playback: DSD files in the given mode, everything else through
/// the PCM decoders. Returns as soon as the headers are read; the rest loads in the background.
pub fn open_file(path: &Path, dsd: dsd::DsdMode) -> anyhow::Result<Arc<Track>> {
    open_file_at(path, dsd, 0.0)
}

/// [`open_file`], decoding from `start_secs` on: for a seek the track in memory can't serve
/// (see [`Track::can_seek_to`]), or a restart in the middle.
pub fn open_file_at(path: &Path, dsd: dsd::DsdMode, start_secs: f64) -> anyhow::Result<Arc<Track>> {
    // A CUE track's path names its image file and its part of it.
    if let Some((file, Some(range))) = path.to_str().map(cue::split) {
        let file = Path::new(file);
        let meta = cue::track_meta(file, range).unwrap_or_default();
        return decode::open_range_at(file, Some(range), start_secs, meta);
    }
    if dsd::is_dsd_path(path) { dsd::open_dsd_at(path, dsd, start_secs) } else { decode::open_track_at(path, start_secs) }
}
