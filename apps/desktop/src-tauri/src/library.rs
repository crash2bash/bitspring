//! Library state for the app: folders, the scanned index (persisted as JSON), album covers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hifi_engine::library::{self, Album, TrackInfo, album_id, folder_image, group_albums};
use serde::{Deserialize, Serialize};
use tauri::http::{Response, StatusCode};
use tauri::{AppHandle, Manager, State};

#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    folders: Vec<String>,
    tracks: Vec<TrackInfo>,
}

#[derive(Default)]
struct Inner {
    folders: Vec<String>,
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

impl Library {
    /// Load the saved index, then refresh it in the background (new or changed files only).
    pub fn load(app: &AppHandle) -> Self {
        let lib = Library::default();
        if let Some(p) = index_path(app).and_then(|p| std::fs::read(p).ok())
            && let Ok(saved) = serde_json::from_slice::<Persisted>(&p)
        {
            let mut g = lib.0.lock().unwrap();
            g.albums = Arc::new(group_albums(&saved.tracks));
            g.tracks = Arc::new(saved.tracks);
            g.folders = saved.folders;
        }
        lib.rescan(app.clone());
        lib
    }

    fn rescan(&self, app: AppHandle) {
        let (folders, previous) = {
            let mut g = self.0.lock().unwrap();
            if g.progress.is_some() {
                return; // one scan at a time
            }
            g.progress = Some((0, 0));
            (g.folders.iter().map(PathBuf::from).collect::<Vec<_>>(), g.tracks.clone())
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
            let saved = Persisted { folders: folders.iter().map(|p| p.to_string_lossy().into_owned()).collect(), tracks: result.tracks.clone() };
            if let Some(p) = index_path(&app) {
                let _ = std::fs::create_dir_all(p.parent().unwrap());
                let _ = std::fs::write(p, serde_json::to_vec(&saved).unwrap_or_default());
            }
            let mut g = lib.0.lock().unwrap();
            g.albums = Arc::new(group_albums(&result.tracks));
            g.tracks = Arc::new(result.tracks);
            g.failed = result.failed;
            g.progress = None;
            g.revision += 1;
        });
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

#[tauri::command]
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

#[tauri::command]
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
            g.folders.push(path);
        }
    }
    lib.rescan(app);
    lib.status()
}

#[tauri::command]
pub fn library_remove_folder(app: AppHandle, lib: State<'_, Library>, path: String) -> LibraryStatus {
    lib.0.lock().unwrap().folders.retain(|f| *f != path);
    lib.rescan(app);
    lib.status()
}

#[tauri::command]
pub fn library_rescan(app: AppHandle, lib: State<'_, Library>) -> LibraryStatus {
    lib.rescan(app);
    lib.status()
}
