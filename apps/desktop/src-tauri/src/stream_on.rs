//! Qobuz and TIDAL in the app — compiled only into builds that don't go to the App Store
//! (cargo feature `streaming`, never on iOS). `stream_off.rs` stands in everywhere else.
//!
//! Sign-in happens on the service's own page: on a computer in a window of its own, on Android
//! in the app's web view for a moment. Either way we wait for the page to send the browser to
//! the service's redirect address and take the code from there; the password never passes
//! through us.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use hifi_engine::{Track, UserError};
use hifi_streaming::{AccountDto, AlbumPage, PlaylistPage, Quality, SAlbum, SPlaylist, STrack, SearchResult, Service, Streaming};
use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::playlists::{self, Imported, PlaylistEntry};

pub const ENABLED: bool = true;

pub fn init(app: &AppHandle) {
    let path = app.path().app_data_dir().map(|d| d.join("streaming.json")).unwrap_or_else(|_| "streaming.json".into());
    app.manage(Streaming::load(path));
}

fn svc(app: &AppHandle) -> tauri::State<'_, Streaming> {
    app.state::<Streaming>()
}

pub fn is_uri(s: &str) -> bool {
    hifi_streaming::is_stream_uri(s)
}

/// Open a streaming track for playback (called from the queue, off the UI thread).
pub fn open(uri: &str) -> anyhow::Result<Arc<Track>> {
    let app = crate::APP.get().ok_or_else(|| anyhow::anyhow!("the app hasn't started yet"))?;
    svc(app).open(uri)
}

type Res<T> = Result<T, UserError>;

async fn blocking<T: Send + 'static>(app: AppHandle, f: impl FnOnce(&Streaming) -> anyhow::Result<T> + Send + 'static) -> Res<T> {
    tauri::async_runtime::spawn_blocking(move || f(&svc(&app)).map_err(|e| UserError::of(&e)))
        .await
        .map_err(UserError::other)?
}

fn parse(service: &str) -> Res<Service> {
    Service::parse(service).map_err(|e| UserError::of(&e))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamingStatus {
    available: bool,
    accounts: Vec<AccountDto>,
    quality: Quality,
}

#[tauri::command]
pub fn streaming_status(app: AppHandle) -> StreamingStatus {
    let s = svc(&app);
    StreamingStatus { available: true, accounts: s.accounts(), quality: s.quality() }
}

#[tauri::command]
pub fn streaming_set_quality(app: AppHandle, quality: Quality) {
    svc(&app).set_quality(quality);
}

#[tauri::command]
pub fn streaming_logout(app: AppHandle, service: String) -> Res<()> {
    svc(&app).logout(parse(&service)?);
    Ok(())
}

// --- sign-in -----------------------------------------------------------------------------------

/// The sign-in in progress on Android, where it runs in the main web view.
#[cfg(target_os = "android")]
pub(crate) static MAIN_LOGIN: std::sync::Mutex<Option<(String, mpsc::Sender<Option<String>>, tauri::Url)>> = std::sync::Mutex::new(None);

/// Android: the main web view's navigation hook. True = let it navigate.
#[cfg(target_os = "android")]
pub fn main_navigation(url: &tauri::Url) -> bool {
    let mut g = MAIN_LOGIN.lock().unwrap();
    let hit = g.as_ref().is_some_and(|(redirect, ..)| url.as_str().starts_with(redirect.as_str()));
    if !hit {
        return true;
    }
    let (_, tx, back) = g.take().unwrap();
    let _ = tx.send(Some(url.to_string()));
    // Back to the app (it reloads; everything that matters lives in Rust).
    if let Some(w) = crate::APP.get().and_then(|a| a.get_webview_window("main")) {
        let _ = w.navigate(back);
    }
    false
}

/// Show `url` and wait until it redirects to `redirect`; None = the user closed it.
fn wait_for_redirect(
    app: &AppHandle,
    title: &str,
    url: &str,
    redirect: &str,
    tx: mpsc::Sender<Option<String>>,
    rx: mpsc::Receiver<Option<String>>,
) -> Res<Option<String>> {
    let url: tauri::Url = url.parse().map_err(UserError::other)?;
    #[cfg(desktop)]
    {
        use tauri::{WebviewUrl, WebviewWindowBuilder, WindowEvent};
        if let Some(old) = app.get_webview_window("streaming-login") {
            let _ = old.destroy();
        }
        let redirect_nav = redirect.to_string();
        let redirect_load = redirect.to_string();
        let tx_nav = tx.clone();
        let tx_load = tx.clone();
        let tx_closed = tx.clone();
        let win = WebviewWindowBuilder::new(app, "streaming-login", WebviewUrl::External(url))
            .title(title)
            .inner_size(500.0, 760.0)
            .on_navigation(move |u| {
                if u.as_str().starts_with(&redirect_nav) {
                    let _ = tx_nav.send(Some(u.to_string()));
                    false
                } else {
                    true
                }
            })
            // Some redirects only show up as a loaded page.
            .on_page_load(move |_, payload| {
                if payload.url().as_str().starts_with(&redirect_load) {
                    let _ = tx_load.send(Some(payload.url().to_string()));
                }
            })
            .build()
            .map_err(UserError::other)?;
        win.on_window_event(move |e| {
            if let WindowEvent::Destroyed = e {
                let _ = tx_closed.send(None);
            }
        });
        let got = rx.recv_timeout(Duration::from_secs(15 * 60)).ok().flatten();
        let _ = win.destroy();
        Ok(got)
    }
    #[cfg(target_os = "android")]
    {
        let _ = title;
        let main = app.get_webview_window("main").ok_or_else(|| UserError::other("no main window"))?;
        let back = main.url().map_err(UserError::other)?;
        *MAIN_LOGIN.lock().unwrap() = Some((redirect.to_string(), tx, back));
        main.navigate(url).map_err(UserError::other)?;
        Ok(rx.recv_timeout(Duration::from_secs(15 * 60)).ok().flatten())
    }
}

/// Loopback listeners on one free port, IPv4 and (where there is one) IPv6, held for the whole
/// sign-in. Qobuz's page is sent back to http://localhost:<port>: should a web view ever follow
/// that address instead of letting us catch it, it reaches us — never whatever else might be
/// listening on a fixed port.
fn loopback() -> std::io::Result<(u16, Vec<TcpListener>)> {
    let mut last = None;
    for _ in 0..16 {
        let v4 = TcpListener::bind("127.0.0.1:0")?;
        let port = v4.local_addr()?.port();
        match TcpListener::bind(("::1", port)) {
            Ok(v6) => return Ok((port, vec![v4, v6])),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => last = Some(e),
            Err(_) => return Ok((port, vec![v4])), // no IPv6 here
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no free port")))
}

/// Answer the redirect if it does reach us, and pass its address on — only a request for
/// exactly `redirect` (whose path holds the sign-in's random nonce) with a code in it.
fn serve_redirect(listeners: Vec<TcpListener>, redirect: &str, tx: mpsc::Sender<Option<String>>, stop: Arc<AtomicBool>) {
    let expected = redirect.split_once("://").and_then(|(_, rest)| rest.find('/').map(|i| rest[i..].to_string())).unwrap_or_else(|| "/".into());
    let origin = redirect.trim_end_matches(expected.as_str()).to_string();
    for l in listeners {
        let (tx, stop, expected, origin) = (tx.clone(), stop.clone(), expected.clone(), origin.clone());
        let _ = l.set_nonblocking(true);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match l.accept() {
                    Ok((mut conn, _)) => {
                        let _ = conn.set_nonblocking(false);
                        let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
                        let mut buf = [0u8; 8192];
                        let n = conn.read(&mut buf).unwrap_or(0);
                        let head = String::from_utf8_lossy(&buf[..n]);
                        let path = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/");
                        let body = "<!doctype html><meta charset=utf-8><title>Hifi</title><p style=\"font:16px system-ui;margin:40px\">&#10003;</p>";
                        let _ = write!(conn, "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                        let (route, query) = path.split_once('?').unwrap_or((path, ""));
                        if route == expected && query.split('&').any(|kv| kv.starts_with("code_autorisation=") || kv.starts_with("code=")) {
                            let _ = tx.send(Some(format!("{origin}{path}")));
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(100)),
                    Err(_) => break,
                }
            }
        });
    }
}

#[tauri::command]
pub async fn streaming_login(app: AppHandle, service: String) -> Res<Vec<AccountDto>> {
    let service = parse(&service)?;
    tauri::async_runtime::spawn_blocking(move || {
        let s = svc(&app);
        let (tx, rx) = mpsc::channel::<Option<String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let listeners = match service {
            Service::Qobuz => Some(loopback().map_err(UserError::other)?),
            Service::Tidal => None,
        };
        let result = (|| {
            let start = s.start_login(service, listeners.as_ref().map_or(0, |(port, _)| *port)).map_err(|e| UserError::of(&e))?;
            if let Some((_, ls)) = listeners {
                serve_redirect(ls, &start.redirect, tx.clone(), stop.clone());
            }
            match wait_for_redirect(&app, service.name(), &start.url, &start.redirect, tx, rx)? {
                Some(url) => s.finish_login(service, &url).map_err(|e| UserError::of(&e)),
                None => Err(UserError::SignInCancelled),
            }
        })();
        stop.store(true, Ordering::Relaxed);
        result?;
        Ok(s.accounts())
    })
    .await
    .map_err(UserError::other)?
}

// --- catalogue ---------------------------------------------------------------------------------

#[tauri::command]
pub async fn streaming_search(app: AppHandle, service: String, query: String) -> Res<SearchResult> {
    let service = parse(&service)?;
    blocking(app, move |s| s.search(service, &query)).await
}

#[tauri::command]
pub async fn streaming_album(app: AppHandle, service: String, id: String) -> Res<AlbumPage> {
    let service = parse(&service)?;
    blocking(app, move |s| s.album(service, &id)).await
}

#[tauri::command]
pub async fn streaming_favorites(app: AppHandle, service: String) -> Res<Vec<SAlbum>> {
    let service = parse(&service)?;
    blocking(app, move |s| s.favorite_albums(service)).await
}

#[tauri::command]
pub async fn streaming_playlists(app: AppHandle, service: String) -> Res<Vec<SPlaylist>> {
    let service = parse(&service)?;
    blocking(app, move |s| s.playlists(service)).await
}

#[tauri::command]
pub async fn streaming_playlist(app: AppHandle, service: String, id: String) -> Res<PlaylistPage> {
    let service = parse(&service)?;
    blocking(app, move |s| s.playlist(service, &id)).await
}

fn entry(t: &STrack) -> PlaylistEntry {
    PlaylistEntry {
        uri: Some(t.uri.clone()),
        title: t.title.clone(),
        artist: t.artist.clone(),
        album: t.album.clone(),
        duration: Some(t.duration).filter(|d| *d > 0.0),
        isrc: t.isrc.clone(),
    }
}

/// Copy a service playlist into the app's own playlists.
#[tauri::command]
pub async fn streaming_import_playlist(app: AppHandle, service: String, id: String) -> Res<Imported> {
    let service = parse(&service)?;
    let handle = app.clone();
    let page = blocking(app, move |s| s.playlist(service, &id)).await?;
    let entries: Vec<PlaylistEntry> = page.tracks.iter().map(entry).collect();
    playlists::add_playlist(&handle, &page.playlist.name, entries)
}

/// Look up a playlist's missing entries on a service; returns how many were found.
#[tauri::command]
pub async fn streaming_find_missing(app: AppHandle, playlist_id: String, service: String) -> Res<usize> {
    let service = parse(&service)?;
    tauri::async_runtime::spawn_blocking(move || {
        let s = svc(&app);
        if !s.signed_in(service) {
            return Err(UserError::SignInNeeded { service: service.name().into() });
        }
        let mut first_error = None;
        let found = playlists::fill_missing(&app, &playlist_id, |e| match s.find(service, &e.title, e.artist.as_deref(), e.duration, e.isrc.as_deref()) {
            Ok(t) => t.map(|t| entry(&t)),
            Err(err) => {
                first_error.get_or_insert(UserError::of(&err));
                None
            }
        })?;
        match (found, first_error) {
            (0, Some(e)) => Err(e),
            (n, _) => Ok(n),
        }
    })
    .await
    .map_err(UserError::other)?
}

/// Tags for imported streaming links that came without them (a pasted list of URLs).
pub fn fill_tags(entries: &mut [PlaylistEntry]) {
    let Some(app) = crate::APP.get() else { return };
    let s = svc(app);
    let mut asked = 0;
    for e in entries.iter_mut() {
        let Some(uri) = e.uri.clone().filter(|u| is_uri(u)) else { continue };
        if e.artist.is_some() || asked >= 300 {
            continue;
        }
        let Ok((service, _)) = hifi_streaming::split_uri(&uri) else { continue };
        if !s.signed_in(service) {
            continue;
        }
        asked += 1;
        if let Ok(t) = s.track(&uri) {
            *e = entry(&t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    fn get(port: u16, path: &str) {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(c, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut answer = String::new();
        let _ = c.read_to_string(&mut answer);
    }

    #[test]
    fn redirect_needs_the_nonce_path_and_a_code() {
        let (port, listeners) = loopback().unwrap();
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let redirect = format!("http://localhost:{port}/0123abcd");
        serve_redirect(listeners, &redirect, tx, stop.clone());
        get(port, "/?code_autorisation=evil");
        get(port, "/0123abcdX?code_autorisation=evil");
        get(port, "/0123abcd?state=nothing");
        get(port, "/0123abcd?code_autorisation=good");
        let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(got.as_deref(), Some(format!("{redirect}?code_autorisation=good").as_str()));
        assert!(rx.try_recv().is_err());
        stop.store(true, Ordering::Relaxed);
    }
}
