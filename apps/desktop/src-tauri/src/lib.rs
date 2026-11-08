//! Desktop app backend: a thin command layer over `hifi-engine`.
//!
//! The UI never touches audio directly; it calls these commands and polls `status`.
//! All playback state lives here, in Rust.

mod library;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use hifi_engine::output::{self, DeviceInfo, PlayOptions, Player, SignalPath};
use hifi_engine::dsp::crossfeed::CrossfeedLevel;
use hifi_engine::dsp::eq::{EqProfile, FilterKind};
use hifi_engine::dsp::lossy::LossySettings;
use hifi_engine::dsp::{DspSettings, Upsample};
use hifi_engine::dsd::{self, DsdMode};
use hifi_engine::{Track, open_file, testsignal};
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
    dsd_note: Option<String>,
}

#[derive(Default)]
struct AppState {
    player: Mutex<Option<Player>>,
    current: Mutex<Option<Current>>,
    dsp: Mutex<DspSettings>,
}

const CROSSFEED_LEVELS: [(&str, CrossfeedLevel); 3] = [
    ("light", CrossfeedLevel::JAN_MEIER),
    ("medium", CrossfeedLevel::CHU_MOY),
    ("strong", CrossfeedLevel::DEFAULT),
];

fn crossfeed_key(level: CrossfeedLevel) -> String {
    CROSSFEED_LEVELS.iter().find(|(_, l)| *l == level).map_or("custom", |(k, _)| k).to_string()
}

type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
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

#[derive(Serialize)]
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
    dsd_note: Option<String>,
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

#[derive(Serialize)]
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
}

#[derive(Serialize)]
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
    /// Decoding stopped early (corrupt file, dropped connection).
    error: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BandDto {
    kind: &'static str,
    freq: f64,
    gain: f64,
    q: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EqDto {
    name: String,
    preamp: f64,
    bands: Vec<BandDto>,
    /// [frequency Hz, gain dB] from 20 Hz to 20 kHz, log-spaced, for drawing.
    curve: Vec<[f64; 2]>,
}

impl From<&EqProfile> for EqDto {
    fn from(p: &EqProfile) -> Self {
        Self {
            name: p.name.clone(),
            preamp: p.preamp_db,
            bands: p
                .bands
                .iter()
                .map(|b| BandDto {
                    kind: match b.kind {
                        FilterKind::Peak => "peak",
                        FilterKind::LowShelf => "lowShelf",
                        FilterKind::HighShelf => "highShelf",
                    },
                    freq: b.freq,
                    gain: b.gain_db,
                    q: b.q,
                })
                .collect(),
            curve: (0..=96)
                .map(|i| {
                    let f = 20.0 * 1000f64.powf(i as f64 / 96.0);
                    [f, p.response_db(f, 96_000.0)]
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DspResult {
    /// New signal path if something was playing and got restarted with the new settings.
    signal: Option<SignalDto>,
    eq: Option<EqDto>,
}

// ---------------------------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------------------------

fn find_device(uid: &str) -> CmdResult<DeviceInfo> {
    output::output_devices()
        .map_err(err)?
        .into_iter()
        .find(|d| d.uid == uid)
        .ok_or_else(|| "device not found — was it unplugged?".to_string())
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
fn dsd_mode(path: &std::path::Path, device: &DeviceInfo, exclusive: bool, dop: bool, dsp: &DspSettings) -> (DsdMode, Option<String>) {
    if !dop || !dsd::is_dsd_path(path) {
        return (DsdMode::Pcm, None);
    }
    let rate = match dsd::probe(path) {
        Ok(h) => h.rate,
        Err(_) => return (DsdMode::Pcm, None),
    };
    let note = if !exclusive {
        Some("DoP работает только в эксклюзивном режиме".to_string())
    } else if dsp.eq.is_some() || dsp.crossfeed.is_some() {
        Some("эквалайзер и кроссфид работают с PCM".to_string())
    } else if !device.device.supports_dop(rate) {
        Some(format!(
            "{} не работает на {} кГц, нужных для DoP {}",
            device.name.trim(),
            (rate as f64 / 16_000.0).to_string().replace('.', ","),
            dsd::dsd_name(rate)
        ))
    } else {
        None
    };
    (if note.is_none() { DsdMode::Dop } else { DsdMode::Pcm }, note)
}

fn open_for(path: &std::path::Path, mode: DsdMode) -> CmdResult<Arc<Track>> {
    open_file(path, mode).map_err(err)
}

const AUDIO_EXTENSIONS: [&str; 12] = ["flac", "wav", "aif", "aiff", "m4a", "alac", "mp3", "ogg", "caf", "mka", "dsf", "dff"];

/// Turn dropped files and folders into a flat, ordered list of audio files.
/// Folders are walked recursively and sorted by path, so an album lands in track order.
#[tauri::command]
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

#[tauri::command]
fn list_devices() -> CmdResult<Vec<DeviceDto>> {
    Ok(output::output_devices().map_err(err)?.iter().map(DeviceDto::from).collect())
}

#[tauri::command]
async fn play_file(
    state: State<'_, AppState>,
    path: String,
    device_uid: String,
    exclusive: bool,
    start_at: Option<f64>,
    dop: Option<bool>,
) -> CmdResult<NowPlayingDto> {
    let device = find_device(&device_uid)?;
    let file = PathBuf::from(&path);
    let dop = dop.unwrap_or(false);
    let dsp = state.dsp.lock().unwrap().clone();
    let (mode, dsd_note) = dsd_mode(&file, &device, exclusive, dop, &dsp);
    // Only the headers are parsed here; the decoder keeps filling RAM in the background
    // while playback starts. Still off the UI/IPC threads: opening may touch a slow disk.
    let track = {
        let file = file.clone();
        tauri::async_runtime::spawn_blocking(move || open_for(&file, mode)).await.map_err(err)??
    };
    let meta = track.meta.clone();
    let (channels, duration) = (track.channels, track.duration_secs());
    let opts = PlayOptions { exclusive, dsp, start_at: start_at.unwrap_or(0.0), start_paused: false };
    let mut signal = start(&state, &device, track.clone(), opts)?;
    signal.dsd_note = dsd_note.clone();
    *state.current.lock().unwrap() = Some(Current { track, path: file, device_uid, exclusive, dop, dsd_note });

    let file_stem = PathBuf::from(&path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(NowPlayingDto {
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
    })
}

#[tauri::command]
fn channel_test(state: State<'_, AppState>, device_uid: String) -> CmdResult<SignalDto> {
    let device = find_device(&device_uid)?;
    // Play at a rate the device already runs at, so the test itself switches nothing.
    let rate = device.nominal_rate.round() as u32;
    // The test bypasses processing: crossfeed would blur exactly what it checks.
    *state.current.lock().unwrap() = None;
    start(&state, &device, Arc::new(testsignal::channel_test(rate)), PlayOptions::default())
}

#[tauri::command]
fn status(state: State<'_, AppState>) -> StatusDto {
    match state.player.lock().unwrap().as_ref() {
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
            error: p.source_error().or_else(|| p.output_error()),
        },
        None => StatusDto {
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
        },
    }
}

/// Peak level per channel (dBFS, floored at −90) since the previous call.
#[tauri::command]
fn meter(state: State<'_, AppState>) -> [f64; 2] {
    let peaks = state.player.lock().unwrap().as_ref().map_or([0.0; 2], |p| p.take_meter());
    peaks.map(|v| (20.0 * (v as f64).max(1e-9).log10()).max(-90.0))
}

#[tauri::command]
fn set_paused(state: State<'_, AppState>, paused: bool) {
    if let Some(p) = state.player.lock().unwrap().as_ref() {
        p.set_paused(paused);
    }
}

#[tauri::command]
fn seek(state: State<'_, AppState>, seconds: f64) {
    if let Some(p) = state.player.lock().unwrap().as_ref() {
        p.seek(seconds);
    }
}

#[tauri::command]
fn stop(state: State<'_, AppState>) {
    drop(state.player.lock().unwrap().take());
    *state.current.lock().unwrap() = None;
}

/// Replace the processing settings. Everything off = bit-perfect. If a track is loaded it
/// continues from the same position (and pause state) with the new chain.
#[tauri::command]
fn set_dsp(
    state: State<'_, AppState>,
    upsample_khz: Option<f64>,
    crossfeed: Option<String>,
    eq_path: Option<String>,
    headroom: Option<bool>,
    restore_highs: Option<bool>,
) -> CmdResult<DspResult> {
    let eq = match &eq_path {
        Some(path) => {
            let p = PathBuf::from(path);
            let text = std::fs::read_to_string(&p).map_err(|e| format!("не удалось прочитать профиль: {e}"))?;
            let name = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let name = name.trim_end_matches(" ParametricEQ").to_string();
            Some(EqProfile::parse_autoeq(&name, &text).map_err(err)?)
        }
        None => None,
    };
    let settings = DspSettings {
        upsample: upsample_khz.map_or(Upsample::Off, |k| Upsample::UpTo(k * 1000.0)),
        eq,
        crossfeed: crossfeed
            .and_then(|k| CROSSFEED_LEVELS.iter().find(|(key, _)| *key == k).map(|(_, l)| *l)),
        lossy: LossySettings { headroom: headroom.unwrap_or(false), restore_highs: restore_highs.unwrap_or(false) },
    };
    let eq_dto = settings.eq.as_ref().map(EqDto::from);
    *state.dsp.lock().unwrap() = settings.clone();

    // Restart in place, if something is loaded.
    let resume = {
        let player = state.player.lock().unwrap();
        player.as_ref().map(|p| (p.position_secs(), p.is_paused(), p.is_finished()))
    };
    let mut current = state.current.lock().unwrap();
    let signal = match (resume, current.as_mut()) {
        (Some((pos, paused, false)), Some(cur)) => {
            let device = find_device(&cur.device_uid)?;
            // A DSD file may have to switch between DoP and PCM (EQ/crossfeed need PCM).
            if dsd::is_dsd_path(&cur.path) {
                let (mode, note) = dsd_mode(&cur.path, &device, cur.exclusive, cur.dop, &settings);
                if (mode == DsdMode::Dop) != cur.track.is_dop() {
                    cur.track = open_for(&cur.path, mode)?;
                }
                cur.dsd_note = note;
            }
            let opts = PlayOptions { exclusive: cur.exclusive, dsp: settings, start_at: pos, start_paused: paused };
            let mut signal = start(&state, &device, cur.track.clone(), opts)?;
            signal.dsd_note = cur.dsd_note.clone();
            Some(signal)
        }
        _ => None,
    };
    Ok(DspResult { signal, eq: eq_dto })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .setup(|app| {
            use tauri::Manager;
            let lib = library::Library::load(app.handle());
            app.manage(lib);
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
            expand_paths,
            play_file,
            channel_test,
            status,
            meter,
            set_paused,
            seek,
            stop,
            set_dsp,
            library::library_status,
            library::library_albums,
            library::library_tracks,
            library::library_add_folder,
            library::library_remove_folder,
            library::library_rescan
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
