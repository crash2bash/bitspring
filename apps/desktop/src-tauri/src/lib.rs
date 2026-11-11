//! Desktop app backend: a thin command layer over `hifi-engine`.
//!
//! The UI never touches audio directly; it calls these commands and polls `status`.
//! All playback state lives here, in Rust.

#[cfg(target_os = "android")]
mod android_media;
#[cfg(target_os = "ios")]
mod ios_media;
mod eq;
mod library;
mod playlists;
/// Qobuz and TIDAL: the real thing outside the App Store, a stand-in inside it.
#[cfg_attr(all(feature = "streaming", not(target_os = "ios")), path = "stream_on.rs")]
#[cfg_attr(not(all(feature = "streaming", not(target_os = "ios"))), path = "stream_off.rs")]
mod stream;
/// Updating from GitHub Releases: the real thing in website builds, nothing in App Store ones.
#[cfg_attr(all(feature = "updater", not(target_os = "ios")), path = "update_on.rs")]
#[cfg_attr(not(all(feature = "updater", not(target_os = "ios"))), path = "update_off.rs")]
mod update;
/// Lock screen / notification player and remote controls, per phone OS.
#[cfg(target_os = "android")]
use android_media as media;
#[cfg(target_os = "ios")]
use ios_media as media;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::Duration;

use base64::Engine as _;
use hifi_engine::output::{self, DeviceInfo, PlayOptions, Player, SignalPath};
use hifi_engine::dsp::crossfeed::CrossfeedLevel;
use hifi_engine::dsp::fir::ImpulseResponse;
use hifi_engine::dsp::lossy::LossySettings;
use hifi_engine::dsp::{Convolution, DspSettings, Upsample};
use hifi_engine::dsd::{self, DsdMode};
use hifi_engine::{Track, UserError, open_file, testsignal};
use serde::Serialize;
use tauri::State;

/// What is loaded right now, so processing changes can restart it in place.
struct Current {
    track: Arc<Track>,
    path: PathBuf,
    device_uid: String,
    exclusive: bool,
    /// The user wants DSD sent as DoP on this device.
    dop: bool,
    /// Why a DSD file plays as PCM although DoP was asked for.
    dsd_note: Option<DsdNote>,
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
    /// The device doesn't run at the PCM rate DoP needs (`khz`) for this DSD rate (`dsd`).
    Rate { device: String, khz: f64, dsd: String },
}

/// The queue stopped at a track it couldn't play: which one, and the engine's reason.
#[derive(Serialize, Clone, Debug)]
struct AdvanceError {
    name: String,
    error: UserError,
}

/// The play queue, mirrored from the UI so the next track starts from Rust — on time, and
/// also while the UI sleeps (a locked iPhone suspends the web view, not the audio).
#[derive(Default)]
struct Queue {
    paths: Vec<String>,
    index: Option<usize>,
}

#[derive(Default)]
struct AppState {
    player: Mutex<Option<Player>>,
    current: Mutex<Option<Current>>,
    /// Processing for every output; the EQ in it comes from `eq`, per output.
    dsp: Mutex<DspSettings>,
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
    /// Why the queue stopped at a track it couldn't play.
    advance_error: Mutex<Option<AdvanceError>>,
    /// The last status sent to the UI, answered again while a track is starting.
    last_status: Mutex<Option<StatusDto>>,
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
}

impl From<&DeviceInfo> for DeviceDto {
    fn from(d: &DeviceInfo) -> Self {
        Self {
            uid: d.uid.clone(),
            name: d.name.trim().to_string(),
            transport: format!("{:?}", d.transport),
            channels: d.output_channels,
            rates: hifi_engine::rate::candidate_rates(&d.rates),
            nominal_rate: d.nominal_rate,
            is_default: d.is_default,
            bit_perfect_capable: d.transport.can_be_bit_perfect(),
            max_dop: d
                .transport
                .can_be_bit_perfect()
                .then(|| [8u32, 4, 2, 1].into_iter().find(|m| d.device.supports_dop(dsd::DSD64 * m)).map(|m| 64 * m))
                .flatten(),
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
    /// DSD source: its 1-bit rate (Hz).
    dsd_rate: Option<u32>,
    /// DSD goes out as DoP (false with `dsd_rate` set = converted to PCM).
    dop: bool,
    /// Why DoP was asked for but this DSD file plays as PCM.
    dsd_note: Option<DsdNote>,
    bit_perfect: bool,
}

impl From<&SignalPath> for SignalDto {
    fn from(p: &SignalPath) -> Self {
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
            dsd_rate: p.dsd.map(|d| d.rate),
            dop: p.dsd.is_some_and(|d| d.dop),
            dsd_note: None,
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

fn find_device(uid: &str) -> CmdResult<DeviceInfo> {
    output::output_devices()
        .map_err(err)?
        .into_iter()
        .find(|d| d.uid == uid)
        .ok_or(UserError::DeviceNotFound)
}

/// Stop whatever plays and start `track` on `device`. The old player is dropped *first*, so
/// its hog mode is released and the device restored before we claim it again.
fn start(state: &AppState, device: &DeviceInfo, track: Arc<Track>, opts: PlayOptions) -> CmdResult<SignalDto> {
    let mut slot = state.player.lock().unwrap();
    drop(slot.take());
    let player = Player::start(device.device.clone(), track, &opts).map_err(err)?;
    let signal = SignalDto::from(&player.path);
    *slot = Some(player);
    Ok(signal)
}

/// How to carry a DSD file to this device. DoP only when the user switched it on for the
/// device, the device can take it, we have exclusive access, and no EQ/crossfeed has to run
/// (those need PCM). Returns the mode and, for a PCM fallback, the reason.
fn dsd_mode(path: &std::path::Path, device: &DeviceInfo, exclusive: bool, dop: bool, dsp: &DspSettings) -> (DsdMode, Option<DsdNote>) {
    if !dop || !dsd::is_dsd_path(path) {
        return (DsdMode::Pcm, None);
    }
    let rate = match dsd::probe(path) {
        Ok(h) => h.rate,
        Err(_) => return (DsdMode::Pcm, None),
    };
    let note = if !exclusive {
        Some(DsdNote::Exclusive)
    } else if output::dop_blocker().is_some() {
        Some(DsdNote::Volume)
    } else if dsp.eq.is_some() || dsp.convolution.is_some() || dsp.crossfeed.is_some() {
        Some(DsdNote::Dsp)
    } else if !device.device.supports_dop(rate) {
        Some(DsdNote::Rate {
            device: device.name.trim().to_string(),
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
    match path.to_str().filter(|p| stream::is_uri(p)) {
        Some(uri) => stream::open(uri).map_err(|e| UserError::of(&e)),
        None => open_file(path, mode).map_err(err),
    }
}

/// The OS's preferred UI languages, best first (BCP 47, e.g. "de-DE", "zh-Hant-TW"). The web
/// view's navigator.languages can't be trusted for this: WKWebView reports the languages the
/// app bundle declares rather than the user's own.
#[tauri::command]
fn system_locales() -> Vec<String> {
    // Android: the app's own configuration, which includes a language picked for this app in
    // Settings → Apps → Hifi → Language (sys-locale reads only the system-wide one).
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

const AUDIO_EXTENSIONS: [&str; 12] = ["flac", "wav", "aif", "aiff", "m4a", "alac", "mp3", "ogg", "caf", "mka", "dsf", "dff"];

/// Turn dropped files and folders into a flat, ordered list of audio files.
/// Folders are walked recursively and sorted by path, so an album lands in track order.
#[tauri::command(async)]
fn expand_paths(paths: Vec<String>) -> Vec<String> {
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
    out.into_iter().map(|p| p.to_string_lossy().into_owned()).collect()
}

#[tauri::command(async)]
fn list_devices() -> CmdResult<Vec<DeviceDto>> {
    Ok(output::output_devices().map_err(err)?.iter().map(DeviceDto::from).collect())
}

/// The processing for a playback on `device_uid`: the shared settings plus that output's EQ.
fn dsp_for(state: &AppState, device_uid: &str) -> DspSettings {
    let mut dsp = state.dsp.lock().unwrap().clone();
    let eq = state.eq.lock().unwrap();
    dsp.eq = eq.effective(device_uid);
    dsp.eq_bypass = eq.bypass;
    dsp
}

/// Start `file` (already opened as `track`) and record it as what plays now.
fn begin(
    state: &AppState,
    device: &DeviceInfo,
    file: PathBuf,
    track: Arc<Track>,
    exclusive: bool,
    start_at: f64,
    start_paused: bool,
    dop: bool,
    dsd_note: Option<DsdNote>,
) -> CmdResult<NowPlayingDto> {
    let dsp = dsp_for(state, &device.uid);
    let meta = track.meta.clone();
    let (channels, duration) = (track.channels, track.duration_secs());
    let opts = PlayOptions { exclusive, dsp, start_at, start_paused };
    #[cfg(mobile)]
    let lock_screen = media::TrackInfo {
        title: meta.title.clone().unwrap_or_else(|| file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()),
        artist: meta.artist.clone(),
        album: meta.album.clone(),
        duration,
        cover: meta.cover.as_ref().map(|(_, b)| b.clone()),
    };
    let mut signal = start(state, device, track.clone(), opts)?;
    signal.dsd_note = dsd_note.clone();
    let path = file.to_string_lossy().into_owned();
    let file_stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    *state.current.lock().unwrap() =
        Some(Current { track, path: file, device_uid: device.uid.clone(), exclusive, dop, dsd_note });
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
        device_name: device.name.trim().to_string(),
        signal,
        path,
    };
    *state.now.lock().unwrap() = Some(now.clone());
    *state.advance_error.lock().unwrap() = None;
    // Under play_lock: nobody else bumps it between the load above and here.
    state.now_rev.store(rev, Ordering::Release);
    #[cfg(mobile)]
    if let Some(app) = APP.get() {
        media::set_track(app, rev, lock_screen);
    }
    Ok(now)
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
    let dsp = dsp_for(&state, &device.uid);
    let (mode, dsd_note) = dsd_mode(&file, &device, exclusive, dop, &dsp);
    // Only the headers are parsed here; the decoder keeps filling RAM in the background
    // while playback starts. Still off the UI/IPC threads: opening may touch a slow disk.
    let track = {
        let file = file.clone();
        tauri::async_runtime::spawn_blocking(move || open_for(&file, mode)).await.map_err(err)??
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
    let result = find_device(&device_uid).and_then(|device| {
        let dsp = dsp_for(state, &device.uid);
        let (mode, note) = dsd_mode(&file, &device, exclusive, dop, &dsp);
        let track = open_for(&file, mode)?;
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

/// At the end of a track, start the next one in the queue on the same output. True when
/// the last track has played out and nothing follows (the player sits finished).
fn advance(state: &AppState) -> bool {
    let _one = state.play_lock.lock().unwrap();
    if !state.player.lock().unwrap().as_ref().is_some_and(|p| p.is_finished()) {
        return false;
    }
    let next = state.queue.lock().unwrap().index.map(|i| i + 1);
    match next.filter(|&i| i < state.queue.lock().unwrap().paths.len()) {
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

/// Next (+1) or previous (−1) track, for controls outside the UI (lock screen, headphones).
/// "Previous" more than 3 s into a track restarts it, like the UI's button.
#[cfg_attr(desktop, allow(dead_code))]
pub(crate) fn jump(state: &AppState, delta: i64) {
    let _one = state.play_lock.lock().unwrap();
    if delta < 0
        && let Some(p) = state.player.lock().unwrap().as_ref()
        && p.position_secs() > 3.0
    {
        p.seek(0.0);
        return;
    }
    let (index, len) = {
        let q = state.queue.lock().unwrap();
        (q.index, q.paths.len())
    };
    let Some(i) = index.and_then(|i| i.checked_add_signed(delta as isize)).filter(|&i| i < len) else { return };
    play_queue_at(state, i);
}

#[tauri::command(async)]
fn channel_test(state: State<'_, AppState>, device_uid: String) -> CmdResult<SignalDto> {
    let device = find_device(&device_uid)?;
    // Play at a rate the device already runs at, so the test itself switches nothing.
    let rate = device.nominal_rate.round() as u32;
    // The test bypasses processing: crossfeed would blur exactly what it checks.
    let _one = state.play_lock.lock().unwrap();
    *state.current.lock().unwrap() = None;
    start(&state, &device, Arc::new(testsignal::channel_test(rate)), PlayOptions::default())
}

impl StatusDto {
    /// Nothing loaded.
    fn idle(queue_index: Option<usize>, now_rev: u64, advance_error: Option<AdvanceError>, system_volume: Option<f32>) -> Self {
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
        }
    }
}

#[tauri::command]
fn status(state: State<'_, AppState>) -> StatusDto {
    let queue_index = state.queue.lock().unwrap().index;
    let now_rev = state.now_rev.load(Ordering::Acquire);
    let advance_error = state.advance_error.lock().unwrap().clone();
    let system_volume = output::system_volume();
    let Some(player) = player_now(&state) else {
        // A track is starting: the last answer, as waiting for audio (and not finished: what
        // was finished is being replaced).
        let last = state.last_status.lock().unwrap().clone();
        return match last {
            Some(s) => StatusDto { buffering: true, finished: false, queue_index, now_rev, advance_error, system_volume, ..s },
            None => StatusDto { buffering: true, ..StatusDto::idle(queue_index, now_rev, advance_error, system_volume) },
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
        },
        None => StatusDto::idle(queue_index, now_rev, advance_error, system_volume),
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
    if let Some(p) = state.player.lock().unwrap().as_ref() {
        p.seek(seconds);
    }
}

#[tauri::command]
fn stop(state: State<'_, AppState>) {
    let _one = state.play_lock.lock().unwrap();
    drop(state.player.lock().unwrap().take());
    *state.current.lock().unwrap() = None;
    *state.now.lock().unwrap() = None;
    #[cfg(mobile)]
    if let Some(app) = APP.get() {
        media::clear(app);
    }
}

/// Replace the processing settings. Everything off = bit-perfect. If a track is loaded it
/// continues from the same position (and pause state) with the new chain.
#[tauri::command]
async fn set_dsp(
    app: tauri::AppHandle,
    upsample_khz: Option<f64>,
    crossfeed: Option<String>,
    convolution: Option<String>,
    headroom: Option<bool>,
    restore_highs: Option<bool>,
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
    let ir_dto = ir.as_ref().map(|r| IrDto {
        name: r.name.clone(),
        channels: r.channels.len(),
        rate: r.rate,
        seconds: r.len() as f64 / r.rate as f64,
    });
    *state.dsp.lock().unwrap() = DspSettings {
        upsample: upsample_khz.map_or(Upsample::Off, |k| Upsample::UpTo(k * 1000.0)),
        eq: None,
        eq_bypass: false,
        convolution: ir.map(Convolution),
        crossfeed: crossfeed
            .and_then(|k| CROSSFEED_LEVELS.iter().find(|(key, _)| *key == k).map(|(_, l)| *l)),
        lossy: LossySettings { headroom: headroom.unwrap_or(false), restore_highs: restore_highs.unwrap_or(false) },
    };
    let signal = restart(&state)?;
    Ok(DspResult { signal, convolution: ir_dto })
}

/// Restart what plays in place (same position and pause state) with the current settings.
/// Returns the new signal path, or None when nothing is loaded.
fn restart(state: &AppState) -> CmdResult<Option<SignalDto>> {
    let _one = state.play_lock.lock().unwrap();
    let resume = {
        let player = state.player.lock().unwrap();
        player.as_ref().map(|p| (p.position_secs(), p.is_paused(), p.is_finished()))
    };
    let mut current = state.current.lock().unwrap();
    let (Some((pos, paused, false)), Some(cur)) = (resume, current.as_mut()) else { return Ok(None) };
    let device = find_device(&cur.device_uid)?;
    let settings = dsp_for(state, &cur.device_uid);
    // A DSD file may have to switch between DoP and PCM (the EQ and friends need PCM).
    if dsd::is_dsd_path(&cur.path) {
        let (mode, note) = dsd_mode(&cur.path, &device, cur.exclusive, cur.dop, &settings);
        if (mode == DsdMode::Dop) != cur.track.is_dop() {
            cur.track = open_for(&cur.path, mode)?;
        }
        cur.dsd_note = note;
    }
    let opts = PlayOptions { exclusive: cur.exclusive, dsp: settings, start_at: pos, start_paused: paused };
    let mut signal = start(state, &device, cur.track.clone(), opts)?;
    signal.dsd_note = cur.dsd_note.clone();
    if let Some(now) = state.now.lock().unwrap().as_mut() {
        now.signal = signal.clone();
    }
    Ok(Some(signal))
}

/// The EQ settings changed: apply them to what plays — live when the engine can take them
/// (the usual case: an edit, another preset of the same kind, compare), else by a restart.
/// Returns the new signal path when it changed.
fn apply_eq(state: &AppState) -> CmdResult<Option<SignalDto>> {
    let Some(device_uid) = state.current.lock().unwrap().as_ref().map(|c| c.device_uid.clone()) else {
        return Ok(None);
    };
    let (want, bypass) = {
        let s = state.eq.lock().unwrap();
        (s.effective(&device_uid), s.bypass)
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
    let builder = tauri::Builder::default().plugin(tauri_plugin_dialog::init());
    #[cfg(all(feature = "updater", desktop))]
    let builder = builder.plugin(tauri_plugin_updater::Builder::new().build());
    builder
        .manage(AppState::default())
        .setup(|app| {
            use tauri::Manager;
            let lib = library::Library::load(app.handle());
            app.manage(lib);
            app.manage(playlists::Playlists::default());
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
            #[cfg(mobile)]
            media::install(app.handle());
            let handle = app.handle().clone();
            std::thread::Builder::new().name("hifi-queue".into()).spawn(move || {
                let mut ended: Option<std::time::Instant> = None;
                loop {
                    std::thread::sleep(Duration::from_millis(40));
                    let state = handle.state::<AppState>();
                    if advance(&state) {
                        let since = *ended.get_or_insert_with(std::time::Instant::now);
                        if since.elapsed() >= RELEASE_AFTER {
                            release_if_still_finished(&state);
                            ended = None;
                        }
                    } else {
                        ended = None;
                    }
                    #[cfg(mobile)]
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
            now_playing,
            channel_test,
            status,
            meter,
            set_paused,
            seek,
            stop,
            set_dsp,
            eq::eq_state,
            eq::eq_save,
            eq::eq_delete,
            eq::eq_activate,
            eq::eq_set_enabled,
            eq::eq_set_bypass,
            eq::eq_bind_device,
            eq::eq_import_file,
            eq::eq_import_text,
            eq::eq_export,
            eq::eq_export_text,
            library::library_status,
            library::library_albums,
            library::library_tracks,
            library::library_add_folder,
            library::library_remove_folder,
            library::library_rescan,
            library::import_files,
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
