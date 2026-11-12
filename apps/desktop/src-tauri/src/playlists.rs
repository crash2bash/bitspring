//! The user's playlists: kept as JSON next to the library index, imported from other players'
//! and services' files, exported back out.
//!
//! An entry is a file path or a streaming id (`qobuz:track:…`), plus the tags it had when it was
//! added, so a playlist still reads right when a file has moved or a service is signed out.
//! Imported entries the library doesn't have stay in the playlist, marked missing, and are
//! looked for again every time the playlist opens — add the album later and they light up.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use hifi_engine::library::TrackInfo;
use hifi_engine::smart::{self, SmartRules};
use hifi_engine::UserError;
use hifi_engine::playlist::{self, Entry, Found, Index};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::library::Library;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistEntry {
    /// File path or streaming id; None = not found when imported.
    pub uri: Option<String>,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Playlist {
    id: String,
    name: String,
    created: u64,
    updated: u64,
    entries: Vec<PlaylistEntry>,
    /// A smart playlist: its tracks come from these rules, picked afresh from the library
    /// each time (`entries` stays empty).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    smart: Option<SmartRules>,
}

#[derive(Default)]
pub struct Playlists(Mutex<Option<Vec<Playlist>>>);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    id: String,
    name: String,
    count: usize,
    duration: f64,
    updated: u64,
    smart: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryDto {
    #[serde(flatten)]
    entry: PlaylistEntry,
    /// Can be played now (file present; streaming id in a build with that service).
    available: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistDto {
    id: String,
    name: String,
    entries: Vec<EntryDto>,
    smart: Option<SmartRules>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Imported {
    id: String,
    name: String,
    total: usize,
    found: usize,
    /// "Artist – Title" of what wasn't found (first 100).
    missing: Vec<String>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn new_id() -> String {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("{t:x}{:x}", N.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

fn store_path(app: &AppHandle) -> Option<PathBuf> {
    Some(app.path().app_data_dir().ok()?.join("playlists.json"))
}

impl Playlists {
    fn with<R>(&self, app: &AppHandle, f: impl FnOnce(&mut Vec<Playlist>) -> R) -> R {
        let mut g = self.0.lock().unwrap();
        let list = g.get_or_insert_with(|| {
            store_path(app).and_then(|p| hifi_engine::store::load_json(&p)).unwrap_or_default()
        });
        f(list)
    }

    /// Change the playlists and save them (written to a temporary file first, so a crash
    /// mid-write never leaves a half file).
    fn change<R>(&self, app: &AppHandle, f: impl FnOnce(&mut Vec<Playlist>) -> R) -> Result<R, UserError> {
        self.with(app, |list| {
            let r = f(list);
            let path = store_path(app).ok_or(UserError::NoAppFolder)?;
            hifi_engine::store::save_json(&path, list).map_err(|e| UserError::WriteFailed { detail: e.to_string() })?;
            Ok(r)
        })
    }
}

fn summary(p: &Playlist) -> Summary {
    Summary {
        id: p.id.clone(),
        name: p.name.clone(),
        count: p.entries.len(),
        duration: p.entries.iter().filter_map(|e| e.duration).sum(),
        updated: p.updated,
        smart: p.smart.is_some(),
    }
}

/// A smart playlist's tracks now, as entries.
fn smart_entries(app: &AppHandle, rules: &SmartRules, tracks: &[TrackInfo]) -> Vec<PlaylistEntry> {
    let quality = app.state::<crate::quality::Quality>();
    let word = |t: &TrackInfo| quality.word(&t.path);
    smart::select(tracks, rules, &word, now())
        .into_iter()
        .map(|i| {
            let t = &tracks[i];
            PlaylistEntry {
                uri: Some(t.path.clone()),
                title: t.title.clone(),
                artist: t.artist.clone(),
                album: t.album.clone(),
                duration: Some(t.duration),
                isrc: None,
            }
        })
        .collect()
}

/// A playlist with a smart one's tracks filled in.
fn resolved(app: &AppHandle, p: &Playlist, tracks: &[TrackInfo]) -> Playlist {
    let mut p = p.clone();
    if let Some(rules) = &p.smart {
        p.entries = smart_entries(app, rules, tracks);
    }
    p
}

fn label(e: &PlaylistEntry) -> String {
    match &e.artist {
        Some(a) => format!("{a} – {}", e.title),
        None => e.title.clone(),
    }
}

fn is_stream(uri: &str) -> bool {
    crate::stream::is_uri(uri)
}

/// The paths the library has scanned (its own folders, network shares included).
type Known<'a> = std::collections::HashSet<&'a str>;

fn known(tracks: &[TrackInfo]) -> Known<'_> {
    tracks.iter().map(|t| t.path.as_str()).collect()
}

/// Playable here and now. A network path counts only if the library scanned it: one from an
/// imported playlist is never looked up (see `playlist::is_network_path`).
fn available(uri: &str, library: &Known) -> bool {
    if is_stream(uri) {
        crate::stream::ENABLED
    } else if playlist::is_network_path(uri) {
        library.contains(uri)
    } else {
        Path::new(uri).is_file()
    }
}

fn from_library(t: &TrackInfo) -> PlaylistEntry {
    PlaylistEntry {
        uri: Some(t.path.clone()),
        title: t.title.clone(),
        artist: t.artist.clone().or_else(|| t.album_artist.clone()),
        album: t.album.clone(),
        duration: Some(t.duration).filter(|d| *d > 0.0),
        isrc: None,
    }
}

fn as_entry(e: &PlaylistEntry) -> Entry {
    Entry {
        location: e.uri.clone(),
        title: Some(e.title.clone()),
        artist: e.artist.clone(),
        album: e.album.clone(),
        duration: e.duration,
        isrc: e.isrc.clone(),
    }
}

/// Turn parsed entries into playlist entries, finding each in the library.
fn resolve(entries: &[Entry], tracks: &[TrackInfo], base: Option<&Path>) -> Vec<PlaylistEntry> {
    let index = Index::new(tracks);
    playlist::resolve(entries, &index, base)
        .into_iter()
        .zip(entries)
        .map(|(found, e)| {
            let fallback_title = || {
                e.location
                    .as_deref()
                    .and_then(|l| Path::new(l).file_stem().map(|s| s.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| "Untitled".into())
            };
            let own = |uri: Option<String>| PlaylistEntry {
                uri,
                title: e.title.clone().unwrap_or_else(fallback_title),
                artist: e.artist.clone(),
                album: e.album.clone(),
                duration: e.duration,
                isrc: e.isrc.clone(),
            };
            match found {
                Some(Found::Library(i)) => PlaylistEntry { isrc: e.isrc.clone(), ..from_library(&tracks[i]) },
                Some(Found::File(p)) => own(Some(p)),
                Some(Found::Stream(uri)) => own(Some(uri)),
                None => own(None),
            }
        })
        .collect()
}

/// Missing or moved entries looked up again in the current library.
fn heal(entries: &mut [PlaylistEntry], tracks: &[TrackInfo]) -> bool {
    let lib = known(tracks);
    let lost: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.uri.as_deref().is_none_or(|u| !is_stream(u) && !available(u, &lib)))
        .map(|(i, _)| i)
        .collect();
    if lost.is_empty() || tracks.is_empty() {
        return false;
    }
    let index = Index::new(tracks);
    let parsed: Vec<Entry> = lost.iter().map(|&i| as_entry(&entries[i])).collect();
    let mut changed = false;
    for (&i, found) in lost.iter().zip(playlist::resolve(&parsed, &index, None)) {
        if let Some(Found::Library(t)) = found {
            entries[i] = PlaylistEntry { isrc: entries[i].isrc.take(), ..from_library(&tracks[t]) };
            changed = true;
        }
    }
    changed
}

/// Entries the UI made from file paths alone (the queue) get the library's tags.
fn enrich(entries: Vec<PlaylistEntry>, tracks: &[TrackInfo]) -> Vec<PlaylistEntry> {
    let by_path: std::collections::HashMap<&str, &TrackInfo> = tracks.iter().map(|t| (t.path.as_str(), t)).collect();
    entries
        .into_iter()
        .map(|e| match e.uri.as_deref().and_then(|u| by_path.get(u)) {
            Some(t) => PlaylistEntry { isrc: e.isrc, ..from_library(t) },
            None => e,
        })
        .collect()
}

fn dto(p: &Playlist, lib: &Known) -> PlaylistDto {
    PlaylistDto {
        id: p.id.clone(),
        name: p.name.clone(),
        entries: p
            .entries
            .iter()
            .map(|e| EntryDto { available: e.uri.as_deref().is_some_and(|u| available(u, lib)), entry: e.clone() })
            .collect(),
        smart: p.smart.clone(),
    }
}

/// A name not yet taken ("Jazz", "Jazz 2", …).
fn unique_name(list: &[Playlist], name: &str) -> String {
    let base = name.trim();
    let base = if base.is_empty() { "New playlist" } else { base };
    let mut n = 1;
    loop {
        let candidate = if n == 1 { base.to_string() } else { format!("{base} {n}") };
        if !list.iter().any(|p| p.name.eq_ignore_ascii_case(&candidate)) {
            return candidate;
        }
        n += 1;
    }
}

fn add_imported(app: &AppHandle, store: &Playlists, parsed: Vec<playlist::Parsed>, tracks: &[TrackInfo], base: Option<&Path>, fallback: &str) -> Result<Vec<Imported>, UserError> {
    let mut made = Vec::new();
    for p in parsed {
        if p.entries.is_empty() {
            continue;
        }
        let mut entries = resolve(&p.entries, tracks, base);
        crate::stream::fill_tags(&mut entries);
        let found = entries.iter().filter(|e| e.uri.is_some()).count();
        let missing = entries.iter().filter(|e| e.uri.is_none()).take(100).map(label).collect();
        let total = entries.len();
        let (id, name) = store.change(app, |list| {
            let name = unique_name(list, p.name.as_deref().unwrap_or(fallback));
            let t = now();
            let id = new_id();
            list.push(Playlist { id: id.clone(), name: name.clone(), created: t, updated: t, entries, smart: None });
            (id, name)
        })?;
        made.push(Imported { id, name, total, found, missing });
    }
    if made.is_empty() {
        return Err(UserError::PlaylistNoTracks);
    }
    Ok(made)
}

/// A new playlist from entries made elsewhere (a service playlist copied over).
#[cfg_attr(not(all(feature = "streaming", not(target_os = "ios"))), allow(dead_code))]
pub fn add_playlist(app: &AppHandle, name: &str, entries: Vec<PlaylistEntry>) -> Result<Imported, UserError> {
    let store = app.state::<Playlists>();
    let total = entries.len();
    let found = entries.iter().filter(|e| e.uri.is_some()).count();
    let missing = entries.iter().filter(|e| e.uri.is_none()).take(100).map(label).collect();
    let (id, name) = store.change(app, |list| {
        let name = unique_name(list, name);
        let t = now();
        let id = new_id();
        list.push(Playlist { id: id.clone(), name: name.clone(), created: t, updated: t, entries, smart: None });
        (id, name)
    })?;
    Ok(Imported { id, name, total, found, missing })
}

/// Offer each missing entry of a playlist to `find`; keep what it finds. Returns how many.
#[cfg_attr(not(all(feature = "streaming", not(target_os = "ios"))), allow(dead_code))]
pub fn fill_missing(app: &AppHandle, id: &str, mut find: impl FnMut(&PlaylistEntry) -> Option<PlaylistEntry>) -> Result<usize, UserError> {
    let store = app.state::<Playlists>();
    let tracks = app.state::<Library>().tracks();
    let lib = known(&tracks);
    let missing: Vec<(usize, PlaylistEntry)> = store.with(app, |list| {
        let p = list.iter().find(|p| p.id == id).ok_or(UserError::PlaylistNotFound)?;
        Ok::<_, UserError>(p.entries.iter().cloned().enumerate().filter(|(_, e)| e.uri.as_deref().is_none_or(|u| !available(u, &lib))).collect())
    })?;
    // Network lookups happen outside the lock; the results go in by position afterwards.
    let found: Vec<(usize, PlaylistEntry)> = missing.into_iter().filter_map(|(i, e)| find(&e).map(|f| (i, f))).collect();
    let n = found.len();
    if n > 0 {
        store.change(app, |list| {
            if let Some(p) = list.iter_mut().find(|p| p.id == id) {
                for (i, e) in found {
                    if let Some(slot) = p.entries.get_mut(i) {
                        *slot = e;
                    }
                }
                p.updated = now();
            }
        })?;
    }
    Ok(n)
}

// ---------------------------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------------------------

#[tauri::command]
pub fn playlists_list(app: AppHandle, store: State<'_, Playlists>, lib: State<'_, Library>) -> Vec<Summary> {
    let tracks = lib.tracks();
    store.with(&app, |list| {
        let mut v: Vec<Summary> = list.iter().map(|p| summary(&resolved(&app, p, &tracks))).collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.updated));
        v
    })
}

#[tauri::command]
pub fn playlist_get(app: AppHandle, store: State<'_, Playlists>, lib: State<'_, Library>, id: String) -> Result<PlaylistDto, UserError> {
    let tracks = lib.tracks();
    let healed = store.with(&app, |list| {
        let p = list.iter_mut().find(|p| p.id == id).ok_or(UserError::PlaylistNotFound)?;
        Ok::<_, UserError>(heal(&mut p.entries, &tracks))
    })?;
    if healed {
        store.change(&app, |_| ())?;
    }
    store.with(&app, |list| list.iter().find(|p| p.id == id).map(|p| dto(&resolved(&app, p, &tracks), &known(&tracks))).ok_or(UserError::PlaylistNotFound))
}

#[tauri::command]
pub fn playlist_create(
    app: AppHandle,
    store: State<'_, Playlists>,
    lib: State<'_, Library>,
    name: String,
    entries: Vec<PlaylistEntry>,
    smart: Option<SmartRules>,
) -> Result<Summary, UserError> {
    let tracks = lib.tracks();
    let entries = if smart.is_some() { Vec::new() } else { enrich(entries, &tracks) };
    store.change(&app, |list| {
        let t = now();
        let p = Playlist { id: new_id(), name: unique_name(list, &name), created: t, updated: t, entries, smart };
        let s = summary(&resolved(&app, &p, &tracks));
        list.push(p);
        s
    })
}

/// Rename and/or replace the entries (reorder, remove).
#[tauri::command]
pub fn playlist_update(
    app: AppHandle,
    store: State<'_, Playlists>,
    lib: State<'_, Library>,
    id: String,
    name: Option<String>,
    entries: Option<Vec<PlaylistEntry>>,
    smart: Option<SmartRules>,
) -> Result<Summary, UserError> {
    let tracks = lib.tracks();
    store.change(&app, |list| {
        let taken: Vec<Playlist> = list.iter().filter(|p| p.id != id).cloned().collect();
        let p = list.iter_mut().find(|p| p.id == id).ok_or(UserError::PlaylistNotFound)?;
        if let Some(n) = name.filter(|n| !n.trim().is_empty() && n.trim() != p.name) {
            p.name = unique_name(&taken, &n);
        }
        if let Some(e) = entries.filter(|_| p.smart.is_none()) {
            p.entries = e;
        }
        if smart.is_some() && p.smart.is_some() {
            p.smart = smart;
        }
        p.updated = now();
        Ok(summary(&resolved(&app, p, &tracks)))
    })?
}

#[tauri::command]
pub fn playlist_add(app: AppHandle, store: State<'_, Playlists>, lib: State<'_, Library>, id: String, entries: Vec<PlaylistEntry>) -> Result<Summary, UserError> {
    let entries = enrich(entries, &lib.tracks());
    store.change(&app, |list| {
        let p = list.iter_mut().find(|p| p.id == id).ok_or(UserError::PlaylistNotFound)?;
        if p.smart.is_some() {
            return Err(UserError::PlaylistNotFound);
        }
        p.entries.extend(entries);
        p.updated = now();
        Ok(summary(p))
    })?
}

#[tauri::command]
pub fn playlist_delete(app: AppHandle, store: State<'_, Playlists>, id: String) -> Result<(), UserError> {
    store.change(&app, |list| list.retain(|p| p.id != id))
}

/// Import a playlist file (M3U, PLS, XSPF, WPL, iTunes XML, CSV, Apple Music TXT…).
#[tauri::command]
pub async fn playlist_import_file(app: AppHandle, path: String) -> Result<Vec<Imported>, UserError> {
    tauri::async_runtime::spawn_blocking(move || {
        let p = PathBuf::from(&path);
        let bytes = std::fs::read(&p).map_err(|e| UserError::PlaylistUnreadable { detail: e.to_string() })?;
        if bytes.len() > 64 << 20 {
            return Err(UserError::PlaylistUnreadable { detail: "file too large".into() });
        }
        let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let parsed = playlist::parse(&bytes, &name).map_err(|e| UserError::PlaylistUnreadable { detail: format!("{e:#}") })?;
        let tracks = app.state::<Library>().tracks();
        let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        add_imported(&app, &app.state::<Playlists>(), parsed, &tracks, p.parent(), &stem)
    })
    .await
    .map_err(UserError::other)?
}

/// Import pasted text: "Artist - Title" lines, file paths, Qobuz/TIDAL track links, or the
/// contents of any supported playlist file.
#[tauri::command]
pub async fn playlist_import_text(app: AppHandle, text: String, name: String) -> Result<Vec<Imported>, UserError> {
    tauri::async_runtime::spawn_blocking(move || {
        let parsed = playlist::parse_text(&text).map_err(|e| UserError::PlaylistUnreadable { detail: format!("{e:#}") })?;
        let tracks = app.state::<Library>().tracks();
        add_imported(&app, &app.state::<Playlists>(), parsed, &tracks, None, &name)
    })
    .await
    .map_err(UserError::other)?
}

/// Save a playlist as M3U8, PLS, XSPF or CSV. With a `path` the format is its extension; on a
/// phone (no save dialog that hands back a real file) pass `format` alone and the file goes
/// into the app's music folder. Returns where it was written.
#[tauri::command]
pub fn playlist_export(
    app: AppHandle,
    store: State<'_, Playlists>,
    lib: State<'_, Library>,
    id: String,
    path: Option<String>,
    format: Option<String>,
) -> Result<String, UserError> {
    let tracks = lib.tracks();
    let (name, entries) = store.with(&app, |list| {
        list.iter()
            .find(|p| p.id == id)
            .map(|p| resolved(&app, p, &tracks))
            .map(|p| (p.name.clone(), p.entries.iter().map(as_entry).collect::<Vec<_>>()))
            .ok_or(UserError::PlaylistNotFound)
    })?;
    let dest = match path {
        Some(p) => PathBuf::from(p),
        None => {
            let ext = format.as_deref().unwrap_or("m3u8");
            let dir = crate::library::export_dir(&app).ok_or(UserError::NoAppFolder)?;
            let safe: String = name.chars().map(|c| if r#"/\:*?"<>|"#.contains(c) { '_' } else { c }).collect();
            dir.join(format!("{safe}.{ext}"))
        }
    };
    let format = dest
        .extension()
        .and_then(|e| e.to_str())
        .and_then(playlist::Format::from_extension)
        .ok_or(UserError::PlaylistFormat)?;
    let text = playlist::export(&name, &entries, format, Some(&dest));
    std::fs::write(&dest, text).map_err(|e| UserError::WriteFailed { detail: e.to_string() })?;
    Ok(dest.to_string_lossy().into_owned())
}
