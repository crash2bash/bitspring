//! Equaliser presets: kept as JSON in the app's folder, imported from and exported to other
//! apps' formats, applied to what plays — live, without a restart, whenever the engine can.
//!
//! One preset is "active" for every output; an output can instead have its own preset (the
//! headphone correction follows the headphones) or have the EQ off. Every edit is applied as
//! it is made; the UI asks for the file write only when a gesture ends.

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

/// What a device does with the EQ: follow the active preset, play without, or use its own.
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
    active: Option<String>,
    enabled: bool,
    devices: BTreeMap<String, DeviceEq>,
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

impl EqStore {
    pub fn load(app: &AppHandle) -> Self {
        let path = app.path().app_data_dir().ok().map(|d| d.join("eq.json"));
        let file = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<EqFile>(&b).ok())
            .unwrap_or_default();
        Self { file, path, bypass: false }
    }

    /// Written to a temporary file first, so a crash mid-write never leaves half a file.
    fn save(&self) -> CmdResult<()> {
        let path = self.path.as_ref().ok_or(UserError::NoAppFolder)?;
        let write = |e: std::io::Error| UserError::WriteFailed { detail: e.to_string() };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(write)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.file).unwrap_or_default()).map_err(write)?;
        std::fs::rename(&tmp, path).map_err(write)
    }

    fn get(&self, id: &str) -> Option<&StoredPreset> {
        self.file.presets.iter().find(|p| p.id == id)
    }

    /// The preset in effect on `device_uid` (None = EQ off there).
    pub fn effective(&self, device_uid: &str) -> Option<EqPreset> {
        let id = match self.file.devices.get(device_uid) {
            Some(DeviceEq::Off) => return None,
            Some(DeviceEq::Preset(id)) => Some(id),
            None if self.file.enabled => self.file.active.as_ref(),
            None => None,
        }?;
        self.get(id).map(|p| p.preset.clone())
    }

    /// Id of the preset in effect on `device_uid`.
    fn effective_id(&self, device_uid: &str) -> Option<String> {
        match self.file.devices.get(device_uid) {
            Some(DeviceEq::Off) => None,
            Some(DeviceEq::Preset(id)) => Some(id.clone()),
            None => self.file.active.clone().filter(|_| self.file.enabled),
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
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EqState {
    presets: Vec<StoredPreset>,
    active_id: Option<String>,
    enabled: bool,
    bypass: bool,
    devices: BTreeMap<String, DeviceEq>,
    /// The output that plays now, and the preset in effect there.
    playing_on: Option<String>,
    effective_id: Option<String>,
}

fn snapshot(state: &AppState) -> EqState {
    let playing_on = state.current.lock().unwrap().as_ref().map(|c| c.device_uid.clone());
    let s = state.eq.lock().unwrap();
    EqState {
        presets: s.file.presets.clone(),
        active_id: s.file.active.clone(),
        enabled: s.file.enabled,
        bypass: s.bypass,
        devices: s.file.devices.clone(),
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

#[tauri::command]
pub fn eq_state(state: State<'_, AppState>) -> EqState {
    snapshot(&state)
}

/// Save edits to a preset (`persist` false while a gesture is still going: applied, not
/// written). An unknown or missing id makes a new preset.
#[tauri::command]
pub async fn eq_save(state: State<'_, AppState>, id: Option<String>, preset: EqPreset, persist: bool) -> CmdResult<EqChanged> {
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
                s.file.active = Some(added.id);
                s.file.enabled = true;
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
        s.file.presets.retain(|p| p.id != id);
        if s.file.active.as_deref() == Some(&id) {
            s.file.active = s.file.presets.first().map(|p| p.id.clone());
            if s.file.active.is_none() {
                s.file.enabled = false;
            }
        }
        s.file.devices.retain(|_, d| *d != DeviceEq::Preset(id.clone()));
        s.save()?;
    }
    changed(&state)
}

/// Make `id` the preset every output uses (unless it has its own), and switch the EQ on.
#[tauri::command]
pub async fn eq_activate(state: State<'_, AppState>, id: String) -> CmdResult<EqChanged> {
    {
        let mut s = state.eq.lock().unwrap();
        if s.get(&id).is_none() {
            return Err(UserError::EqNotFound);
        }
        s.file.active = Some(id);
        s.file.enabled = true;
        s.save()?;
    }
    changed(&state)
}

#[tauri::command]
pub async fn eq_set_enabled(state: State<'_, AppState>, enabled: bool) -> CmdResult<EqChanged> {
    {
        let mut s = state.eq.lock().unwrap();
        s.file.enabled = enabled && s.file.active.is_some();
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

/// What `device_uid` does: None = follow the active preset, else off or its own preset.
#[tauri::command]
pub async fn eq_bind_device(state: State<'_, AppState>, device_uid: String, binding: Option<DeviceEq>) -> CmdResult<EqChanged> {
    {
        let mut s = state.eq.lock().unwrap();
        match binding {
            Some(DeviceEq::Preset(id)) if s.get(&id).is_none() => return Err(UserError::EqNotFound),
            Some(b) => {
                s.file.devices.insert(device_uid, b);
            }
            None => {
                s.file.devices.remove(&device_uid);
            }
        }
        s.save()?;
    }
    changed(&state)
}

fn add_imported(state: &AppState, presets: Vec<EqPreset>) -> CmdResult<(Vec<String>, EqChanged)> {
    let ids = {
        let mut s = state.eq.lock().unwrap();
        let ids: Vec<String> = presets.into_iter().map(|p| s.add(p).id).collect();
        // The first one plays right away: importing is nearly always "use this".
        s.file.active = ids.first().cloned().or(s.file.active.take());
        s.file.enabled = s.file.active.is_some();
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

/// Read presets from a file in any format we know.
#[tauri::command]
pub async fn eq_import_file(app: AppHandle, path: String) -> CmdResult<EqImported> {
    let p = PathBuf::from(&path);
    let bytes = std::fs::read(&p).map_err(|e| UserError::ProfileUnreadable { detail: e.to_string() })?;
    let text = hifi_engine::playlist::decode_text(&bytes);
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let presets = eq_io::import(&name, &text).map_err(|e| UserError::of(&e))?;
    let state = app.state::<AppState>();
    let (ids, changed) = add_imported(&state, presets)?;
    Ok(EqImported { ids, changed })
}

/// Read presets from pasted text (AutoEQ's web app, a forum post, another app's export).
#[tauri::command]
pub async fn eq_import_text(state: State<'_, AppState>, text: String, name: String) -> CmdResult<EqImported> {
    let presets = eq_io::import(&name, &text).map_err(|e| UserError::of(&e))?;
    let (ids, changed) = add_imported(&state, presets)?;
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
                if ids.len() == 1 { s.get(&ids[0]).map(|p| p.preset.name.clone()).unwrap_or_default() } else { "Hifi EQ".into() }
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

    #[test]
    fn device_binding_decides_what_plays() {
        let mut s = EqStore::default();
        let a = s.add(EqPreset { name: "A".into(), ..Default::default() }).id;
        let b = s.add(EqPreset { name: "B".into(), ..Default::default() }).id;
        s.file.active = Some(a.clone());
        assert!(s.effective("q7").is_none(), "off until enabled");
        s.file.enabled = true;
        assert_eq!(s.effective("q7").unwrap().name, "A");
        s.file.devices.insert("spk".into(), DeviceEq::Preset(b));
        s.file.devices.insert("air".into(), DeviceEq::Off);
        assert_eq!(s.effective("spk").unwrap().name, "B");
        assert!(s.effective("air").is_none());
        s.file.enabled = false;
        assert_eq!(s.effective("spk").unwrap().name, "B", "a device's own preset ignores the global switch");
        assert!(s.effective("q7").is_none());
    }
}
