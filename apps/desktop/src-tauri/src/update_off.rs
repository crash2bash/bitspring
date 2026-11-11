//! App Store builds (Mac App Store, iPhone): the store does the updating — Apple's rules forbid
//! an app updating itself — so there is no updater code, key or address in these builds at all.

use hifi_engine::UserError;
use serde_json::{Value, json};
use tauri::AppHandle;

#[tauri::command]
pub fn update_check(app: AppHandle) -> Value {
    json!({ "enabled": false, "current": app.package_info().version.to_string(), "update": null })
}

#[tauri::command]
pub fn update_install() -> Result<(), UserError> {
    Ok(())
}

#[tauri::command]
pub fn update_progress() -> [u64; 2] {
    [0, 0]
}
