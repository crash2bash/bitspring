//! Streaming stand-in for App Store builds (and any build without the `streaming` feature):
//! the same commands, all answering that this version has no streaming services. Playlists
//! still keep Qobuz/TIDAL entries they were given — they just can't play here.

use std::sync::Arc;

use hifi_engine::Track;
use hifi_engine::UserError;
use serde_json::{Value, json};
use tauri::AppHandle;

use crate::playlists::PlaylistEntry;

pub const ENABLED: bool = false;

pub fn init(_app: &AppHandle) {}

pub fn is_uri(s: &str) -> bool {
    hifi_engine::playlist::streaming_uri(s).is_some_and(|u| u == s)
}

pub fn open(_uri: &str) -> anyhow::Result<Arc<Track>> {
    anyhow::bail!(UserError::StreamingOff)
}

pub fn fill_tags(_entries: &mut [PlaylistEntry]) {}

#[tauri::command]
pub fn streaming_status() -> Value {
    json!({ "available": false, "accounts": [], "quality": "max" })
}

#[tauri::command]
pub fn streaming_set_quality(quality: Value) {
    let _ = quality;
}

#[tauri::command]
pub fn streaming_logout(service: String) -> Result<(), UserError> {
    let _ = service;
    Err(UserError::StreamingOff)
}

#[tauri::command]
pub fn streaming_login(service: String) -> Result<Value, UserError> {
    let _ = service;
    Err(UserError::StreamingOff)
}

#[tauri::command]
pub fn streaming_search(service: String, query: String) -> Result<Value, UserError> {
    let _ = (service, query);
    Err(UserError::StreamingOff)
}

#[tauri::command]
pub fn streaming_album(service: String, id: String) -> Result<Value, UserError> {
    let _ = (service, id);
    Err(UserError::StreamingOff)
}

#[tauri::command]
pub fn streaming_favorites(service: String) -> Result<Value, UserError> {
    let _ = service;
    Err(UserError::StreamingOff)
}

#[tauri::command]
pub fn streaming_playlists(service: String) -> Result<Value, UserError> {
    let _ = service;
    Err(UserError::StreamingOff)
}

#[tauri::command]
pub fn streaming_playlist(service: String, id: String) -> Result<Value, UserError> {
    let _ = (service, id);
    Err(UserError::StreamingOff)
}

#[tauri::command]
pub fn streaming_import_playlist(service: String, id: String) -> Result<Value, UserError> {
    let _ = (service, id);
    Err(UserError::StreamingOff)
}

#[tauri::command]
pub fn streaming_find_missing(playlist_id: String, service: String) -> Result<usize, UserError> {
    let _ = (playlist_id, service);
    Err(UserError::StreamingOff)
}
