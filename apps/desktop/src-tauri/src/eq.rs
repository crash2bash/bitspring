//! Equaliser presets: kept as JSON in the app's folder, imported from and exported to other
//! apps' formats, applied to what plays — live, without a restart, whenever the engine can.
//!
//! The EQ belongs to the output: a headphone correction made for one pair of headphones must
//! not play on the speakers, nor on a device plugged in for the first time. So each output,
//! by its settings key (see `device_key`), is bound to a preset or to "off", and an output
//! that was never bound plays without. Presets themselves are shared: any output can be bound
//! to any of them. Each output also remembers the preset it last played (`last`), so switching
//! its EQ off and on again comes back to its own; `active` is only the preset chosen last,
//! anywhere — what the editor shows on an output that has none of its own.
//!
//! Earlier versions had one switch and one preset for every output, with bindings by uid. Such
//! a file plays as it did until the UI adopts it once at start-up (`adopt`): the shared preset
//! then goes to the output that was picked when it was set up, and nowhere else.
//!
//! Every edit is applied as it is made; the UI asks for the file write only when a gesture ends.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use hifi_engine::UserError;
use hifi_engine::dsp::eq::EqPreset;
use hifi_engine::dsp::eq_io::{self, EqFormat};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::{AppState, CmdResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredPreset {
    pub id: String,
    #[serde(flatten)]
    pub preset: EqPreset,
    #[serde(default)]
    pub updated: u64,
}

/// What an output does with the EQ: play without, or play this preset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "id", rename_all = "camelCase")]
pub enum DeviceEq {
    Off,
    Preset(String),
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct EqFile {
    presets: Vec<StoredPreset>,
    /// The preset chosen last, on any output: what the editor shows where nothing else is to
    /// be shown. It plays nowhere by itself (but see `enabled`).
    active: Option<String>,
    /// Earlier versions' switch: `active` played on every output without a binding. Read only
    /// while `per_device` is false, and switched off by the adoption.
    enabled: bool,
    /// What each output plays, by settings key (written by earlier versions: by uid).
    devices: BTreeMap<String, DeviceEq>,
    /// The file has been adopted: an output without a binding plays without the EQ. False in
    /// a file written by earlier versions.
    per_device: bool,
    /// The preset each output last played, by settings key: where switching its EQ on again
    /// comes back to.
    last: BTreeMap<String, String>,
}

#[derive(Default)]
pub struct EqStore {
    file: EqFile,
    path: Option<PathBuf>,
    /// Compare mode (level-matched bypass). Not saved: the app always starts with the EQ heard.
    pub bypass: bool,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn new_id() -> String {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("eq{t:x}{:x}", N.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// An Android uid as earlier versions bound it: Android hands out a new one at every replug,
/// so a binding under it could never match again (the settings key there is connection + name).
fn stale_android_uid(key: &str) -> bool {
    key.strip_prefix("android:").is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}

impl EqStore {
    pub fn load(app: &AppHandle) -> Self {
        let path = app.path().app_data_dir().ok().map(|d| d.join("eq.json"));
        let file = path.as_deref().and_then(hifi_engine::store::load_json::<EqFile>).unwrap_or_default();
        Self { file, path, bypass: false }
    }

    /// Written to a temporary file first, so a crash mid-write never leaves half a file.
    fn save(&self) -> CmdResult<()> {
        let path = self.path.as_ref().ok_or(UserError::NoAppFolder)?;
        hifi_engine::store::save_json(path, &self.file).map_err(|e| UserError::WriteFailed { detail: e.to_string() })
    }

    fn get(&self, id: &str) -> Option<&StoredPreset> {
        self.file.presets.iter().find(|p| p.id == id)
    }

    /// The preset in effect on the output with settings key `key` (None = EQ off there).
    pub fn effective(&self, key: &str) -> Option<EqPreset> {
        self.effective_id(key).and_then(|id| self.get(&id)).map(|p| p.preset.clone())
    }

    /// Id of the preset in effect on the output with settings key `key`: its binding; without
    /// one, nothing — but in a file not adopted yet, the earlier versions' rule, so an update
    /// changes nothing until the UI has said which output the shared preset belongs to.
    fn effective_id(&self, key: &str) -> Option<String> {
        match self.file.devices.get(key) {
            Some(DeviceEq::Off) => None,
            Some(DeviceEq::Preset(id)) => Some(id.clone()),
            None if !self.file.per_device && self.file.enabled => self.file.active.clone(),
            None => None,
        }
    }

    fn add(&mut self, preset: EqPreset) -> StoredPreset {
        let mut preset = preset.sanitized();
        if preset.name.trim().is_empty() {
            preset.name = "EQ".into();
        }
        let stored = StoredPreset { id: new_id(), preset, updated: now() };
        self.file.presets.push(stored.clone());
        stored
    }

    /// Output `key` plays `binding` from now on. A preset is also what it comes back to after
    /// an off, and the one chosen last; an off keeps what it comes back to.
    fn bind(&mut self, key: &str, binding: DeviceEq) -> CmdResult<()> {
        if let DeviceEq::Preset(id) = &binding {
            if self.get(id).is_none() {
                return Err(UserError::EqNotFound);
            }
            self.file.last.insert(key.to_string(), id.clone());
            self.file.active = Some(id.clone());
        }
        self.file.devices.insert(key.to_string(), binding);
        Ok(())
    }

    /// New presets `ids` (in the order made): the first plays on output `key` and only there;
    /// with no output, it is only the one the editor shows.
    fn place_new(&mut self, ids: &[String], key: Option<&str>) -> CmdResult<()> {
        let Some(first) = ids.first() else { return Ok(()) };
        match key {
            Some(key) => self.bind(key, DeviceEq::Preset(first.clone())),
            None => {
                self.file.active = Some(first.clone());
                Ok(())
            }
        }
    }

    /// Remove preset `id`. The outputs that played it go to off; the editor shows another.
    fn delete(&mut self, id: &str) {
        self.file.presets.retain(|p| p.id != id);
        if self.file.active.as_deref() == Some(id) {
            self.file.active = self.file.presets.first().map(|p| p.id.clone());
        }
        self.file.devices.retain(|_, d| *d != DeviceEq::Preset(id.to_string()));
        self.file.last.retain(|_, l| l != id);
    }

    /// Take over a file written by earlier versions, once: the one preset they played on every
    /// output goes to output `key` (the one picked when it was set up), unless that output has
    /// a binding of its own already; bindings under Android uids that can never match again are
    /// dropped; from then on an output without a binding plays without. Returns false when
    /// there was nothing to do (adopted already): the UI calls this at every start.
    fn adopt(&mut self, key: Option<&str>) -> bool {
        if self.file.per_device {
            return false;
        }
        let shared = self.file.active.clone().filter(|id| self.file.enabled && self.get(id).is_some());
        if let (Some(id), Some(key)) = (shared, key)
            && !self.file.devices.contains_key(key)
        {
            self.file.devices.insert(key.to_string(), DeviceEq::Preset(id));
        }
        self.file.devices.retain(|k, _| !stale_android_uid(k));
        self.file.last.retain(|k, _| !stale_android_uid(k));
        // An output bound to its own preset comes back to it after an off.
        for (k, d) in &self.file.devices {
            if let DeviceEq::Preset(id) = d {
                self.file.last.entry(k.clone()).or_insert_with(|| id.clone());
            }
        }
        self.file.enabled = false;
        self.file.per_device = true;
        true
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EqState {
    presets: Vec<StoredPreset>,
    /// The preset chosen last, anywhere (see `EqFile::active`).
    active_id: Option<String>,
    /// Earlier versions' switch, until the adoption (see `EqFile::enabled`).
    enabled: bool,
    per_device: bool,
    bypass: bool,
    /// Each output's binding, by settings key.
    devices: BTreeMap<String, DeviceEq>,
    /// The preset each output last played, by settings key.
    last: BTreeMap<String, String>,
    /// The settings key of the output that plays now (what `devices` is looked up by), and the
    /// preset in effect there.
    playing_on: Option<String>,
    effective_id: Option<String>,
}

fn snapshot(state: &AppState) -> EqState {
    let playing_on = state.current.lock().unwrap().as_ref().map(|c| c.key.clone());
    let s = state.eq.lock().unwrap();
    EqState {
        presets: s.file.presets.clone(),
        active_id: s.file.active.clone(),
        enabled: s.file.enabled,
        per_device: s.file.per_device,
        bypass: s.bypass,
        devices: s.file.devices.clone(),
        last: s.file.last.clone(),
        effective_id: playing_on.as_deref().and_then(|d| s.effective_id(d)),
        playing_on,
    }
}

// Commands that apply a change are async: Tauri then runs them off the main thread, and a
// linear-phase redesign (up to ~0.1 s at 384 kHz) never stalls the window.

/// Apply a change and hand back the new state (and the signal path, when playback had to
/// restart — an EQ switched on or off, or a different FIR layout).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EqChanged {
    state: EqState,
    signal: Option<crate::SignalDto>,
}

fn changed(state: &AppState) -> CmdResult<EqChanged> {
    let signal = crate::apply_eq(state)?;
    Ok(EqChanged { state: snapshot(state), signal })
}

/// The settings key of the output `device_uid` from the UI; None for no output at all (an
/// empty uid: none connected and none picked).
fn key_of(state: &AppState, device_uid: Option<&str>) -> Option<String> {
    device_uid.filter(|u| !u.is_empty()).map(|u| crate::settings_key_of_uid(state, u))
}

#[tauri::command]
pub fn eq_state(state: State<'_, AppState>) -> EqState {
    snapshot(&state)
}

/// Take over a file written by earlier versions (see `EqStore::adopt`); `device_uid` is the
/// output picked when it was set up. Called by the UI at every start: does nothing once done.
#[tauri::command]
pub async fn eq_adopt(state: State<'_, AppState>, device_uid: String) -> CmdResult<EqChanged> {
    let key = key_of(&state, Some(&device_uid));
    {
        let mut s = state.eq.lock().unwrap();
        if s.adopt(key.as_deref()) {
            s.save()?;
        }
    }
    changed(&state)
}

/// Save edits to a preset (`persist` false while a gesture is still going: applied, not
/// written). An unknown or missing id makes a new preset, which plays on output `device_uid`
/// (the one the EQ screen shows) and only there.
#[tauri::command]
pub async fn eq_save(state: State<'_, AppState>, id: Option<String>, preset: EqPreset, persist: bool, device_uid: Option<String>) -> CmdResult<EqChanged> {
    let key = key_of(&state, device_uid.as_deref());
    {
        let mut s = state.eq.lock().unwrap();
        let preset = preset.sanitized();
        match id.as_deref().and_then(|id| s.file.presets.iter_mut().find(|p| p.id == id)) {
            Some(p) => {
                p.preset = preset;
                p.updated = now();
            }
            None => {
                let added = s.add(preset);
                s.place_new(&[added.id], key.as_deref())?;
            }
        }
        if persist {
            s.save()?;
        }
    }
    changed(&state)
}

#[tauri::command]
pub async fn eq_delete(state: State<'_, AppState>, id: String) -> CmdResult<EqChanged> {
    {
        let mut s = state.eq.lock().unwrap();
        s.delete(&id);
        s.save()?;
    }
    changed(&state)
}

/// Level-matched compare: hear the music without the EQ but at its average loudness.
#[tauri::command]
pub async fn eq_set_bypass(state: State<'_, AppState>, bypass: bool) -> CmdResult<EqChanged> {
    state.eq.lock().unwrap().bypass = bypass;
    changed(&state)
}

/// What output `device_uid` plays: off, or a preset. With no output at all there is nothing to
/// bind; a preset then only becomes the one the editor shows.
#[tauri::command]
pub async fn eq_bind_device(state: State<'_, AppState>, device_uid: String, binding: DeviceEq) -> CmdResult<EqChanged> {
    let key = key_of(&state, Some(&device_uid));
    {
        let mut s = state.eq.lock().unwrap();
        match (key, binding) {
            (Some(key), b) => s.bind(&key, b)?,
            (None, DeviceEq::Preset(id)) => {
                if s.get(&id).is_none() {
                    return Err(UserError::EqNotFound);
                }
                s.place_new(&[id], None)?;
            }
            (None, DeviceEq::Off) => {}
        }
        s.save()?;
    }
    changed(&state)
}

/// Add `presets`; the first plays on output `device_uid` (the one the EQ screen shows: adding
/// is nearly always "use this here"), and only there.
fn add_imported(state: &AppState, presets: Vec<EqPreset>, device_uid: Option<&str>) -> CmdResult<(Vec<String>, EqChanged)> {
    let key = key_of(state, device_uid);
    let ids = {
        let mut s = state.eq.lock().unwrap();
        let ids: Vec<String> = presets.into_iter().map(|p| s.add(p).id).collect();
        s.place_new(&ids, key.as_deref())?;
        s.save()?;
        ids
    };
    Ok((ids, changed(state)?))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EqImported {
    ids: Vec<String>,
    #[serde(flatten)]
    changed: EqChanged,
}

/// Read presets from a file in any format we know; the first plays on output `device_uid`.
#[tauri::command]
pub async fn eq_import_file(app: AppHandle, path: String, device_uid: Option<String>) -> CmdResult<EqImported> {
    let p = PathBuf::from(&path);
    let bytes = std::fs::read(&p).map_err(|e| UserError::ProfileUnreadable { detail: e.to_string() })?;
    let text = hifi_engine::playlist::decode_text(&bytes);
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let presets = eq_io::import(&name, &text).map_err(|e| UserError::of(&e))?;
    let state = app.state::<AppState>();
    let (ids, changed) = add_imported(&state, presets, device_uid.as_deref())?;
    Ok(EqImported { ids, changed })
}

#[derive(serde::Serialize)]
pub struct HeadphoneDto {
    name: String,
    source: String,
}

/// The built-in headphone profiles (AutoEQ), by name.
#[tauri::command]
pub fn eq_headphones() -> Vec<HeadphoneDto> {
    eq_io::headphone_profiles().iter().map(|p| HeadphoneDto { name: p.name.clone(), source: p.source.clone() }).collect()
}

/// Add a built-in headphone profile as a preset, playing on output `device_uid`.
#[tauri::command]
pub async fn eq_add_headphone(state: State<'_, AppState>, name: String, device_uid: Option<String>) -> CmdResult<EqImported> {
    let preset = eq_io::headphone_preset(&name).ok_or(UserError::EqNotFound)?;
    let (ids, changed) = add_imported(&state, vec![preset], device_uid.as_deref())?;
    Ok(EqImported { ids, changed })
}

// ---- AutoEQ's whole catalog ------------------------------------------------------------------

/// The catalog as last read, so adding a profile doesn't re-read the file.
static CATALOG: std::sync::Mutex<Option<std::sync::Arc<Vec<eq_io::CatalogEntry>>>> = std::sync::Mutex::new(None);

/// A downloaded catalog is used for this long before it's fetched again (AutoEQ adds headphones
/// every few weeks); an older one still serves when the download fails.
const CATALOG_FRESH: std::time::Duration = std::time::Duration::from_secs(30 * 86_400);
/// The file is about 2 MB; anything far bigger isn't ours.
const CATALOG_MAX: u64 = 16 << 20;

fn catalog_failed(e: impl std::fmt::Display) -> UserError {
    UserError::CatalogUnavailable { detail: e.to_string() }
}

fn download_catalog() -> Result<String, UserError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(15)))
        .timeout_global(Some(std::time::Duration::from_secs(120)))
        .user_agent(concat!("BitSpring/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut resp = agent.get(eq_io::AUTOEQ_CATALOG).call().map_err(catalog_failed)?;
    resp.body_mut().with_config().limit(CATALOG_MAX).read_to_string().map_err(catalog_failed)
}

/// The catalog: the copy kept in the app's folder while it's fresh (or `refresh` isn't asked),
/// else downloaded anew, falling back to the old copy when that fails.
fn load_catalog(dir: Option<PathBuf>, refresh: bool) -> Result<std::sync::Arc<Vec<eq_io::CatalogEntry>>, UserError> {
    let path = dir.map(|d| d.join("autoeq-catalog.json"));
    let cached = path.as_ref().and_then(|p| {
        let age = std::fs::metadata(p).and_then(|m| m.modified()).ok()?.elapsed().unwrap_or_default();
        let text = std::fs::read_to_string(p).ok()?;
        Some((eq_io::parse_catalog(&text).ok()?, age < CATALOG_FRESH))
    });
    if let Some((all, true)) = &cached
        && !refresh
    {
        return Ok(std::sync::Arc::new(all.clone()));
    }
    let fetched = download_catalog().and_then(|text| {
        let all = eq_io::parse_catalog(&text).map_err(catalog_failed)?;
        if let Some(p) = &path {
            // Written aside and renamed, so a crash never leaves half a file.
            let part = p.with_extension("json.part");
            if std::fs::create_dir_all(p.parent().unwrap_or(p)).and_then(|_| std::fs::write(&part, &text)).and_then(|_| std::fs::rename(&part, p)).is_err() {
                let _ = std::fs::remove_file(&part);
            }
        }
        Ok(all)
    });
    match (fetched, cached) {
        (Ok(all), _) => Ok(std::sync::Arc::new(all)),
        (Err(_), Some((all, _))) => Ok(std::sync::Arc::new(all)),
        (Err(e), None) => Err(e),
    }
}

#[derive(Serialize)]
pub struct CatalogDto {
    name: String,
    source: String,
    rig: Option<String>,
}

/// AutoEQ's whole catalog (thousands of headphones), downloaded once and kept for a month;
/// `refresh` fetches it again now.
#[tauri::command]
pub async fn eq_catalog(app: AppHandle, refresh: bool) -> CmdResult<Vec<CatalogDto>> {
    let dir = app.path().app_data_dir().ok();
    let all = tauri::async_runtime::spawn_blocking(move || load_catalog(dir, refresh)).await.map_err(catalog_failed)??;
    *CATALOG.lock().unwrap() = Some(all.clone());
    Ok(all.iter().map(|e| CatalogDto { name: e.name.clone(), source: e.source.clone(), rig: e.rig.clone() }).collect())
}

/// Add catalog entry `index` (as `eq_catalog` listed it) as a preset; `name` and `source` make
/// sure it's still the one the listener picked. It plays on output `device_uid`.
#[tauri::command]
pub async fn eq_add_catalog(state: State<'_, AppState>, index: usize, name: String, source: String, device_uid: Option<String>) -> CmdResult<EqImported> {
    let entry = CATALOG
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|all| all.get(index).cloned())
        .filter(|e| e.name == name && e.source == source)
        .ok_or(UserError::EqNotFound)?;
    let preset = eq_io::catalog_preset(&entry).map_err(|e| UserError::of(&e))?;
    let (ids, changed) = add_imported(&state, vec![preset], device_uid.as_deref())?;
    Ok(EqImported { ids, changed })
}

/// Read presets from pasted text (AutoEQ's web app, a forum post, another app's export); the
/// first plays on output `device_uid`.
#[tauri::command]
pub async fn eq_import_text(state: State<'_, AppState>, text: String, name: String, device_uid: Option<String>) -> CmdResult<EqImported> {
    let presets = eq_io::import(&name, &text).map_err(|e| UserError::of(&e))?;
    let (ids, changed) = add_imported(&state, presets, device_uid.as_deref())?;
    Ok(EqImported { ids, changed })
}

fn exported(state: &AppState, ids: &[String], format: EqFormat) -> CmdResult<String> {
    let s = state.eq.lock().unwrap();
    let presets: Vec<EqPreset> = ids.iter().filter_map(|id| s.get(id)).map(|p| p.preset.clone()).collect();
    if presets.is_empty() {
        return Err(UserError::EqNotFound);
    }
    Ok(eq_io::export(&presets, format))
}

/// The preset(s) as text in `format`, for the clipboard.
#[tauri::command]
pub fn eq_export_text(state: State<'_, AppState>, ids: Vec<String>, format: EqFormat) -> CmdResult<String> {
    exported(&state, &ids, format)
}

/// Write the preset(s) to `path` (or, with no path, to the app's export folder) and return
/// where it went.
#[tauri::command]
pub fn eq_export(app: AppHandle, state: State<'_, AppState>, ids: Vec<String>, format: EqFormat, path: Option<String>) -> CmdResult<String> {
    let text = exported(&state, &ids, format)?;
    let path = match path {
        Some(p) => PathBuf::from(p),
        None => {
            let name = {
                let s = state.eq.lock().unwrap();
                if ids.len() == 1 { s.get(&ids[0]).map(|p| p.preset.name.clone()).unwrap_or_default() } else { "BitSpring EQ".into() }
            };
            let safe: String = name.chars().map(|c| if c.is_alphanumeric() || " -_.()".contains(c) { c } else { '_' }).collect();
            crate::library::export_dir(&app).ok_or(UserError::NoAppFolder)?.join(format!("{}.{}", safe.trim(), format.extension()))
        }
    };
    write_file(&path, &text)?;
    Ok(path.to_string_lossy().into_owned())
}

fn write_file(path: &Path, text: &str) -> CmdResult<()> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(path, text).map_err(|e| UserError::WriteFailed { detail: e.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hifi_engine::dsp::eq::{Channels, FilterKind, Phase};

    /// What the UI sends (api.ts EqPreset / DeviceEq) must land in the engine's types.
    #[test]
    fn ui_shapes_round_trip() {
        let ui = r#"{"name":"LCD-2","preamp":-6.4,"autoPreamp":true,"balance":0.5,"phase":"linear",
            "curve":[[20,1.5],[1000,0]],
            "bands":[{"enabled":true,"kind":"highPass","freq":20,"gain":0,"q":0.71,"slope":24,"channels":"right"}]}"#;
        let p: EqPreset = serde_json::from_str(ui).unwrap();
        assert_eq!(p.phase, Phase::Linear);
        assert_eq!((p.bands[0].kind, p.bands[0].slope, p.bands[0].channels), (FilterKind::HighPass, 24, Channels::Right));
        let stored = StoredPreset { id: "x".into(), preset: p, updated: 1 };
        let v = serde_json::to_value(&stored).unwrap();
        assert_eq!(v["id"], "x");
        assert_eq!(v["autoPreamp"], true);
        assert_eq!(v["bands"][0]["kind"], "highPass");

        let off: DeviceEq = serde_json::from_str(r#"{"mode":"off"}"#).unwrap();
        assert_eq!(off, DeviceEq::Off);
        let own: DeviceEq = serde_json::from_str(r#"{"mode":"preset","id":"p1"}"#).unwrap();
        assert_eq!(own, DeviceEq::Preset("p1".into()));
        assert_eq!(serde_json::to_string(&DeviceEq::Off).unwrap(), r#"{"mode":"off"}"#);
        let f: EqFormat = serde_json::from_str(r#""graphicEq""#).unwrap();
        assert_eq!(f, EqFormat::GraphicEq);
    }

    /// A store with presets A and B (ids returned), adopted already unless `legacy`.
    fn store(legacy: bool) -> (EqStore, String, String) {
        let mut s = EqStore::default();
        let a = s.add(EqPreset { name: "A".into(), ..Default::default() }).id;
        let b = s.add(EqPreset { name: "B".into(), ..Default::default() }).id;
        s.file.per_device = !legacy;
        (s, a, b)
    }

    fn playing(s: &EqStore, key: &str) -> Option<String> {
        s.effective(key).map(|p| p.name)
    }

    #[test]
    fn device_binding_decides_what_plays() {
        let (mut s, a, b) = store(false);
        s.bind("q7", DeviceEq::Preset(a.clone())).unwrap();
        s.bind("spk", DeviceEq::Preset(b.clone())).unwrap();
        s.bind("air", DeviceEq::Off).unwrap();
        assert_eq!(playing(&s, "q7").as_deref(), Some("A"));
        assert_eq!(playing(&s, "spk").as_deref(), Some("B"));
        assert_eq!(playing(&s, "air"), None);
        // The one chosen last is what the editor shows elsewhere; it plays nowhere by itself.
        assert_eq!(s.file.active.as_deref(), Some(b.as_str()));
        assert_eq!(playing(&s, "new-dac"), None);
        assert_eq!(s.bind("q7", DeviceEq::Preset("gone".into())), Err(UserError::EqNotFound));
        assert_eq!(playing(&s, "q7").as_deref(), Some("A"), "a failed bind changes nothing");
    }

    #[test]
    fn an_unbound_output_is_silent_once_adopted() {
        let (mut s, a, _) = store(false);
        s.file.active = Some(a);
        // Even with the earlier versions' switch left on in the file.
        s.file.enabled = true;
        assert_eq!(playing(&s, "q7"), None);
        assert_eq!(s.effective_id("q7"), None);
    }

    #[test]
    fn the_old_rule_holds_until_adopted() {
        let (mut s, a, b) = store(true);
        s.file.active = Some(a);
        assert_eq!(playing(&s, "q7"), None, "off until enabled");
        s.file.enabled = true;
        assert_eq!(playing(&s, "q7").as_deref(), Some("A"));
        assert_eq!(playing(&s, "anything").as_deref(), Some("A"), "one preset for every output");
        s.file.devices.insert("spk".into(), DeviceEq::Preset(b));
        s.file.devices.insert("air".into(), DeviceEq::Off);
        assert_eq!(playing(&s, "spk").as_deref(), Some("B"));
        assert_eq!(playing(&s, "air"), None);
        s.file.enabled = false;
        assert_eq!(playing(&s, "spk").as_deref(), Some("B"), "an output's own preset ignores the switch");
        assert_eq!(playing(&s, "q7"), None);
    }

    #[test]
    fn adoption_gives_the_shared_preset_to_the_picked_output_once() {
        let (mut s, a, b) = store(true);
        s.file.active = Some(a.clone());
        s.file.enabled = true;
        s.file.devices.insert("spk".into(), DeviceEq::Preset(b.clone()));
        s.file.devices.insert("air".into(), DeviceEq::Off);
        // Bound by earlier versions under Android uids, which change at every replug.
        s.file.devices.insert("android:17".into(), DeviceEq::Preset(b.clone()));
        s.file.devices.insert("android:Usb:FiiO Q7".into(), DeviceEq::Preset(b.clone()));
        assert!(s.adopt(Some("q7")));
        assert!(s.file.per_device && !s.file.enabled);
        assert_eq!(playing(&s, "q7").as_deref(), Some("A"));
        assert_eq!(s.file.last.get("q7"), Some(&a));
        assert_eq!(playing(&s, "spk").as_deref(), Some("B"), "an output's own preset stays");
        assert_eq!(s.file.last.get("spk"), Some(&b), "and is what it comes back to after an off");
        assert_eq!(playing(&s, "air"), None);
        assert_eq!(playing(&s, "headphones"), None, "the shared preset went to one output only");
        assert!(!s.file.devices.contains_key("android:17"));
        assert_eq!(playing(&s, "android:Usb:FiiO Q7").as_deref(), Some("B"));

        // Every later start: nothing more happens.
        s.bind("q7", DeviceEq::Off).unwrap();
        s.file.enabled = true;
        assert!(!s.adopt(Some("spk")));
        assert_eq!(playing(&s, "q7"), None);
        assert_eq!(playing(&s, "spk").as_deref(), Some("B"));
        assert_eq!(playing(&s, "other"), None);
    }

    #[test]
    fn adoption_leaves_the_picked_outputs_own_binding_alone() {
        let (mut s, a, b) = store(true);
        s.file.active = Some(a.clone());
        s.file.enabled = true;
        s.file.devices.insert("q7".into(), DeviceEq::Off);
        s.file.devices.insert("spk".into(), DeviceEq::Preset(b.clone()));
        assert!(s.adopt(Some("q7")));
        assert_eq!(playing(&s, "q7"), None, "switched off there before: stays off");
        assert!(!s.file.devices.values().any(|d| *d == DeviceEq::Preset(a.clone())), "and the shared preset goes nowhere else");

        let (mut s, a, b) = store(true);
        s.file.active = Some(a);
        s.file.enabled = true;
        s.file.devices.insert("spk".into(), DeviceEq::Preset(b));
        assert!(s.adopt(Some("spk")));
        assert_eq!(playing(&s, "spk").as_deref(), Some("B"), "its own preset, not the shared one");
    }

    #[test]
    fn adoption_of_a_file_with_the_eq_off_or_no_file_only_sets_the_flag() {
        let (mut s, a, _) = store(true);
        s.file.active = Some(a);
        assert!(s.adopt(Some("q7")));
        assert!(s.file.devices.is_empty() && s.file.last.is_empty());
        assert!(s.file.per_device);

        let mut fresh = EqStore::default();
        assert!(fresh.adopt(Some("q7")));
        assert!(fresh.file.per_device && fresh.file.devices.is_empty());
        // A shared preset that is gone goes nowhere; no output at all binds nothing either.
        let (mut s, _, _) = store(true);
        s.file.active = Some("gone".into());
        s.file.enabled = true;
        assert!(s.adopt(Some("q7")));
        assert!(s.file.devices.is_empty());
        let (mut s, a, _) = store(true);
        s.file.active = Some(a);
        s.file.enabled = true;
        assert!(s.adopt(None));
        assert!(s.file.devices.is_empty() && !s.file.enabled);
    }

    #[test]
    fn stale_android_uids_are_told_from_settings_keys() {
        assert!(stale_android_uid("android:17"));
        assert!(stale_android_uid("android:0"));
        assert!(!stale_android_uid("android:"));
        assert!(!stale_android_uid("android:Usb:FiiO Q7"));
        assert!(!stale_android_uid("android:BuiltIn:Speaker"));
        assert!(!stale_android_uid("AppleUSBAudioEngine:FiiO:Q7:1234:1"));
        assert!(!stale_android_uid("17"));
    }

    #[test]
    fn an_output_comes_back_to_its_own_preset() {
        let (mut s, a, b) = store(false);
        s.bind("q7", DeviceEq::Preset(a.clone())).unwrap();
        s.bind("spk", DeviceEq::Preset(b.clone())).unwrap();
        s.bind("q7", DeviceEq::Off).unwrap();
        assert_eq!(playing(&s, "q7"), None);
        assert_eq!(s.file.last.get("q7"), Some(&a), "an off keeps what it comes back to");
        // B was chosen last (on the speakers), but the DAC's own is A.
        assert_eq!(s.file.active.as_deref(), Some(b.as_str()));
        let back = s.file.last.get("q7").cloned().unwrap();
        s.bind("q7", DeviceEq::Preset(back)).unwrap();
        assert_eq!(playing(&s, "q7").as_deref(), Some("A"));
        assert_eq!(playing(&s, "spk").as_deref(), Some("B"));
    }

    #[test]
    fn a_new_preset_plays_only_where_it_was_made() {
        let (mut s, a, _) = store(false);
        s.bind("spk", DeviceEq::Preset(a.clone())).unwrap();
        let c = s.add(EqPreset { name: "C".into(), ..Default::default() }).id;
        let d = s.add(EqPreset { name: "D".into(), ..Default::default() }).id;
        s.place_new(&[c.clone(), d], Some("q7")).unwrap();
        assert_eq!(playing(&s, "q7").as_deref(), Some("C"), "the first of an import plays");
        assert_eq!(playing(&s, "spk").as_deref(), Some("A"));
        assert_eq!(playing(&s, "air"), None);
        assert_eq!(s.file.last.get("q7"), Some(&c));
        // No output: only added, and shown in the editor.
        let e = s.add(EqPreset { name: "E".into(), ..Default::default() }).id;
        s.place_new(std::slice::from_ref(&e), None).unwrap();
        assert_eq!(s.file.active.as_deref(), Some(e.as_str()));
        assert_eq!(playing(&s, "q7").as_deref(), Some("C"));
        assert!(!s.file.enabled);
    }

    #[test]
    fn deleting_a_preset_clears_its_bindings_and_returns() {
        let (mut s, a, b) = store(false);
        s.bind("q7", DeviceEq::Preset(a.clone())).unwrap();
        s.bind("spk", DeviceEq::Preset(b.clone())).unwrap();
        s.bind("air", DeviceEq::Preset(a.clone())).unwrap();
        s.bind("air", DeviceEq::Off).unwrap();
        s.delete(&a);
        assert_eq!(playing(&s, "q7"), None);
        assert!(!s.file.devices.contains_key("q7"));
        assert!(!s.file.last.values().any(|l| *l == a));
        assert_eq!(s.file.devices.get("air"), Some(&DeviceEq::Off));
        assert_eq!(playing(&s, "spk").as_deref(), Some("B"));
        assert_eq!(s.file.last.get("spk"), Some(&b));
        assert_eq!(s.file.active.as_deref(), Some(b.as_str()));
    }

    /// A file written by earlier versions reads with the new fields at their defaults.
    #[test]
    fn an_earlier_file_reads_as_not_adopted() {
        let f: EqFile = serde_json::from_str(r#"{"presets":[],"active":null,"enabled":true,"devices":{"spk":{"mode":"off"}}}"#).unwrap();
        assert!(!f.per_device && f.last.is_empty() && f.enabled);
        let v = serde_json::to_value(EqFile { per_device: true, ..Default::default() }).unwrap();
        assert_eq!(v["perDevice"], true);
        assert!(v["last"].is_object());
    }
}
