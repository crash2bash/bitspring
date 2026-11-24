//! Desktop app backend: a thin command layer over `hifi-engine`.
//!
//! The UI never touches audio directly; it calls these commands and polls `status`.
//! All playback state lives here, in Rust.

#[cfg(target_os = "android")]
mod android_media;
#[cfg(any(target_os = "ios", target_os = "macos"))]
mod apple_media;
#[cfg(windows)]
mod windows_media;
#[cfg(target_os = "linux")]
mod linux_media;
mod abx;
mod bookmarks;
mod crash;
mod device_key;
mod dsd_out;
mod eq;
mod folders;
mod library;
mod loudness;
mod lyrics;
mod marks;
#[cfg(target_os = "linux")]
mod pad;
mod playlists;
mod quality;
mod queue;
mod remote;
mod remote_api;
mod ripcheck;
mod upsampling;
/// Qobuz and TIDAL: the real thing outside the App Store, a stand-in inside it.
#[cfg_attr(all(feature = "streaming", not(target_os = "ios")), path = "stream_on.rs")]
#[cfg_attr(not(all(feature = "streaming", not(target_os = "ios"))), path = "stream_off.rs")]
mod stream;
/// Updating from GitHub Releases: the real thing in website builds, nothing in App Store ones.
#[cfg_attr(all(feature = "updater", not(target_os = "ios")), path = "update_on.rs")]
#[cfg_attr(not(all(feature = "updater", not(target_os = "ios"))), path = "update_off.rs")]
mod update;
/// The system's player (lock screen, notification, Control Center, media keys) and the remote
/// controls that come from it, per OS. `cfg(system_media)` (set in build.rs) where there is one.
#[cfg(target_os = "android")]
use android_media as media;
#[cfg(any(target_os = "ios", target_os = "macos"))]
use apple_media as media;
#[cfg(windows)]
use windows_media as media;
#[cfg(target_os = "linux")]
use linux_media as media;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::Duration;

use base64::Engine as _;
use hifi_engine::output::{self, PlayOptions, SignalPath};
use hifi_engine::player::{self as outputs, Output, Player};
use hifi_engine::dsp::crossfeed::CrossfeedLevel;
use hifi_engine::dsp::fir::ImpulseResponse;
use hifi_engine::dsp::lossy::LossySettings;
use hifi_engine::dsp::loudness::{Basis, LevelMode, LevelSettings};
use hifi_engine::dsp::upsample;
use hifi_engine::dsp::{Convolution, DspSettings, Upsample};
use hifi_engine::dsd::{self, DsdMode};
use hifi_engine::{Track, UserError, open_file_at, testsignal};
use dsd_out::DsdOutNote;
use queue::{Queue, Repeat};
use serde::Serialize;
use tauri::State;

/// What is loaded right now, so processing changes can restart it in place.
struct Current {
    track: Arc<Track>,
    path: PathBuf,
    device_uid: String,
    /// The settings key of that device (see `device_key`): whose processing plays.
    key: String,
    exclusive: bool,
    /// The user wants DSD sent as DoP on this device.
    dop: bool,
    /// Why a DSD file plays as PCM although DoP was asked for.
    dsd_note: Option<DsdNote>,
    /// Why PCM plays although DSD output was asked for (kept for a track that joins on).
    dsd_out_note: Option<DsdOutNote>,
}

/// Why a DSD file plays as PCM although DoP was asked for. Sent as a code: the UI words it
/// in the user's language.
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum DsdNote {
    /// DoP needs exclusive access to the device.
    Exclusive,
    /// Something system-wide blocks DoP (iOS: the volume is below maximum).
    Volume,
    /// The equalizer, convolution and crossfeed work on PCM.
    Dsp,
    /// Volume levelling works on PCM: DSD bits can't be turned down.
    Level,
    /// The device doesn't run at the PCM rate DoP needs (`khz`) for this DSD rate (`dsd`).
    Rate { device: String, khz: f64, dsd: String },
    /// The file has more channels (`channels`) than the device: DoP can't fold them down.
    Channels { channels: usize },
}

/// The queue stopped at a track it couldn't play: which one, and the engine's reason.
#[derive(Serialize, Clone, Debug)]
struct AdvanceError {
    name: String,
    error: UserError,
}

#[derive(Default)]
struct AppState {
    player: Mutex<Option<Player>>,
    current: Mutex<Option<Current>>,
    /// Each output's processing, by settings key (see `device_key`), as the UI last sent it;
    /// an output with none plays with [`default_dsp`]. The EQ in them comes from `eq`, per
    /// output. Kept in memory only: the UI saves the settings and sends them at start-up.
    dsp: Mutex<std::collections::HashMap<String, DspSettings>>,
    eq: Mutex<eq::EqStore>,
    /// The last impulse response loaded (path, response), so settings changes don't reload it.
    ir: Mutex<Option<(String, Arc<ImpulseResponse>)>>,
    queue: Mutex<Queue>,
    /// What plays now, as last sent to the UI (the queue thread changes it on its own).
    now: Mutex<Option<NowPlayingDto>>,
    /// Bumped whenever `now` changes, so the UI knows to fetch it.
    now_rev: AtomicU64,
    /// One start at a time: a click in the UI and the queue's own advance can't race.
    play_lock: Mutex<()>,
    /// The user turned off the short fades at pause, seek and stop (on unless they did).
    fades_off: AtomicBool,
    /// Why the queue stopped at a track it couldn't play.
    advance_error: Mutex<Option<AdvanceError>>,
    /// The last status sent to the UI, answered again while a track is starting.
    last_status: Mutex<Option<StatusDto>>,
    /// Tracks opened ahead to follow the current one without a break (see `follow_queue`).
    lined_up: Mutex<Vec<LinedUp>>,
    /// The queue entry (index, path) that failed to open ahead: not tried again every tick.
    line_up_failed: Mutex<Option<(usize, String)>>,
    /// The last automatic step down of the upsampling filter (see `step_down`), until
    /// the UI sends settings of its own for the output that plays.
    filter_step: Mutex<Option<upsampling::FilterStep>>,
    filter_step_seq: AtomicU64,
    /// The last automatic step down of the DSD output (see `step_down`), likewise.
    dsd_step: Mutex<Option<dsd_out::DsdStep>>,
    dsd_step_seq: AtomicU64,
}

/// A queue entry opened ahead of time, to join on after the current track (gapless).
struct LinedUp {
    index: usize,
    path: String,
    track: Arc<Track>,
    dsd_note: Option<DsdNote>,
    /// The player can't join it on (another rate or kind): it starts anew when the current
    /// track ends — already decoded, at least.
    refused: bool,
}

/// The player, unless a track is starting right now. Starting holds this lock for as long as
/// the device takes to switch rates (up to seconds); the UI's polling must not wait for that.
fn player_now(state: &AppState) -> Option<MutexGuard<'_, Option<Player>>> {
    match state.player.try_lock() {
        Ok(g) => Some(g),
        Err(TryLockError::WouldBlock) => None,
        Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
    }
}

const CROSSFEED_LEVELS: [(&str, CrossfeedLevel); 3] = [
    ("light", CrossfeedLevel::JAN_MEIER),
    ("medium", CrossfeedLevel::CHU_MOY),
    ("strong", CrossfeedLevel::DEFAULT),
];

fn crossfeed_key(level: CrossfeedLevel) -> String {
    CROSSFEED_LEVELS.iter().find(|(_, l)| *l == level).map_or("custom", |(k, _)| k).to_string()
}

/// Errors cross to the UI as codes (`{ code, ...params }`), worded there in the user's language.
type CmdResult<T> = Result<T, UserError>;

/// The app, for code that runs outside a command (system media controls).
static APP: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();

/// Hand a playback command (`toggle`, `next`, `prev`) to the window, which carries it out as
/// its own buttons would — the path the phone remote takes. Used by the desktop media keys.
#[cfg(desktop)]
pub(crate) fn remote_command(app: &tauri::AppHandle, kind: &str) {
    use tauri::Emitter;
    let _ = app.emit("remote", serde_json::json!({ "kind": kind }));
}

fn err(e: impl Into<anyhow::Error>) -> UserError {
    UserError::of(&e.into())
}

// ---------------------------------------------------------------------------------------------
// DTOs sent to the UI
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceDto {
    uid: String,
    /// What the output's processing settings are remembered under (see `device_key`).
    key: String,
    name: String,
    transport: String,
    channels: u32,
    rates: Vec<f64>,
    nominal_rate: f64,
    is_default: bool,
    /// The connection itself can carry untouched PCM (false for Bluetooth/AirPlay).
    bit_perfect_capable: bool,
    /// Highest DSD rate (64, 128, 256, 512) the device's clock and formats could carry as DoP.
    max_dop: Option<u32>,
    /// A network player (UPnP/DLNA): no exclusive mode or DoP to set, maybe a volume.
    network: bool,
    /// A network player's maker and model ("WiiM Pro"), to tell it apart.
    model: Option<String>,
    /// A network player's protocol ("UPnP", "Chromecast"): a device may speak both.
    protocol: Option<String>,
}

impl From<&Output> for DeviceDto {
    fn from(d: &Output) -> Self {
        let transport = d.transport();
        Self {
            uid: d.uid().to_string(),
            key: device_key::of(d),
            name: d.name().trim().to_string(),
            transport: format!("{transport:?}"),
            channels: d.output_channels(),
            rates: hifi_engine::rate::candidate_rates(&d.rates()),
            nominal_rate: d.nominal_rate(),
            is_default: d.is_default(),
            bit_perfect_capable: transport.can_be_bit_perfect(),
            max_dop: (transport.can_be_bit_perfect() && !d.is_network())
                .then(|| [8u32, 4, 2, 1].into_iter().find(|m| d.supports_dop(dsd::DSD64 * m)).map(|m| 64 * m))
                .flatten(),
            network: d.is_network(),
            model: match d {
                Output::Network(t) => Some(t.model()).filter(|m| !m.is_empty()),
                Output::Device(_) => None,
            },
            protocol: match d {
                Output::Network(t) => Some(t.protocol().to_string()),
                Output::Device(_) => None,
            },
        }
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SignalDto {
    source_rate: u32,
    source_bits: Option<u32>,
    device_rate: f64,
    dac_bits: Option<u32>,
    dac_float: bool,
    integer_mode: bool,
    exclusive: bool,
    transport: String,
    resampled: bool,
    eq: Option<String>,
    eq_linear: bool,
    convolution: Option<String>,
    crossfeed: Option<String>,
    /// Source is MP3/AAC/Vorbis/Opus.
    source_lossy: bool,
    /// Anti-clipping gain applied to this lossy track (dB, negative), if any.
    headroom_db: Option<f64>,
    /// Headroom measured on the whole track (false = safe default while a stream downloads).
    headroom_measured: bool,
    /// Encoder low-pass the highs are restored from (Hz), if restoration runs.
    restore_from_hz: Option<f64>,
    /// The gain this track plays at (volume levelling or peak headroom), dB.
    level_db: Option<f64>,
    /// What it is based on: "track", "album", "peak" (headroom only) or "unknown" (not
    /// measured yet: plays as it is).
    level_basis: Option<Basis>,
    /// The gain was held back to keep the track's peak under full scale.
    level_limited: bool,
    /// DSD source: its 1-bit rate (Hz).
    dsd_rate: Option<u32>,
    /// DSD goes out as DoP (false with `dsd_rate` set = converted to PCM).
    dop: bool,
    /// Why DoP was asked for but this DSD file plays as PCM.
    dsd_note: Option<DsdNote>,
    /// The source's channels: "Stereo", "Mono", "5.1", "10 ch"…
    layout: String,
    /// A multichannel source folded down to the device's speakers.
    downmix: Option<DownmixDto>,
    /// The long upsampling filter that runs ("linear", "minimum", "apodizing"); None = the
    /// standard resampler, or no upsampling.
    upsample_filter: Option<String>,
    /// Its length in taps (None for apodizing).
    upsample_taps: Option<u32>,
    /// PCM encoded to DSD and sent as DoP: the rate that runs (64, 128, 256), which may be
    /// lower than the one asked for (the device's limit). None = it doesn't run.
    dsd_out: Option<u32>,
    /// The modulator's order (5, 7) with `dsd_out`.
    dsd_out_order: Option<u32>,
    /// Why DSD output was asked for and doesn't run (None when it runs, and for a DSD file
    /// sent as DoP, which ignores it).
    dsd_out_note: Option<DsdOutNote>,
    bit_perfect: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DownmixDto {
    to: String,
    gain_db: f64,
}

impl From<&SignalPath> for SignalDto {
    fn from(p: &SignalPath) -> Self {
        let (upsample_filter, upsample_taps) = upsampling::names(p.upsample_filter);
        let (dsd_out, dsd_out_order) = dsd_out::names(p.dsd_out);
        Self {
            source_rate: p.source_rate,
            source_bits: p.source_bits,
            device_rate: p.device_rate,
            dac_bits: p.physical.map(|f| f.bits),
            dac_float: p.physical.is_some_and(|f| f.float),
            integer_mode: p.integer_mode,
            exclusive: p.exclusive,
            transport: format!("{:?}", p.transport),
            resampled: p.resampled,
            eq: p.eq.clone(),
            eq_linear: p.eq_linear,
            convolution: p.convolution.clone(),
            crossfeed: p.crossfeed.map(crossfeed_key),
            source_lossy: p.source_lossy,
            headroom_db: p.lossy.filter(|l| l.gain < 1.0).map(|l| l.gain_db()),
            headroom_measured: p.lossy.is_none_or(|l| l.gain_measured),
            restore_from_hz: p.lossy.and_then(|l| l.restore_from_hz),
            level_db: p.level.map(|l| l.gain_db()),
            level_basis: p.level.map(|l| l.basis),
            level_limited: p.level.is_some_and(|l| l.limited),
            dsd_rate: p.dsd.map(|d| d.rate),
            dop: p.dsd.is_some_and(|d| d.dop),
            dsd_note: None,
            layout: p.layout.clone(),
            downmix: p.downmix.as_ref().map(|d| DownmixDto { to: d.to.clone(), gain_db: d.gain_db }),
            upsample_filter,
            upsample_taps,
            dsd_out,
            dsd_out_order,
            dsd_out_note: None,
            bit_perfect: p.bit_perfect,
        }
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct NowPlayingDto {
    path: String,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    year: Option<String>,
    /// `data:` URL, ready for an <img src>.
    cover: Option<String>,
    channels: usize,
    duration: f64,
    device_name: String,
    signal: SignalDto,
    /// The `now_rev` this track started as.
    rev: u64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusDto {
    active: bool,
    position: f64,
    duration: f64,
    paused: bool,
    finished: bool,
    /// Peak level after processing (dBFS); None on the bit-perfect path.
    peak_db: Option<f64>,
    underruns: u64,
    /// Waiting for audio not yet decoded/downloaded.
    buffering: bool,
    /// Seconds decoded so far (== duration once the whole track is in RAM).
    buffered: f64,
    /// Decoding stopped early (corrupt file, dropped connection), or the output hit a problem
    /// (device gone; iOS: volume below maximum during DoP).
    error: Option<UserError>,
    /// Queue position of the playing track (moves on by itself at the end of a track).
    queue_index: Option<usize>,
    /// Changes whenever a different track starts; then fetch `now_playing`.
    now_rev: u64,
    /// The queue stopped at a track it could not play.
    advance_error: Option<AdvanceError>,
    /// System volume 0…1 where the OS may scale our samples with it (iOS); else None.
    system_volume: Option<f32>,
    /// The processing's share of one core (0 until measured); None on the direct path.
    load: Option<f32>,
    /// The upsampling filter was stepped down because the processor couldn't keep up.
    filter_step: Option<upsampling::FilterStep>,
    /// The DSD output was stepped down (or off) because the processor couldn't keep up.
    dsd_step: Option<dsd_out::DsdStep>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IrDto {
    name: String,
    channels: usize,
    rate: u32,
    seconds: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DspResult {
    /// New signal path if something was playing and got restarted with the new settings.
    signal: Option<SignalDto>,
    convolution: Option<IrDto>,
}

// ---------------------------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------------------------

fn find_device(uid: &str) -> CmdResult<Output> {
    outputs::find(uid).map_err(err)?.ok_or(UserError::DeviceNotFound)
}

/// Stop whatever plays and start `track` on `device`. The old player is dropped *first*, so
/// its hog mode is released and the device restored before we claim it again.
///
/// `dsd_out_note`: why the DSD output asked for doesn't run (see [`dsp_for`]), for the signal
/// path. A device that was opened for DSD output and failed to start (for any reason) gets
/// one more try with PCM, and the signal path says why.
fn start(state: &AppState, device: &Output, track: Arc<Track>, opts: PlayOptions, dsd_out_note: Option<DsdOutNote>) -> CmdResult<SignalDto> {
    let mut slot = state.player.lock().unwrap();
    if let Some(p) = slot.take() {
        // What sounds now ends in a short fade, not a click.
        p.fade_out();
    }
    let mut refused = None;
    let player = match (Player::start(device, track.clone(), &opts).map_err(err), opts.dsp.dsd_out) {
        // Opened for DSD output and failed, whatever the reason (the device refused DoP, or
        // the format, or the rate, or didn't open at that rate at all): once more with PCM.
        // Should that fail too, its error is the one to show: it is about plain playback.
        (Err(_), Some(asked)) if !track.is_dop() => {
            refused = Some(DsdOutNote::refused(device.name(), asked));
            // The track's gain stays as it was set for the encoder (`set_level`): levelling
            // gives the same gain either way, and the peak headroom it adds (a dB or so, at
            // most what keeps the true peaks at full scale) applies only if this playback
            // processes at all; a bit-perfect one ignores the gain. At worst the track plays
            // that much quieter, never louder, and bit-perfect playback stays bit-perfect.
            let pcm = PlayOptions { dsp: DspSettings { dsd_out: None, ..opts.dsp.clone() }, ..opts.clone() };
            Player::start(device, track.clone(), &pcm).map_err(err)?
        }
        (r, _) => r?,
    };
    let mut signal = SignalDto::from(&player.path);
    signal.dsd_out_note = if signal.dsd_out.is_some() || track.is_dop() { None } else { refused.or(dsd_out_note) };
    *slot = Some(player);
    Ok(signal)
}

/// How to carry a DSD file to this device. DoP only when the user switched it on for the
/// device, the device can take it, we have exclusive access, and no EQ/crossfeed has to run
/// (those need PCM). Returns the mode and, for a PCM fallback, the reason.
fn dsd_mode(path: &std::path::Path, device: &Output, exclusive: bool, dop: bool, dsp: &DspSettings) -> (DsdMode, Option<DsdNote>) {
    if !dop || !dsd::is_dsd_path(path) {
        return (DsdMode::Pcm, None);
    }
    let (rate, channels) = match dsd::probe(path) {
        Ok(h) => (h.rate, h.channels),
        Err(_) => return (DsdMode::Pcm, None),
    };
    let note = if !exclusive {
        Some(DsdNote::Exclusive)
    } else if output::dop_blocker().is_some() {
        Some(DsdNote::Volume)
    } else if dsp.eq.is_some() || dsp.convolution.is_some() || dsp.crossfeed.is_some() {
        Some(DsdNote::Dsp)
    } else if dsp.level.mode != LevelMode::Off {
        Some(DsdNote::Level)
    } else if channels > 2 && channels > device.output_channels() as usize {
        Some(DsdNote::Channels { channels })
    } else if !device.supports_dop(rate) {
        Some(DsdNote::Rate {
            device: device.name().trim().to_string(),
            khz: rate as f64 / 16_000.0,
            dsd: dsd::dsd_name(rate),
        })
    } else {
        None
    };
    (if note.is_none() { DsdMode::Dop } else { DsdMode::Pcm }, note)
}

/// Open a library file — or a Qobuz/TIDAL track, whose "path" is its id (`qobuz:track:…`).
fn open_for(path: &std::path::Path, mode: DsdMode) -> CmdResult<Arc<Track>> {
    open_for_at(path, mode, 0.0)
}

/// [`open_for`], decoding from `start_secs` on (a stream always starts at its beginning).
fn open_for_at(path: &std::path::Path, mode: DsdMode, start_secs: f64) -> CmdResult<Arc<Track>> {
    match path.to_str().filter(|p| stream::is_uri(p)) {
        Some(uri) => stream::open(uri).map_err(|e| UserError::of(&e)),
        None => open_file_at(path, mode, start_secs).map_err(err),
    }
}

/// The OS's preferred UI languages, best first (BCP 47, e.g. "de-DE", "zh-Hant-TW"). The web
/// view's navigator.languages can't be trusted for this: WKWebView reports the languages the
/// app bundle declares rather than the user's own.
#[tauri::command]
fn system_locales() -> Vec<String> {
    // Android: the app's own configuration, which includes a language picked for this app in
    // Settings → Apps → BitSpring → Language (sys-locale reads only the system-wide one).
    #[cfg(target_os = "android")]
    {
        let own = android_media::locales();
        if !own.is_empty() {
            return own;
        }
    }
    sys_locale::get_locales().collect()
}

/// macOS menu bar in the UI's language: the UI sends its translations of the `menu.*` keys
/// (with the app name filled in) whenever the language changes. Elsewhere there is no menu bar.
#[tauri::command]
fn set_menu_labels(app: tauri::AppHandle, labels: std::collections::HashMap<String, String>) -> CmdResult<()> {
    #[cfg(target_os = "macos")]
    {
        let menu = mac_menu(&app, &labels).map_err(err)?;
        app.set_menu(menu).map_err(err)?;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (app, labels);
    Ok(())
}

/// The standard app, Edit, View and Window menus (as Tauri's default one), with our labels.
#[cfg(target_os = "macos")]
fn mac_menu(app: &tauri::AppHandle, labels: &std::collections::HashMap<String, String>) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{AboutMetadata, Menu, PredefinedMenuItem as P, Submenu};
    let l = |k: &str| labels.get(k).map(String::as_str);
    let name = app.package_info().name.clone();
    let about = AboutMetadata { name: Some(name.clone()), version: Some(app.package_info().version.to_string()), ..Default::default() };
    let app_menu = Submenu::with_items(
        app,
        &name,
        true,
        &[
            &P::about(app, l("menu.about"), Some(about))?,
            &P::separator(app)?,
            &P::services(app, l("menu.services"))?,
            &P::separator(app)?,
            &P::hide(app, l("menu.hide"))?,
            &P::hide_others(app, l("menu.hideOthers"))?,
            &P::show_all(app, l("menu.showAll"))?,
            &P::separator(app)?,
            &P::quit(app, l("menu.quit"))?,
        ],
    )?;
    let edit = Submenu::with_items(
        app,
        l("menu.edit").unwrap_or("Edit"),
        true,
        &[
            &P::undo(app, l("menu.undo"))?,
            &P::redo(app, l("menu.redo"))?,
            &P::separator(app)?,
            &P::cut(app, l("menu.cut"))?,
            &P::copy(app, l("menu.copy"))?,
            &P::paste(app, l("menu.paste"))?,
            &P::select_all(app, l("menu.selectAll"))?,
        ],
    )?;
    let view = Submenu::with_items(app, l("menu.view").unwrap_or("View"), true, &[&P::fullscreen(app, l("menu.fullscreen"))?])?;
    let window = Submenu::with_items(
        app,
        l("menu.window").unwrap_or("Window"),
        true,
        &[
            &P::minimize(app, l("menu.minimize"))?,
            &P::maximize(app, l("menu.zoom"))?,
            &P::separator(app)?,
            &P::close_window(app, l("menu.close"))?,
        ],
    )?;
    // macOS lists the open windows in it and adds its own window commands.
    window.set_as_windows_menu_for_nsapp()?;
    Menu::with_items(app, &[&app_menu, &edit, &view, &window])
}

const AUDIO_EXTENSIONS: [&str; 17] = ["flac", "wav", "aif", "aiff", "m4a", "alac", "mp3", "ogg", "opus", "caf", "mka", "dsf", "dff", "ape", "wv", "tta", "iso"];

/// Turn dropped files and folders into a flat, ordered list of audio files.
/// Folders are walked recursively and sorted by path, so an album lands in track order.
#[tauri::command(async)]
fn expand_paths(paths: Vec<String>) -> Vec<ExpandedDto> {
    fn is_audio(p: &std::path::Path) -> bool {
        p.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
    }
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        entries.sort();
        for p in entries {
            let hidden = p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.'));
            if hidden {
                continue;
            }
            if p.is_dir() {
                walk(&p, out);
            } else if is_audio(&p) {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    for p in paths.iter().map(PathBuf::from) {
        if p.is_dir() {
            walk(&p, &mut out);
        } else if is_audio(&p) {
            out.push(p);
        }
    }
    // An album ripped to one file with a CUE sheet goes in as its tracks.
    hifi_engine::cue::expand(out)
        .into_iter()
        .map(|(path, meta)| {
            let meta = meta.unwrap_or_default();
            ExpandedDto { path, title: meta.title, artist: meta.artist }
        })
        .collect()
}

/// A file to queue, with the title and artist a CUE sheet gives it (the UI shows the file name
/// of anything else until it plays).
#[derive(Serialize)]
struct ExpandedDto {
    path: String,
    title: Option<String>,
    artist: Option<String>,
}

#[tauri::command(async)]
fn list_devices(search: Option<bool>) -> CmdResult<Vec<DeviceDto>> {
    // Someone is looking at the list (or a network player is the chosen output): look for
    // network players (again, at most every so often).
    if search.unwrap_or(false) {
        outputs::refresh();
    }
    Ok(outputs::outputs().map_err(err)?.iter().map(DeviceDto::from).collect())
}

/// A network player's own volume, 0…100 (None for other outputs and players without one).
#[tauri::command(async)]
fn output_volume(uid: String) -> Option<u32> {
    match outputs::find(&uid).ok()?? {
        Output::Network(t) if t.has_volume() => t.volume().ok(),
        _ => None,
    }
}

#[tauri::command(async)]
fn set_output_volume(uid: String, volume: u32) {
    if let Ok(Some(Output::Network(t))) = outputs::find(&uid) {
        let _ = t.set_volume(volume);
    }
}

/// The processing of an output the UI has sent no settings for: what the UI starts every
/// output with. Nothing that changes the signal; the lossy headroom (lossy files only, which
/// are never bit-perfect) and the peak headroom (only while other processing runs) are on.
fn default_dsp() -> DspSettings {
    DspSettings {
        lossy: LossySettings { headroom: true, restore_highs: false },
        level: LevelSettings { mode: LevelMode::Off, headroom: true },
        ..Default::default()
    }
}

/// The processing asked for on `device`: its own settings plus its EQ. Its `dsd_out` is a
/// wish, not yet what can run: never hand this to the engine or to `set_level` as it is, go
/// through [`dsp_for`] (a playback) or [`dsp_offline`] (none).
fn wanted_dsp(state: &AppState, device: &Output) -> DspSettings {
    let mut dsp = state.dsp.lock().unwrap().get(&device_key::of(device)).cloned().unwrap_or_else(default_dsp);
    let eq = state.eq.lock().unwrap();
    dsp.eq = eq.effective(&device_key::of(device));
    dsp.eq_bypass = eq.bypass;
    dsp
}

/// The processing for one playback on `device`, `exclusive` as asked and `dop` the device's
/// DoP switch, and why the DSD output asked for doesn't run, if it doesn't.
///
/// Invariant: every `DspSettings` that reaches the engine (`PlayOptions`) or `set_level` for
/// a playback comes from here, so its `dsd_out` is what can run on that device for that
/// playback (see `dsd_out::resolve`), not the user's wish. A wish left in would make
/// `set_level` see processing (`reshapes`) and give the track a peak headroom cut meant for
/// the encoder, which a playback that then can't encode would still apply wherever it
/// processes for another reason; and the signal path couldn't say why nothing was encoded.
/// (The engine itself falls back to PCM silently.) A DSD file sent as DoP ignores `dsd_out`
/// in the engine and gets no note (`start` drops it).
fn dsp_for(state: &AppState, device: &Output, exclusive: bool, dop: bool) -> (DspSettings, Option<DsdOutNote>) {
    let mut dsp = wanted_dsp(state, device);
    let (out, note) = dsd_out::resolve(dsp.dsd_out, device, exclusive, dop);
    dsp.dsd_out = out;
    (dsp, note)
}

/// The processing asked for on `device`, for work that plays nothing through it (an offline
/// render): no DSD output.
fn dsp_offline(state: &AppState, device: &Output) -> DspSettings {
    DspSettings { dsd_out: None, ..wanted_dsp(state, device) }
}

/// Set the gain `track` plays at with these settings (volume levelling, peak headroom): call
/// after opening it, before it starts or is lined up. `wait`: how long a track nothing is
/// known about may take to load so it can be measured on the spot.
fn set_level(dsp: &DspSettings, file: &std::path::Path, track: &Arc<Track>, wait: Duration) {
    use tauri::Manager;
    let Some(app) = APP.get() else { return };
    let (levels, lib) = (app.state::<loudness::Loudness>(), app.state::<library::Library>());
    let shuffled = app.state::<AppState>().queue.lock().unwrap().shuffle;
    levels.level(&lib, &level_for(dsp, shuffled), &file.to_string_lossy(), track, wait);
}

/// The settings a track's gain goes by. An album's one gain keeps its quiet tracks quiet next
/// to its loud ones — right when the album plays through, wrong in a shuffled queue, where a
/// track follows one from another album: there every track is levelled by itself.
fn level_for(dsp: &DspSettings, shuffled: bool) -> std::borrow::Cow<'_, DspSettings> {
    if shuffled && dsp.level.mode == LevelMode::Album {
        let mut by_track = dsp.clone();
        by_track.level.mode = LevelMode::Track;
        return std::borrow::Cow::Owned(by_track);
    }
    std::borrow::Cow::Borrowed(dsp)
}

/// How long opening a track may wait for it to load, to measure a loudness nobody knows yet:
/// when the user asked for it now, and when it is lined up well before its turn.
const MEASURE_NOW: Duration = Duration::from_millis(1500);
const MEASURE_AHEAD: Duration = Duration::from_secs(6);

/// Start `file` (already opened as `track`) and record it as what plays now.
fn begin(
    state: &AppState,
    device: &Output,
    file: PathBuf,
    track: Arc<Track>,
    exclusive: bool,
    start_at: f64,
    start_paused: bool,
    dop: bool,
    dsd_note: Option<DsdNote>,
) -> CmdResult<NowPlayingDto> {
    let (dsp, dsd_out_note) = dsp_for(state, device, exclusive, dop);
    let opts = PlayOptions { exclusive, dsp, start_at, start_paused, fades: !state.fades_off.load(Ordering::Relaxed) };
    let mut signal = start(state, device, track.clone(), opts, dsd_out_note)?;
    signal.dsd_note = dsd_note.clone();
    let current = Current {
        track,
        path: file,
        device_uid: device.uid().to_string(),
        key: device_key::of(device),
        exclusive,
        dop,
        dsd_note,
        dsd_out_note: signal.dsd_out_note.clone(),
    };
    Ok(record_now(state, current, device.name().trim().to_string(), signal))
}

/// Make `current` what plays now: for the UI (a new `now_rev`) and the lock screen.
/// Call with `play_lock` held.
fn record_now(state: &AppState, current: Current, device_name: String, signal: SignalDto) -> NowPlayingDto {
    let (track, file) = (current.track.clone(), current.path.clone());
    let meta = track.meta.clone();
    let (channels, duration) = (track.channels, track.duration_secs());
    #[cfg(system_media)]
    let lock_screen = media::TrackInfo {
        title: meta.title.clone().unwrap_or_else(|| file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()),
        artist: meta.artist.clone(),
        album: meta.album.clone(),
        duration,
        cover: meta.cover.as_ref().map(|(_, b)| b.clone()),
    };
    let path = file.to_string_lossy().into_owned();
    let file_stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    *state.current.lock().unwrap() = Some(current);
    let rev = state.now_rev.load(Ordering::Acquire) + 1;
    let now = NowPlayingDto {
        rev,
        title: meta.title.unwrap_or(file_stem),
        artist: meta.artist,
        album: meta.album,
        year: meta.year,
        cover: meta.cover.map(|(mime, bytes)| {
            format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
        }),
        channels,
        duration,
        device_name,
        signal,
        path,
    };
    *state.now.lock().unwrap() = Some(now.clone());
    *state.advance_error.lock().unwrap() = None;
    // Under play_lock: nobody else bumps it between the load above and here.
    state.now_rev.store(rev, Ordering::Release);
    #[cfg(system_media)]
    if let Some(app) = APP.get() {
        media::set_track(app, rev, lock_screen);
    }
    now
}

#[tauri::command]
async fn play_file(
    state: State<'_, AppState>,
    path: String,
    device_uid: String,
    exclusive: bool,
    start_at: Option<f64>,
    dop: Option<bool>,
    index: Option<usize>,
    paused: Option<bool>,
) -> CmdResult<NowPlayingDto> {
    let device = find_device(&device_uid)?;
    let file = PathBuf::from(&path);
    let dop = dop.unwrap_or(false);
    let (dsp, _) = dsp_for(&state, &device, exclusive, dop);
    let (mode, dsd_note) = dsd_mode(&file, &device, exclusive, dop, &dsp);
    // Only the headers are parsed here; the decoder keeps filling RAM in the background
    // while playback starts. Still off the UI/IPC threads: opening may touch a slow disk.
    let track = {
        let file = file.clone();
        // Resuming in the middle (another output): decode from there, not from the start.
        let at = start_at.unwrap_or(0.0);
        tauri::async_runtime::spawn_blocking(move || {
            let track = open_for_at(&file, mode, at)?;
            set_level(&dsp, &file, &track, MEASURE_NOW);
            Ok::<_, UserError>(track)
        })
        .await
        .map_err(err)??
    };
    let _one = state.play_lock.lock().unwrap();
    let now = begin(&state, &device, file, track, exclusive, start_at.unwrap_or(0.0), paused.unwrap_or(false), dop, dsd_note)?;
    if let Some(i) = index {
        state.queue.lock().unwrap().index = Some(i);
    }
    Ok(now)
}

/// The UI's queue changed. `rev` is the `now_rev` the UI last saw: if a track has started
/// since (the queue moved on by itself), our index is newer than the UI's and stays.
#[tauri::command]
fn set_queue(state: State<'_, AppState>, paths: Vec<String>, index: Option<usize>, rev: u64) {
    let mut q = state.queue.lock().unwrap();
    q.paths = paths;
    if rev == state.now_rev.load(Ordering::Acquire) {
        q.index = index;
    }
    if q.index.is_some_and(|i| i >= q.paths.len()) {
        q.index = None;
    }
}

/// Shuffle and repeat, as the UI's keys stand. (Shuffling itself is the UI reordering the
/// queue; here it only changes how volume levelling treats albums.)
#[tauri::command]
fn set_play_modes(state: State<'_, AppState>, shuffle: bool, repeat: Repeat) {
    let mut q = state.queue.lock().unwrap();
    q.shuffle = shuffle;
    q.repeat = repeat;
}

#[tauri::command]
fn now_playing(state: State<'_, AppState>) -> Option<NowPlayingDto> {
    state.now.lock().unwrap().clone()
}

/// Start queue entry `i` on the output the current track plays on. On failure the queue
/// stops there, with the reason for the UI. Call with `play_lock` held.
fn play_queue_at(state: &AppState, i: usize) {
    let Some((device_uid, exclusive, dop)) =
        state.current.lock().unwrap().as_ref().map(|c| (c.device_uid.clone(), c.exclusive, c.dop))
    else {
        return; // channel test, or stopped
    };
    let Some(path) = state.queue.lock().unwrap().paths.get(i).cloned() else { return };
    let file = PathBuf::from(&path);
    let ahead = take_lined_up(state, i, &path);
    let result = find_device(&device_uid).and_then(|device| {
        let (dsp, _) = dsp_for(state, &device, exclusive, dop);
        let (mode, note) = dsd_mode(&file, &device, exclusive, dop, &dsp);
        // Opened ahead (and decoding already) — unless the settings changed how DSD goes out.
        let track = match ahead.filter(|t| t.is_dop() == (mode == DsdMode::Dop)) {
            // (Its level was set when it was lined up; again, in case more is known by now.)
            Some(t) => {
                set_level(&dsp, &file, &t, Duration::ZERO);
                t
            }
            None => {
                let t = open_for(&file, mode)?;
                set_level(&dsp, &file, &t, MEASURE_NOW);
                t
            }
        };
        begin(state, &device, file.clone(), track, exclusive, 0.0, false, dop, note)
    });
    state.queue.lock().unwrap().index = Some(i);
    if let Err(e) = result {
        drop(state.player.lock().unwrap().take());
        *state.current.lock().unwrap() = None;
        let name = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or(path);
        *state.advance_error.lock().unwrap() = Some(AdvanceError { name, error: e });
        state.now_rev.fetch_add(1, Ordering::AcqRel);
    }
}

/// At the end of a track, start the one that follows it in the queue (with repeat on, that
/// may be the first track, or the same one) on the same output. True when the last track has
/// played out and nothing follows (the player sits finished).
fn advance(state: &AppState) -> bool {
    let _one = state.play_lock.lock().unwrap();
    if !state.player.lock().unwrap().as_ref().is_some_and(|p| p.is_finished()) {
        return false;
    }
    let next = state.queue.lock().unwrap().following();
    match next {
        Some(i) => {
            play_queue_at(state, i);
            false
        }
        None => true,
    }
}

/// How long a finished queue keeps the DAC: a quick replay or a new track right after reuses
/// it as it is; after that, other apps get it back (exclusive mode, the sample rate we set).
const RELEASE_AFTER: Duration = Duration::from_secs(5);

/// The queue ended a while ago: let go of the output. The track stays shown; Play starts it
/// again on the same output.
fn release_if_still_finished(state: &AppState) {
    let _one = state.play_lock.lock().unwrap();
    let mut player = state.player.lock().unwrap();
    if player.as_ref().is_some_and(|p| p.is_finished()) {
        drop(player.take());
    }
}

/// How long before the end of a track the next one in the queue is opened and lined up
/// (more behind a long upsampling filter: see [`line_up_before`]).
const LINE_UP_BEFORE: f64 = 20.0;

/// [`LINE_UP_BEFORE`] for what plays now: a long filter makes the render thread run ahead of
/// what is heard by the input it holds back, so the next track is wanted that much earlier.
fn line_up_before(state: &AppState) -> f64 {
    let playing = player_now(state).and_then(|g| g.as_ref().and_then(|p| p.path.upsample_filter.map(|f| (f, p.path.source_rate))));
    // (Without the player's lock: the filter's figures may have to be fetched.)
    let extra = playing.map_or(0.0, |(spec, rate)| upsampling::line_up_extra(Some(upsampling::startup_frames(spec)), rate));
    LINE_UP_BEFORE + extra
}

/// The gapless half of the queue: notice when the player has moved on to a lined-up track by
/// itself (and show it), and line up the next one as the current track nears its end.
fn follow_queue(state: &AppState) {
    {
        let _one = state.play_lock.lock().unwrap();
        took_over(state);
    }
    let before = line_up_before(state);
    let Some(want) = wanted_next(state, before) else { return };
    let known = state.lined_up.lock().unwrap().iter().find(|l| l.index == want.index && l.path == want.path).map(|l| (l.track.clone(), l.refused));
    let track = match known {
        Some((_, true)) => return,
        Some((track, false)) => track,
        None => {
            let key = (want.index, want.path.clone());
            if state.line_up_failed.lock().unwrap().as_ref() == Some(&key) {
                return;
            }
            // Opening may take a moment (a stream): no locks held meanwhile.
            let file = PathBuf::from(&want.path);
            let Ok(device) = find_device(&want.device_uid) else { return };
            // (Resolved as for the playback it joins on to: its gain is set by these.)
            let (dsp, _) = dsp_for(state, &device, want.exclusive, want.dop);
            let (mode, dsd_note) = dsd_mode(&file, &device, want.exclusive, want.dop, &dsp);
            let track = match open_for(&file, mode) {
                Ok(t) => t,
                Err(_) => {
                    // It fails again, with its reason shown, when its turn comes.
                    *state.line_up_failed.lock().unwrap() = Some(key);
                    return;
                }
            };
            set_level(&dsp, &file, &track, MEASURE_AHEAD);
            let mut l = state.lined_up.lock().unwrap();
            l.retain(|e| e.index == want.index);
            l.push(LinedUp { index: want.index, path: want.path.clone(), track: track.clone(), dsd_note, refused: false });
            track
        }
    };
    let _one = state.play_lock.lock().unwrap();
    let player = state.player.lock().unwrap();
    let Some(p) = player.as_ref() else { return };
    // Still the same situation as when we looked?
    let same = state.current.lock().unwrap().as_ref().is_some_and(|c| Arc::ptr_eq(&c.track, &p.heard()))
        && {
            let q = state.queue.lock().unwrap();
            q.following() == Some(want.index) && q.paths.get(want.index) == Some(&want.path)
        };
    if same && !p.queue_next(track.clone()) {
        let mut l = state.lined_up.lock().unwrap();
        if let Some(e) = l.iter_mut().find(|e| Arc::ptr_eq(&e.track, &track)) {
            e.refused = true;
        }
    }
}

/// The queue entry to line up now, when one should be and isn't yet.
struct WantNext {
    index: usize,
    path: String,
    device_uid: String,
    exclusive: bool,
    dop: bool,
}

fn wanted_next(state: &AppState, before: f64) -> Option<WantNext> {
    // A track starting right now: look again next time.
    let _one = state.play_lock.try_lock().ok()?;
    let player = state.player.lock().unwrap();
    let p = player.as_ref()?;
    let (device_uid, exclusive, dop) = {
        let c = state.current.lock().unwrap();
        let c = c.as_ref()?;
        if !Arc::ptr_eq(&c.track, &p.heard()) {
            return None; // took_over hasn't caught up yet
        }
        (c.device_uid.clone(), c.exclusive, c.dop)
    };
    if p.is_finished() {
        return None;
    }
    let (index, path) = {
        let q = state.queue.lock().unwrap();
        let i = q.following()?;
        (i, q.paths.get(i)?.clone())
    };
    // Forget what was lined up for a queue that has changed since.
    state.lined_up.lock().unwrap().retain(|e| e.index == index && e.path == path);
    if let Some(up) = p.upcoming() {
        let ours = state.lined_up.lock().unwrap().iter().any(|e| Arc::ptr_eq(&e.track, &up));
        if ours {
            return None;
        }
        p.clear_next();
    }
    if p.duration_secs() - p.position_secs() > before {
        return None;
    }
    Some(WantNext { index, path, device_uid, exclusive, dop })
}

/// The player moved on to a lined-up track by itself: make it what plays now (title, signal
/// path, queue position, lock screen). Call with `play_lock` held.
fn took_over(state: &AppState) {
    let mut player = state.player.lock().unwrap();
    let Some(p) = player.as_mut() else { return };
    let heard = p.heard();
    let mut current = state.current.lock().unwrap();
    let Some(cur) = current.as_mut() else { return };
    if Arc::ptr_eq(&heard, &cur.track) {
        return;
    }
    let entry = {
        let mut l = state.lined_up.lock().unwrap();
        l.iter().position(|e| Arc::ptr_eq(&e.track, &heard)).map(|i| l.remove(i))
    };
    let Some(e) = entry else {
        // Not one of ours (can't happen): at least stop looking at it.
        cur.track = heard;
        return;
    };
    p.path = p.path.for_next(&heard);
    let mut signal = SignalDto::from(&p.path);
    signal.dsd_note = e.dsd_note.clone();
    // The same player, so the same DSD output as before, and the same reason when there is
    // none (a refusal included).
    let dsd_out_note = if p.path.dsd_out.is_some() || heard.is_dop() { None } else { cur.dsd_out_note.clone() };
    signal.dsd_out_note = dsd_out_note.clone();
    drop(player);
    let next = Current {
        track: heard,
        path: PathBuf::from(&e.path),
        device_uid: cur.device_uid.clone(),
        key: cur.key.clone(),
        exclusive: cur.exclusive,
        dop: cur.dop,
        dsd_note: e.dsd_note,
        dsd_out_note,
    };
    drop(current);
    let device_name = state.now.lock().unwrap().as_ref().map(|n| n.device_name.clone()).unwrap_or_default();
    state.queue.lock().unwrap().index = Some(e.index);
    record_now(state, next, device_name, signal);
}

/// The lined-up track for queue entry `i`, if it was opened ahead.
fn take_lined_up(state: &AppState, i: usize, path: &str) -> Option<Arc<Track>> {
    let mut l = state.lined_up.lock().unwrap();
    let at = l.iter().position(|e| e.index == i && e.path == path)?;
    Some(l.remove(at).track)
}

/// Next (+1) or previous (−1) track, for controls outside the UI (lock screen, headphones).
/// "Previous" more than 3 s into a track restarts it, like the UI's button.
#[cfg_attr(desktop, allow(dead_code))]
pub(crate) fn jump(state: &AppState, delta: i64) {
    let _one = state.play_lock.lock().unwrap();
    let restart_here = {
        let player = state.player.lock().unwrap();
        match player.as_ref() {
            Some(p) if delta < 0 && p.position_secs() > 3.0 => Some(p.seek(0.0)),
            _ => None,
        }
    };
    match restart_here {
        Some(true) => return,
        Some(false) => {
            let _ = restart_locked(state, Some(0.0));
            return;
        }
        None => {}
    }
    let Some(i) = state.queue.lock().unwrap().step(delta) else { return };
    play_queue_at(state, i);
}

#[tauri::command(async)]
fn channel_test(state: State<'_, AppState>, device_uid: String) -> CmdResult<SignalDto> {
    let device = find_device(&device_uid)?;
    // Play at a rate the device already runs at, so the test itself switches nothing.
    // (A network player has no rate of its own: 48 kHz, which every one plays.)
    let rate = match device.nominal_rate().round() as u32 {
        0 => 48_000,
        r => r,
    };
    // The test bypasses processing: crossfeed would blur exactly what it checks.
    let _one = state.play_lock.lock().unwrap();
    *state.current.lock().unwrap() = None;
    start(&state, &device, Arc::new(testsignal::channel_test(rate)), PlayOptions::default(), None)
}

impl StatusDto {
    /// Nothing loaded.
    fn idle(
        queue_index: Option<usize>,
        now_rev: u64,
        advance_error: Option<AdvanceError>,
        system_volume: Option<f32>,
        filter_step: Option<upsampling::FilterStep>,
        dsd_step: Option<dsd_out::DsdStep>,
    ) -> Self {
        Self {
            active: false,
            position: 0.0,
            duration: 0.0,
            paused: false,
            finished: false,
            peak_db: None,
            underruns: 0,
            buffering: false,
            buffered: 0.0,
            error: None,
            queue_index,
            now_rev,
            advance_error,
            system_volume,
            load: None,
            filter_step,
            dsd_step,
        }
    }
}

#[tauri::command]
fn status(state: State<'_, AppState>) -> StatusDto {
    let queue_index = state.queue.lock().unwrap().index;
    let now_rev = state.now_rev.load(Ordering::Acquire);
    let advance_error = state.advance_error.lock().unwrap().clone();
    let system_volume = output::system_volume();
    let filter_step = state.filter_step.lock().unwrap().clone();
    let dsd_step = state.dsd_step.lock().unwrap().clone();
    let Some(player) = player_now(&state) else {
        // A track is starting: the last answer, as waiting for audio (and not finished: what
        // was finished is being replaced).
        let last = state.last_status.lock().unwrap().clone();
        return match last {
            Some(s) => StatusDto { buffering: true, finished: false, queue_index, now_rev, advance_error, system_volume, filter_step, dsd_step, ..s },
            None => StatusDto { buffering: true, ..StatusDto::idle(queue_index, now_rev, advance_error, system_volume, filter_step, dsd_step) },
        };
    };
    let s = match player.as_ref() {
        Some(p) => StatusDto {
            active: true,
            position: p.position_secs(),
            duration: p.duration_secs(),
            paused: p.is_paused(),
            finished: p.is_finished(),
            peak_db: p.output_peak().map(|v| 20.0 * (v.max(1e-9) as f64).log10()),
            underruns: p.underruns(),
            buffering: p.is_buffering(),
            buffered: p.buffered_secs(),
            error: p.source_error().map(UserError::other).or_else(|| p.output_error()),
            queue_index,
            now_rev,
            advance_error,
            system_volume,
            load: p.load(),
            filter_step,
            dsd_step,
        },
        None => StatusDto::idle(queue_index, now_rev, advance_error, system_volume, filter_step, dsd_step),
    };
    drop(player);
    *state.last_status.lock().unwrap() = Some(s.clone());
    s
}

/// Peak level per channel (dBFS, floored at −90) since the previous call.
#[tauri::command]
fn meter(state: State<'_, AppState>) -> [f64; 2] {
    let peaks = player_now(&state).and_then(|g| g.as_ref().map(|p| p.take_meter())).unwrap_or([0.0; 2]);
    peaks.map(|v| (20.0 * (v as f64).max(1e-9).log10()).max(-90.0))
}

#[tauri::command(async)]
fn set_paused(state: State<'_, AppState>, paused: bool) {
    if let Some(p) = state.player.lock().unwrap().as_ref() {
        p.set_paused(paused);
    }
}

#[tauri::command(async)]
fn seek(state: State<'_, AppState>, seconds: f64) {
    seek_to(&state, seconds);
}

/// Jump within the current track. In its last half second, when processing, the next track
/// is already on its way to the device: then the track starts anew at that point.
pub(crate) fn seek_to(state: &AppState, seconds: f64) {
    let _one = state.play_lock.lock().unwrap();
    let done = state.player.lock().unwrap().as_ref().map(|p| p.seek(seconds));
    if done == Some(false) {
        let _ = restart_locked(state, Some(seconds));
    }
}

#[tauri::command]
fn stop(state: State<'_, AppState>) {
    let _one = state.play_lock.lock().unwrap();
    if let Some(p) = state.player.lock().unwrap().take() {
        p.fade_out();
    }
    state.lined_up.lock().unwrap().clear();
    *state.current.lock().unwrap() = None;
    *state.now.lock().unwrap() = None;
    #[cfg(system_media)]
    if let Some(app) = APP.get() {
        media::clear(app);
    }
}

/// The short fades at pause, seek, stop and a change of track, on or off. Takes hold on what
/// plays now, without a restart. Never touches a track playing through: bit-perfect stays.
#[tauri::command]
fn set_fades(state: State<'_, AppState>, on: bool) {
    state.fades_off.store(!on, Ordering::Relaxed);
    if let Some(p) = state.player.lock().unwrap().as_ref() {
        p.set_fades(on);
    }
}

/// Replace the processing settings of the output `device_uid`. Everything off = bit-perfect.
/// If a track is loaded on that output it continues from the same position (and pause state)
/// with the new chain. Settings for another output are only kept, for when it plays: the UI
/// sends the new output's settings on a switch while the old one still plays, and that one
/// must not restart with them.
#[tauri::command]
async fn set_dsp(
    app: tauri::AppHandle,
    device_uid: String,
    upsample_khz: Option<f64>,
    crossfeed: Option<String>,
    convolution: Option<String>,
    headroom: Option<bool>,
    restore_highs: Option<bool>,
    level: Option<String>,
    peak_headroom: Option<bool>,
    upsample_filter: Option<String>,
    upsample_taps: Option<u32>,
    dsd_out: Option<u32>,
    dsd_order: Option<u32>,
) -> CmdResult<DspResult> {
    use tauri::Manager;
    let state = app.state::<AppState>();
    // Reading and decoding an impulse response can take a moment: off the IPC thread.
    let ir = match convolution {
        Some(path) => {
            let cached = state.ir.lock().unwrap().as_ref().filter(|(p, _)| *p == path).map(|(_, r)| r.clone());
            match cached {
                Some(r) => Some(r),
                None => {
                    let p = PathBuf::from(&path);
                    let r = tauri::async_runtime::spawn_blocking(move || ImpulseResponse::load(&p)).await.map_err(err)?.map_err(err)?;
                    *state.ir.lock().unwrap() = Some((path, r.clone()));
                    Some(r)
                }
            }
        }
        None => None,
    };
    // A long filter is designed once and kept; the longest minimum-phase one takes about half a
    // second the first time: that too off the IPC thread, before the restart needs it. It runs
    // when upsampling is on, and with DSD output (which always upsamples to the wire rate, see
    // `dsp::long_filter`) even when "upsample to" is off.
    let filter = upsampling::parse(upsample_filter.as_deref(), upsample_taps);
    let dsd_out = dsd_out::parse(dsd_out, dsd_order);
    if let Some(spec) = filter.filter(|_| upsample_khz.is_some() || dsd_out.is_some()) {
        tauri::async_runtime::spawn_blocking(move || drop(upsample::filter(spec))).await.map_err(err)?;
    }
    let ir_dto = ir.as_ref().map(|r| IrDto {
        name: r.name.clone(),
        channels: r.channels.len(),
        rate: r.rate,
        seconds: r.len() as f64 / r.rate as f64,
    });
    let settings = DspSettings {
        upsample: upsample_khz.map_or(Upsample::Off, |k| Upsample::UpTo(k * 1000.0)),
        upsample_filter: filter,
        eq: None,
        eq_bypass: false,
        convolution: ir.map(Convolution),
        crossfeed: crossfeed
            .and_then(|k| CROSSFEED_LEVELS.iter().find(|(key, _)| *key == k).map(|(_, l)| *l)),
        lossy: LossySettings { headroom: headroom.unwrap_or(false), restore_highs: restore_highs.unwrap_or(false) },
        level: LevelSettings {
            mode: match level.as_deref() {
                Some("track") => LevelMode::Track,
                Some("album") => LevelMode::Album,
                _ => LevelMode::Off,
            },
            headroom: peak_headroom.unwrap_or(false),
        },
        // What the user wants: each playback runs what its device can take of it (`dsp_for`).
        dsd_out,
    };
    let key = settings_key_of_uid(&state, &device_uid);
    state.dsp.lock().unwrap().insert(key.clone(), settings);
    // (Under play_lock: what plays can't change between the look and the restart.)
    let _one = state.play_lock.lock().unwrap();
    let here = state.current.lock().unwrap().as_ref().is_some_and(|c| c.key == key);
    if !here {
        return Ok(DspResult { signal: None, convolution: ir_dto });
    }
    // The UI's own choice now: an earlier automatic step is no longer news.
    *state.filter_step.lock().unwrap() = None;
    *state.dsd_step.lock().unwrap() = None;
    // What was lined up carries a gain from the old settings: it is opened again.
    state.lined_up.lock().unwrap().clear();
    let signal = restart_locked(&state, None)?;
    Ok(DspResult { signal, convolution: ir_dto })
}

/// The settings key of the output `uid`. On Android that takes the device itself; one that
/// has just gone away is known still if it is what plays. Failing that, the uid: settings kept
/// under it are never read (a playback looks its device's key up), and no error spoils the
/// rest of what the UI sent.
fn settings_key_of_uid(state: &AppState, uid: &str) -> String {
    if let Some(key) = device_key::of_uid(uid) {
        return key;
    }
    if let Ok(device) = find_device(uid) {
        return device_key::of(&device);
    }
    let playing = state.current.lock().unwrap().as_ref().filter(|c| c.device_uid == uid).map(|c| c.key.clone());
    playing.unwrap_or_else(|| uid.to_string())
}

/// Restart what plays in place (same position and pause state) with the current settings.
/// Returns the new signal path, or None when nothing is loaded.
fn restart(state: &AppState) -> CmdResult<Option<SignalDto>> {
    let _one = state.play_lock.lock().unwrap();
    restart_locked(state, None)
}

/// [`restart`], at `at` seconds instead of where it plays. Call with `play_lock` held.
fn restart_locked(state: &AppState, at: Option<f64>) -> CmdResult<Option<SignalDto>> {
    let resume = {
        let player = state.player.lock().unwrap();
        player.as_ref().map(|p| (at.unwrap_or_else(|| p.position_secs()), p.is_paused(), p.is_finished()))
    };
    let mut current = state.current.lock().unwrap();
    let (Some((pos, paused, false)), Some(cur)) = (resume, current.as_mut()) else { return Ok(None) };
    let device = find_device(&cur.device_uid)?;
    let (settings, dsd_out_note) = dsp_for(state, &device, cur.exclusive, cur.dop);
    // A DSD file may have to switch between DoP and PCM (the EQ and friends need PCM).
    let mut mode = DsdMode::Pcm;
    if dsd::is_dsd_path(&cur.path) {
        let (m, note) = dsd_mode(&cur.path, &device, cur.exclusive, cur.dop, &settings);
        mode = m;
        if (mode == DsdMode::Dop) != cur.track.is_dop() {
            cur.track = open_for_at(&cur.path, mode, pos)?;
        }
        cur.dsd_note = note;
    }
    // A long file holds only part of itself in memory: far from it, open it anew there.
    let frame = (pos.max(0.0) * cur.track.sample_rate as f64) as usize;
    if !cur.track.can_seek_to(frame) {
        cur.track = open_for_at(&cur.path, mode, pos)?;
    }
    // The settings may be new, or the track just opened: its gain under them. (Measured on
    // the spot only if it is in memory whole already.)
    set_level(&settings, &cur.path, &cur.track, Duration::ZERO);
    // So does what is lined up to follow it: it joins the new player as it is.
    for next in state.lined_up.lock().unwrap().iter() {
        set_level(&settings, std::path::Path::new(&next.path), &next.track, Duration::ZERO);
    }
    let opts = PlayOptions { exclusive: cur.exclusive, dsp: settings, start_at: pos, start_paused: paused, fades: !state.fades_off.load(Ordering::Relaxed) };
    let mut signal = start(state, &device, cur.track.clone(), opts, dsd_out_note)?;
    signal.dsd_note = cur.dsd_note.clone();
    cur.dsd_out_note = signal.dsd_out_note.clone();
    if let Some(now) = state.now.lock().unwrap().as_mut() {
        now.signal = signal.clone();
    }
    Ok(Some(signal))
}

/// How often the queue thread asks whether the processing is too heavy.
const STEP_CHECK_EVERY: Duration = Duration::from_secs(3);

/// The processor can't keep up with what plays: put the next cheaper setting in the settings
/// (see `dsd_out::next_step`: a lighter DSD output, a shorter upsampling filter — 2^20 → 2^16
/// taps → the standard resampler — or no DSD output), restart in place with it, and record the
/// step for the UI to show and adopt. Runs on the queue thread, so it works with no window to
/// ask; never while a track is starting or restarting.
fn step_down(state: &AppState) {
    let playing = {
        let Ok(_one) = state.play_lock.try_lock() else { return };
        let player = state.player.lock().unwrap();
        let Some(p) = player.as_ref() else { return };
        // A debug build run with BITSPRING_TRACE says what the filter and the encoder are doing.
        if cfg!(debug_assertions) && std::env::var_os("BITSPRING_TRACE").is_some() {
            eprintln!(
                "filter {:?}, dsd out {:?}, {} → {} Hz, load {:?}, underruns {}, at {:.1} s",
                p.path.upsample_filter,
                p.path.dsd_out,
                p.path.source_rate,
                p.path.device_rate,
                p.load(),
                p.underruns(),
                p.position_secs()
            );
        }
        let heavy = p.path.upsample_filter.is_some() || p.path.dsd_out.is_some();
        if p.is_finished() || !upsampling::should_step_down(p.load(), p.underruns(), heavy) {
            return;
        }
        (p.path.upsample_filter, p.path.dsd_out)
    };
    let (filter, out) = playing;
    // From what plays: the device may run a lower DSD rate than the one asked for.
    let Some(step) = dsd_out::next_step(out, filter) else { return };
    // A filter is designed (or fetched) with no lock held.
    if let dsd_out::Step::Filter(Some(spec)) = step {
        drop(upsample::filter(spec));
    }
    let Ok(_one) = state.play_lock.try_lock() else { return };
    // Still the same playing, and still what the settings ask for? Settings sent meanwhile
    // restart playback themselves.
    let same = state.player.lock().unwrap().as_ref().is_some_and(|p| (p.path.upsample_filter, p.path.dsd_out) == playing);
    // The step goes into the settings of the output that plays, and only its.
    let Some(device_key) = state.current.lock().unwrap().as_ref().map(|c| c.key.clone()) else { return };
    {
        let mut all = state.dsp.lock().unwrap();
        // (No settings of its own: it plays the defaults, which have nothing to step down.)
        let Some(dsp) = all.get_mut(&device_key).filter(|_| same) else { return };
        match step {
            dsd_out::Step::Filter(next) => {
                if dsp.upsample_filter.map(|f| f.effective()) != filter {
                    return;
                }
                dsp.upsample_filter = next;
            }
            dsd_out::Step::Dsd(next) => {
                if !out.is_some_and(|o| dsd_out::asks_for(dsp.dsd_out, o)) {
                    return;
                }
                dsp.dsd_out = next;
            }
        }
    }
    match step {
        dsd_out::Step::Filter(next) => {
            let (filter, taps) = upsampling::names(next);
            let seq = state.filter_step_seq.fetch_add(1, Ordering::AcqRel) + 1;
            *state.filter_step.lock().unwrap() = Some(upsampling::FilterStep { seq, filter, taps, device_key });
        }
        dsd_out::Step::Dsd(next) => {
            let (dsd_out, order) = dsd_out::names(next);
            let seq = state.dsd_step_seq.fetch_add(1, Ordering::AcqRel) + 1;
            *state.dsd_step.lock().unwrap() = Some(dsd_out::DsdStep { seq, dsd_out, order, device_key });
        }
    }
    // (A failure shows as the next status's error, as for any restart.)
    let _ = restart_locked(state, None);
}

/// The EQ settings changed: apply them to what plays — live when the engine can take them
/// (the usual case: an edit, another preset of the same kind, compare), else by a restart.
/// Returns the new signal path when it changed.
fn apply_eq(state: &AppState) -> CmdResult<Option<SignalDto>> {
    let Some(key) = state.current.lock().unwrap().as_ref().map(|c| c.key.clone()) else {
        return Ok(None);
    };
    let (want, bypass) = {
        let s = state.eq.lock().unwrap();
        (s.effective(&key), s.bypass)
    };
    {
        let mut player = state.player.lock().unwrap();
        let Some(p) = player.as_mut() else { return Ok(None) };
        match &want {
            None if p.path.eq.is_none() => return Ok(None),
            Some(eq) if p.path.eq.is_some() && p.update_eq(eq, bypass) => {
                // Same chain, new settings: only the name shown can change.
                if p.path.eq.as_deref() == Some(eq.name.as_str()) {
                    return Ok(None);
                }
                p.path.eq = Some(eq.name.clone());
                let mut signal = SignalDto::from(&p.path);
                drop(player);
                let mut now = state.now.lock().unwrap();
                if let Some(now) = now.as_mut() {
                    signal.dsd_note = now.signal.dsd_note.clone();
                    signal.dsd_out_note = now.signal.dsd_out_note.clone();
                    now.signal = signal.clone();
                }
                return Ok(Some(signal));
            }
            _ => {}
        }
    }
    restart(state)
}

/// iPhone: let the page run under the status bar and home indicator (it pads itself with
/// env(safe-area-inset-*)), and keep the page itself still — only its panels scroll.
#[cfg(target_os = "ios")]
fn ios_edge_to_edge(webview: *mut std::ffi::c_void) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    const NEVER: isize = 2; // UIScrollViewContentInsetAdjustmentNever
    // SAFETY: `webview` is the live WKWebView of the main window, used on the main thread.
    unsafe {
        let wk = webview as *mut AnyObject;
        if wk.is_null() {
            return;
        }
        let scroll: *mut AnyObject = msg_send![wk, scrollView];
        if scroll.is_null() {
            return;
        }
        let _: () = msg_send![scroll, setContentInsetAdjustmentBehavior: NEVER];
        let _: () = msg_send![scroll, setScrollEnabled: false];
        let _: () = msg_send![scroll, setBounces: false];
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        // The website link in Settings opens in the browser (capabilities allow bitspring.app only).
        .plugin(tauri_plugin_opener::init());
    #[cfg(all(feature = "updater", desktop))]
    let builder = builder.plugin(tauri_plugin_updater::Builder::new().build());
    builder
        .manage(AppState::default())
        .setup(|app| {
            use tauri::Manager;
            // First: whatever breaks from here on leaves a report (on this device only).
            crash::install(app.handle());
            // (Before the library: its first scan looks the favourite albums up when it ends.)
            app.manage(marks::Marks::load(app.handle()));
            let lib = library::Library::load(app.handle());
            app.manage(lib);
            app.manage(playlists::Playlists::default());
            app.manage(quality::Quality::load(app.handle()));
            app.manage(loudness::Loudness::load(app.handle()));
            app.manage(remote::Remote::load(app.handle()));
            app.manage(abx::Abx::default());
            app.manage(ripcheck::RipCheck::load(app.handle()));
            *app.state::<AppState>().eq.lock().unwrap() = eq::EqStore::load(app.handle());
            stream::init(app.handle());
            // Android: the main window is made here rather than from the config (see
            // tauri.android.conf.json), so a streaming sign-in can run in it.
            #[cfg(target_os = "android")]
            {
                let config = app.config().app.windows.iter().find(|w| w.label == "main").cloned();
                if let Some(config) = config {
                    let builder = tauri::WebviewWindowBuilder::from_config(app.handle(), &config)?;
                    #[cfg(feature = "streaming")]
                    let builder = builder.on_navigation(|u| stream::main_navigation(u));
                    builder.build()?;
                }
            }
            #[cfg(target_os = "ios")]
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.with_webview(|wv| ios_edge_to_edge(wv.inner()));
            }
            let _ = APP.set(app.handle().clone());
            #[cfg(system_media)]
            media::install(app.handle());
            #[cfg(target_os = "linux")]
            pad::install(app.handle());
            let handle = app.handle().clone();
            std::thread::Builder::new().name("hifi-queue".into()).spawn(move || {
                let mut ended: Option<std::time::Instant> = None;
                let mut load_checked = std::time::Instant::now();
                loop {
                    std::thread::sleep(Duration::from_millis(40));
                    let state = handle.state::<AppState>();
                    if load_checked.elapsed() >= STEP_CHECK_EVERY {
                        load_checked = std::time::Instant::now();
                        step_down(&state);
                    }
                    follow_queue(&state);
                    if advance(&state) {
                        let since = *ended.get_or_insert_with(std::time::Instant::now);
                        if since.elapsed() >= RELEASE_AFTER {
                            release_if_still_finished(&state);
                            ended = None;
                        }
                    } else {
                        ended = None;
                    }
                    #[cfg(system_media)]
                    {
                        let rev = state.now_rev.load(Ordering::Acquire);
                        let now = state.player.lock().unwrap().as_ref().map(|p| (p.position_secs(), p.is_paused() || p.is_finished()));
                        if let Some((position, paused)) = now {
                            media::sync(&handle, rev, position, paused);
                        }
                    }
                }
            })?;
            Ok(())
        })
        // Album art for the library grid: cover://localhost/<album id>
        .register_uri_scheme_protocol("cover", |ctx, request| {
            use tauri::Manager;
            let app = ctx.app_handle();
            app.state::<library::Library>().cover_response(app, request.uri().path())
        })
        .invoke_handler(tauri::generate_handler![
            list_devices,
            system_locales,
            set_menu_labels,
            update::update_check,
            update::update_install,
            update::update_progress,
            expand_paths,
            play_file,
            set_queue,
            set_play_modes,
            now_playing,
            channel_test,
            status,
            meter,
            set_paused,
            seek,
            stop,
            set_fades,
            set_dsp,
            eq::eq_state,
            eq::eq_save,
            eq::eq_delete,
            eq::eq_adopt,
            eq::eq_set_bypass,
            eq::eq_bind_device,
            eq::eq_import_file,
            eq::eq_import_text,
            eq::eq_headphones,
            eq::eq_add_headphone,
            eq::eq_catalog,
            eq::eq_add_catalog,
            eq::eq_export,
            eq::eq_export_text,
            library::library_status,
            library::library_albums,
            library::library_tracks,
            library::library_add_folder,
            library::library_remove_folder,
            library::library_pick_folder,
            library::library_skip_folder,
            library::library_unskip_folder,
            library::library_subfolders,
            library::library_rescan,
            library::import_files,
            library::edit_tags,
            lyrics::lyrics,
            output_volume,
            set_output_volume,
            remote::remote_status,
            remote::remote_enable,
            remote::remote_new_address,
            quality::quality_cached,
            quality::quality_check,
            loudness::loudness_status,
            loudness::loudness_analyze,
            loudness::loudness_write_tags,
            loudness::dynamics,
            marks::marks,
            marks::mark_favorite,
            marks::mark_rating,
            marks::mark_album,
            ripcheck::rip_cached,
            ripcheck::rip_progress,
            ripcheck::rip_check,
            abx::abx_start,
            abx::abx_listen,
            abx::abx_set_trials,
            abx::abx_answer,
            abx::abx_again,
            abx::abx_stop,
            crash::crash_report,
            crash::crash_dismiss,
            playlists::playlists_list,
            playlists::playlist_get,
            playlists::playlist_create,
            playlists::playlist_update,
            playlists::playlist_add,
            playlists::playlist_delete,
            playlists::playlist_import_file,
            playlists::playlist_import_text,
            playlists::playlist_export,
            stream::streaming_status,
            stream::streaming_set_quality,
            stream::streaming_login,
            stream::streaming_logout,
            stream::streaming_search,
            stream::streaming_album,
            stream::streaming_favorites,
            stream::streaming_playlists,
            stream::streaming_playlist,
            stream::streaming_import_playlist,
            stream::streaming_find_missing
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // Release the DAC (hog mode, rate, format) before the process goes away.
            if let tauri::RunEvent::Exit = event {
                use tauri::Manager;
                drop(app.state::<AppState>().player.lock().unwrap().take());
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An output the UI has sent nothing for plays as the UI shows a new one: the two headroom
    /// switches on, nothing that changes the signal (bit-perfect where the source allows).
    #[test]
    fn an_output_without_settings_plays_the_uis_defaults() {
        let d = default_dsp();
        assert!(d.lossy.headroom && !d.lossy.restore_highs);
        assert!(d.level.headroom);
        assert_eq!(d.level.mode, LevelMode::Off);
        assert!(!d.reshapes() && !d.is_active());
        assert_eq!((d.upsample_filter, d.dsd_out, d.crossfeed), (None, None, None));
        assert!(AppState::default().dsp.lock().unwrap().is_empty());
    }

    /// Levelling by album holds while albums play through; a shuffled queue is levelled
    /// track by track. Nothing else in the settings moves, and "by track" and "off" stay.
    #[test]
    fn a_shuffled_queue_is_levelled_by_track() {
        let with = |mode| DspSettings { level: LevelSettings { mode, headroom: true }, ..Default::default() };
        let album = with(LevelMode::Album);
        assert_eq!(level_for(&album, false).level.mode, LevelMode::Album);
        let shuffled = level_for(&album, true);
        assert_eq!(shuffled.level.mode, LevelMode::Track);
        assert!(shuffled.level.headroom);
        assert_eq!(level_for(&with(LevelMode::Track), true).level.mode, LevelMode::Track);
        assert_eq!(level_for(&with(LevelMode::Off), true).level.mode, LevelMode::Off);
    }
}
