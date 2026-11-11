//! Library state for the app: folders, the scanned index (persisted as JSON), album covers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hifi_engine::UserError;
use hifi_engine::library::{self, Album, TrackInfo, album_id, folder_image, group_albums};
use serde::{Deserialize, Serialize};
use tauri::http::{Response, StatusCode};
use tauri::{AppHandle, Manager, State};

#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    folders: Vec<String>,
    tracks: Vec<TrackInfo>,
    /// Folder → security-scoped bookmark (macOS: what lets the sandboxed build read the folder
    /// again after a relaunch; see bookmarks.rs).
    #[serde(default)]
    bookmarks: HashMap<String, String>,
}

#[derive(Default)]
struct Inner {
    folders: Vec<String>,
    bookmarks: HashMap<String, String>,
    tracks: Arc<Vec<TrackInfo>>,
    albums: Arc<Vec<Album>>,
    /// (done, total) while a scan runs.
    progress: Option<(usize, usize)>,
    failed: Vec<(String, String)>,
    /// Bumped after every scan so the UI knows to refetch.
    revision: u64,
}

#[derive(Default, Clone)]
pub struct Library(Arc<Mutex<Inner>>);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryStatus {
    folders: Vec<String>,
    scanning: Option<(usize, usize)>,
    albums: usize,
    tracks: usize,
    failed: usize,
    revision: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumDto {
    #[serde(flatten)]
    album: Album,
    has_cover: bool,
    track_count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackDto {
    #[serde(flatten)]
    track: TrackInfo,
    album_id: String,
    hi_res: bool,
}

fn index_path(app: &AppHandle) -> Option<PathBuf> {
    Some(app.path().app_data_dir().ok()?.join("library.json"))
}

fn covers_dir(app: &AppHandle) -> Option<PathBuf> {
    let d = app.path().app_cache_dir().ok()?.join("covers");
    std::fs::create_dir_all(&d).ok()?;
    Some(d)
}

#[cfg(target_os = "ios")]
fn phone_music_dirs(app: &AppHandle) -> Vec<String> {
    music_dir(app).into_iter().collect()
}
#[cfg(target_os = "android")]
fn phone_music_dirs(_app: &AppHandle) -> Vec<String> {
    crate::android_media::music_dirs()
}
#[cfg(desktop)]
fn phone_music_dirs(_app: &AppHandle) -> Vec<String> {
    Vec::new()
}

/// Where a phone saves exported playlists: the folder the user reaches from outside the app
/// (iPhone: Files → BitSpring; Android: the app's own Music folder). Desktop: Downloads.
pub fn export_dir(app: &AppHandle) -> Option<PathBuf> {
    if cfg!(mobile) {
        phone_music_dirs(app).last().map(PathBuf::from)
    } else {
        app.path().download_dir().ok()
    }
}

/// The app's Documents folder, where music copied to the phone lives.
fn music_dir(app: &AppHandle) -> Option<String> {
    let d = app.path().document_dir().ok()?;
    std::fs::create_dir_all(&d).ok()?;
    Some(d.to_string_lossy().into_owned())
}

/// Open the saved bookmarks so the folders can be read (and scanned) again. A folder that was
/// moved or renamed since comes back under its new path.
fn reopen_folders(g: &mut Inner) {
    let saved = std::mem::take(&mut g.bookmarks);
    for (folder, bookmark) in saved {
        match crate::bookmarks::open(&bookmark) {
            Some((now, fresh)) => {
                if now != folder {
                    for f in g.folders.iter_mut().filter(|f| **f == folder) {
                        *f = now.clone();
                    }
                }
                g.bookmarks.insert(now, fresh.unwrap_or(bookmark));
            }
            // Gone for now (an unplugged drive): keep it, it may open next time.
            None => {
                g.bookmarks.insert(folder, bookmark);
            }
        }
    }
}

impl Library {
    /// Load the saved index, then refresh it in the background (new or changed files only).
    pub fn load(app: &AppHandle) -> Self {
        let lib = Library::default();
        if let Some(saved) = index_path(app).and_then(|p| hifi_engine::store::load_json::<Persisted>(&p)) {
            let mut g = lib.0.lock().unwrap();
            g.albums = Arc::new(group_albums(&saved.tracks));
            g.tracks = Arc::new(saved.tracks);
            g.folders = saved.folders;
            g.bookmarks = saved.bookmarks;
            reopen_folders(&mut g);
        }
        // On a phone the library is fixed folders, not ones the user picks: on iPhone the app's
        // own (Files → On My iPhone → BitSpring, or Finder file sharing) — the only one the sandbox
        // lets us keep; on Android the shared Music folder plus the app's own.
        if cfg!(mobile) {
            let dirs = phone_music_dirs(app);
            if !dirs.is_empty() {
                lib.0.lock().unwrap().folders = dirs;
            }
        }
        lib.rescan(app.clone());
        lib
    }

    /// Rescan from outside a command (e.g. storage permission just granted).
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn rescan_now(&self, app: AppHandle) {
        self.rescan(app);
    }

    fn rescan(&self, app: AppHandle) {
        let (folders, previous, bookmarks) = {
            let mut g = self.0.lock().unwrap();
            if g.progress.is_some() {
                return; // one scan at a time
            }
            g.progress = Some((0, 0));
            (g.folders.iter().map(PathBuf::from).collect::<Vec<_>>(), g.tracks.clone(), g.bookmarks.clone())
        };
        let lib = self.clone();
        std::thread::spawn(move || {
            let covers = covers_dir(&app);
            let result = library::scan(
                &folders,
                &previous,
                |done, total| lib.0.lock().unwrap().progress = Some((done, total)),
                |t, (mime, bytes)| {
                    if let Some(dir) = &covers {
                        let ext = if mime.contains("png") { "png" } else { "jpg" };
                        let file = dir.join(format!("{}.{ext}", album_id(t)));
                        if !file.exists() {
                            let _ = std::fs::write(file, bytes);
                        }
                    }
                },
            );
            let saved = Persisted {
                folders: folders.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
                tracks: result.tracks.clone(),
                bookmarks,
            };
            if let Some(p) = index_path(&app) {
                // Best effort: a failed write only means the next launch starts from an older index.
                let _ = hifi_engine::store::save_json(&p, &saved);
            }
            let mut g = lib.0.lock().unwrap();
            g.albums = Arc::new(group_albums(&result.tracks));
            g.tracks = Arc::new(result.tracks);
            g.failed = result.failed;
            g.progress = None;
            g.revision += 1;
        });
    }

    /// The scanned tracks as they are now.
    pub fn tracks(&self) -> Arc<Vec<TrackInfo>> {
        self.0.lock().unwrap().tracks.clone()
    }

    fn status(&self) -> LibraryStatus {
        let g = self.0.lock().unwrap();
        LibraryStatus {
            folders: g.folders.clone(),
            scanning: g.progress,
            albums: g.albums.len(),
            tracks: g.tracks.len(),
            failed: g.failed.len(),
            revision: g.revision,
        }
    }

    /// Serve `cover://localhost/<album id>`: cached embedded art, else a cover.jpg-style file
    /// next to the tracks, else 404 (the UI draws its own placeholder).
    pub fn cover_response(&self, app: &AppHandle, path: &str) -> Response<Vec<u8>> {
        let id = path.trim_start_matches('/');
        let not_found = || Response::builder().status(StatusCode::NOT_FOUND).body(Vec::new()).unwrap();
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            return not_found();
        }
        let cached = covers_dir(app).and_then(|d| ["jpg", "png"].iter().map(|e| d.join(format!("{id}.{e}"))).find(|p| p.is_file()));
        let file = cached.or_else(|| {
            let g = self.0.lock().unwrap();
            let album = g.albums.iter().find(|a| a.id == id)?;
            album.tracks.iter().find_map(|&i| folder_image(Path::new(&g.tracks[i].path)))
        });
        match file.and_then(|f| std::fs::read(&f).ok().map(|b| (f, b))) {
            Some((f, bytes)) => Response::builder()
                .header("Content-Type", if f.extension().is_some_and(|e| e == "png") { "image/png" } else { "image/jpeg" })
                .header("Cache-Control", "max-age=31536000")
                .body(bytes)
                .unwrap(),
            None => not_found(),
        }
    }
}

#[tauri::command]
pub fn library_status(lib: State<'_, Library>) -> LibraryStatus {
    lib.status()
}

#[tauri::command(async)]
pub fn library_albums(app: AppHandle, lib: State<'_, Library>) -> Vec<AlbumDto> {
    let (albums, tracks) = {
        let g = lib.0.lock().unwrap();
        (g.albums.clone(), g.tracks.clone())
    };
    let covers = covers_dir(&app);
    let cached: std::collections::HashSet<String> = covers
        .and_then(|d| std::fs::read_dir(d).ok())
        .map(|it| it.filter_map(|e| e.ok()?.path().file_stem().map(|s| s.to_string_lossy().into_owned())).collect())
        .unwrap_or_default();
    albums
        .iter()
        .map(|a| AlbumDto {
            has_cover: cached.contains(&a.id)
                || a.tracks.first().is_some_and(|&i| folder_image(Path::new(&tracks[i].path)).is_some()),
            track_count: a.tracks.len(),
            album: a.clone(),
        })
        .collect()
}

#[tauri::command(async)]
pub fn library_tracks(lib: State<'_, Library>) -> Vec<TrackDto> {
    let (albums, tracks) = {
        let g = lib.0.lock().unwrap();
        (g.albums.clone(), g.tracks.clone())
    };
    let mut owner: HashMap<usize, &str> = HashMap::new();
    for a in albums.iter() {
        for &i in &a.tracks {
            owner.insert(i, &a.id);
        }
    }
    tracks
        .iter()
        .enumerate()
        .map(|(i, t)| TrackDto { album_id: owner.get(&i).unwrap_or(&"").to_string(), hi_res: t.hi_res(), track: t.clone() })
        .collect()
}

#[tauri::command]
pub fn library_add_folder(app: AppHandle, lib: State<'_, Library>, path: String) -> LibraryStatus {
    {
        let mut g = lib.0.lock().unwrap();
        if !g.folders.contains(&path) {
            // Made now, while the open panel's permission is live: the only moment it can be.
            if let Some(b) = crate::bookmarks::create(&path) {
                g.bookmarks.insert(path.clone(), b);
            }
            g.folders.push(path);
        }
    }
    lib.rescan(app);
    lib.status()
}

#[tauri::command]
pub fn library_remove_folder(app: AppHandle, lib: State<'_, Library>, path: String) -> LibraryStatus {
    {
        let mut g = lib.0.lock().unwrap();
        g.folders.retain(|f| *f != path);
        g.bookmarks.remove(&path);
    }
    lib.rescan(app);
    lib.status()
}

#[tauri::command]
pub fn library_rescan(app: AppHandle, lib: State<'_, Library>) -> LibraryStatus {
    lib.rescan(app);
    lib.status()
}

/// Copy files picked from the Files app into the library folder (the picker hands us
/// temporary copies that iOS deletes later). Returns the kept copies, in the given order.
#[tauri::command(async)]
pub fn import_files(app: AppHandle, lib: State<'_, Library>, paths: Vec<String>) -> Result<Vec<String>, UserError> {
    let dir = PathBuf::from(music_dir(&app).ok_or(UserError::NoAppFolder)?);
    let mut kept = Vec::new();
    for p in paths.iter().map(PathBuf::from) {
        let Some(name) = p.file_name() else { continue };
        if p.starts_with(&dir) {
            kept.push(p.to_string_lossy().into_owned());
            continue;
        }
        let size = std::fs::metadata(&p).map(|m| m.len()).ok();
        // The same file imported twice is kept once; a different file with the same name
        // gets a number.
        let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        let mut target = dir.join(name);
        let mut n = 2;
        while target.exists() && std::fs::metadata(&target).map(|m| m.len()).ok() != size {
            target = dir.join(format!("{stem} {n}{ext}"));
            n += 1;
        }
        if !target.exists() {
            std::fs::copy(&p, &target).map_err(|e| UserError::CopyFailed { name: name.to_string_lossy().into_owned(), detail: e.to_string() })?;
            let _ = std::fs::remove_file(&p);
        }
        kept.push(target.to_string_lossy().into_owned());
    }
    lib.rescan(app);
    Ok(kept)
}
