//! The quality check of library files (see hifi_engine::quality), with its results kept in
//! quality.json so an album checked once shows its verdicts at once next time. A result counts
//! as long as the file has the same size and modification time.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use hifi_engine::quality::{self, Report};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    /// The rules the result came from (results from older rules are checked again).
    #[serde(default)]
    rules: u32,
    size: u64,
    mtime: u64,
    report: Report,
}

#[derive(Default, Clone)]
pub struct Quality {
    cache: Arc<Mutex<HashMap<String, Entry>>>,
    dirty: Arc<AtomicBool>,
    file: Option<PathBuf>,
}

/// Size and modification time of the file a library path reads (a CUE track: its image).
fn stamp(path: &str) -> Option<(u64, u64)> {
    let m = std::fs::metadata(hifi_engine::cue::split(path).0).ok()?;
    Some((m.len(), m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs()))
}

impl Quality {
    pub fn load(app: &AppHandle) -> Self {
        let file = app.path().app_data_dir().ok().map(|d| d.join("quality.json"));
        let cache = file.as_deref().and_then(hifi_engine::store::load_json).unwrap_or_default();
        let q = Self { cache: Arc::new(Mutex::new(cache)), dirty: Arc::default(), file };
        // Results arrive one track at a time: write them out at most every couple of seconds.
        let saver = q.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(2));
                if saver.dirty.swap(false, Ordering::AcqRel)
                    && let Some(f) = &saver.file
                {
                    let snapshot = saver.cache.lock().unwrap().clone();
                    // Best effort: a lost write only means checking those files again.
                    let _ = hifi_engine::store::save_json(f, &snapshot);
                }
            }
        });
        q
    }

    fn cached(&self, path: &str) -> Option<Report> {
        let s = stamp(path)?;
        self.cache.lock().unwrap().get(path).filter(|e| (e.rules, e.size, e.mtime) == (quality::RULES_VERSION, s.0, s.1)).map(|e| e.report.clone())
    }
}

/// The results already known for `paths` (None where a file hasn't been checked, or changed).
#[tauri::command]
pub fn quality_cached(q: State<'_, Quality>, paths: Vec<String>) -> Vec<Option<Report>> {
    paths.iter().map(|p| q.cached(p)).collect()
}

/// Check one track (or return what is known about it). Takes a moment: the whole file is
/// decoded.
#[tauri::command]
pub async fn quality_check(q: State<'_, Quality>, path: String) -> Result<Report, String> {
    if let Some(r) = q.cached(&path) {
        return Ok(r);
    }
    let p = path.clone();
    let report = tauri::async_runtime::spawn_blocking(move || hifi_engine::quality::analyze(Path::new(&p)))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))?;
    if let Some((size, mtime)) = stamp(&path) {
        q.cache.lock().unwrap().insert(path, Entry { rules: quality::RULES_VERSION, size, mtime, report: report.clone() });
        q.dirty.store(true, Ordering::Release);
    }
    Ok(report)
}
