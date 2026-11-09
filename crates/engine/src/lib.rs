//! hifi-engine — the audio core: decoding, device control and bit-perfect output.
//!
//! UI-agnostic: the CLI, the Tauri desktop app and (later) the phone remote all drive it.

pub mod decode;
pub mod dsd;
pub mod dsp;
pub mod error;
pub mod library;
pub mod output;
pub mod playlist;
pub mod rate;
pub mod ring;
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
pub use decode::{DsdSource, Track, TrackMeta, load_track, open_source, open_stream, open_track};
pub use symphonia::core::formats::probe::Hint;
pub use symphonia::core::io::MediaSource;

use std::path::Path;
use std::sync::Arc;

/// Open any supported file for playback: DSD files in the given mode, everything else through
/// the PCM decoders. Returns as soon as the headers are read; the rest loads in the background.
pub fn open_file(path: &Path, dsd: dsd::DsdMode) -> anyhow::Result<Arc<Track>> {
    if dsd::is_dsd_path(path) { dsd::open_dsd(path, dsd) } else { open_track(path) }
}
