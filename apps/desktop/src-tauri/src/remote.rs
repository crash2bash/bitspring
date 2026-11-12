//! The phone remote: a small web page the desktop app serves on the local network. A phone
//! opens it (QR code in Settings), browses the library and controls playback.
//!
//! Commands from the phone go to the app's window as `remote` events, and the window carries
//! them out exactly as its own buttons would: one queue, one chosen output, one set of
//! settings. The phone reads state straight from here.
//!
//! Off until the user switches it on. Every request but the page itself needs the token in
//! the address the QR code carries (128 random bits); "New address" changes it.

use std::net::{Ipv4Addr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use hifi_engine::net::http;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::library::Library;
use crate::AppState;

/// The port asked for first, so a bookmark on the phone keeps working.
const PREFERRED_PORT: u16 = 48_173;
const PAGE: &str = include_str!("remote.html");

#[derive(Serialize, Deserialize, Default, Clone)]
struct Config {
    enabled: bool,
    token: String,
    port: u16,
}

#[derive(Default)]
struct Running {
    port: u16,
    stop: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct Remote {
    config: Mutex<Config>,
    running: Mutex<Option<Running>>,
    file: Option<PathBuf>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteStatus {
    enabled: bool,
    /// The address to open on the phone (with the token), while on.
    url: Option<String>,
    /// The same as a QR code (SVG), while on.
    qr: Option<String>,
    /// Why it couldn't start.
    error: Option<String>,
}

impl Remote {
    pub fn load(app: &AppHandle) -> Self {
        let file = app.path().app_data_dir().ok().map(|d| d.join("remote.json"));
        let config: Config = file.as_deref().and_then(hifi_engine::store::load_json).unwrap_or_default();
        let r = Self { config: Mutex::new(config), running: Mutex::new(None), file };
        if r.config.lock().unwrap().enabled {
            let _ = r.start(app);
        }
        r
    }

    fn save(&self) {
        if let Some(f) = &self.file {
            let _ = hifi_engine::store::save_json(f, &*self.config.lock().unwrap());
        }
    }

    fn start(&self, app: &AppHandle) -> std::io::Result<()> {
        let mut running = self.running.lock().unwrap();
        if running.is_some() {
            return Ok(());
        }
        let mut c = self.config.lock().unwrap();
        if c.token.len() < 32 {
            c.token = http::random_hex();
        }
        let wanted = if c.port != 0 { c.port } else { PREFERRED_PORT };
        // The port may take a moment to come free after a restart (new address).
        let mut listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, wanted));
        for _ in 0..10 {
            if listener.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, wanted));
        }
        let listener = listener.or_else(|_| TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)))?;
        let port = listener.local_addr()?.port();
        c.port = port;
        let stop = Arc::new(AtomicBool::new(false));
        let (app2, token) = (app.clone(), c.token.clone());
        http::accept_loop(listener, "hifi-remote", stop.clone(), move |mut conn| {
            let _ = handle(&app2, &token, &mut conn);
        })?;
        *running = Some(Running { port, stop });
        drop(c);
        self.save();
        Ok(())
    }

    fn stop(&self) {
        if let Some(r) = self.running.lock().unwrap().take() {
            r.stop.store(true, Ordering::Release);
            // Wake the accept loop so it sees the flag.
            let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, r.port));
        }
    }

    fn status(&self, error: Option<String>) -> RemoteStatus {
        let c = self.config.lock().unwrap().clone();
        let port = self.running.lock().unwrap().as_ref().map(|r| r.port);
        let url = port.filter(|_| c.enabled).map(|p| format!("http://{}:{p}/?t={}", lan_ip(), c.token));
        let qr = url.as_deref().and_then(qr_svg);
        RemoteStatus { enabled: c.enabled, url, qr, error }
    }
}

/// This computer's address on the local network (the interface used to go anywhere).
fn lan_ip() -> String {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|s| s.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).map(|_| s))
        .and_then(|s| s.local_addr())
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into())
}

fn qr_svg(url: &str) -> Option<String> {
    let code = qrcode::QrCode::with_error_correction_level(url.as_bytes(), qrcode::EcLevel::M).ok()?;
    Some(code.render::<qrcode::render::svg::Color>().min_dimensions(200, 200).quiet_zone(true).build())
}

#[tauri::command]
pub fn remote_status(remote: State<'_, Remote>) -> RemoteStatus {
    remote.status(None)
}

#[tauri::command]
pub fn remote_enable(app: AppHandle, remote: State<'_, Remote>, enabled: bool) -> RemoteStatus {
    remote.config.lock().unwrap().enabled = enabled;
    remote.save();
    let error = if enabled { remote.start(&app).err().map(|e| e.to_string()) } else { None };
    if !enabled {
        remote.stop();
    }
    remote.status(error)
}

/// A new token: the old address (and every phone that has it) stops working.
#[tauri::command]
pub fn remote_new_address(app: AppHandle, remote: State<'_, Remote>) -> RemoteStatus {
    remote.stop();
    remote.config.lock().unwrap().token = http::random_hex();
    remote.save();
    let enabled = remote.config.lock().unwrap().enabled;
    let error = if enabled { remote.start(&app).err().map(|e| e.to_string()) } else { None };
    remote.status(error)
}

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

fn handle(app: &AppHandle, token: &str, conn: &mut TcpStream) -> anyhow::Result<()> {
    conn.set_write_timeout(Some(std::time::Duration::from_secs(20)))?;
    let req = http::read_request(conn, 1 << 20)?;
    let json_ok = |conn: &mut TcpStream, v: &Value| http::respond(conn, "200 OK", "application/json", &[("Cache-Control", "no-store")], v.to_string().as_bytes());
    // The page and its words need no token: nothing in them is private.
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => {
            return Ok(http::respond(conn, "200 OK", "text/html; charset=utf-8", &[("Cache-Control", "no-cache")], PAGE.as_bytes())?);
        }
        ("GET", p) if p.starts_with("/fonts/") => {
            let font: &[u8] = match p {
                "/fonts/barlow-400.woff2" => include_bytes!("../../node_modules/@fontsource/barlow/files/barlow-latin-400-normal.woff2"),
                "/fonts/barlow-600.woff2" => include_bytes!("../../node_modules/@fontsource/barlow/files/barlow-latin-600-normal.woff2"),
                "/fonts/barlow-sc-600.woff2" => include_bytes!("../../node_modules/@fontsource/barlow-semi-condensed/files/barlow-semi-condensed-latin-600-normal.woff2"),
                _ => return Ok(http::respond(conn, "404 Not Found", "text/plain", &[], b"")?),
            };
            return Ok(http::respond(conn, "200 OK", "font/woff2", &[("Cache-Control", "max-age=604800")], font)?);
        }
        ("GET", p) if p.starts_with("/locales/") => {
            match locale(p.trim_start_matches("/locales/").trim_end_matches(".json")) {
                Some(text) => http::respond(conn, "200 OK", "application/json; charset=utf-8", &[("Cache-Control", "max-age=3600")], text.as_bytes())?,
                None => http::respond(conn, "404 Not Found", "text/plain", &[], b"")?,
            }
            return Ok(());
        }
        _ => {}
    }
    let given = req.headers.get("x-token").or_else(|| req.query.get("t")).map(String::as_str).unwrap_or("");
    if !same(given, token) {
        return Ok(http::respond(conn, "403 Forbidden", "application/json", &[], br#"{"error":"token"}"#)?);
    }
    let state = app.state::<AppState>();
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/state") => {
            let status = crate::status(app.state::<AppState>());
            let (paths, index) = {
                let q = state.queue.lock().unwrap();
                (q.paths.clone(), q.index)
            };
            let lib = app.state::<Library>();
            let known = lib.tracks_by_path(&paths);
            let queue: Vec<Value> = paths
                .iter()
                .map(|p| match known.get(p) {
                    Some(t) => json!({"path": p, "title": t.title, "artist": t.artist, "duration": t.duration, "albumId": t.album_id}),
                    None => json!({"path": p, "title": file_title(p)}),
                })
                .collect();
            let output = state.current.lock().unwrap().as_ref().map(|c| c.device_uid.clone());
            json_ok(conn, &json!({"status": status, "queue": queue, "index": index, "network": output.as_deref().is_some_and(hifi_engine::net::is_network_uid)}))?;
        }
        ("GET", "/api/now") => {
            let now = crate::now_playing(app.state::<AppState>());
            let album_id = now.as_ref().and_then(|n| app.state::<Library>().tracks_by_path(std::slice::from_ref(&n.path)).remove(&n.path)).map(|t| t.album_id);
            let mut v = serde_json::to_value(&now)?;
            if let (Some(obj), Some(id)) = (v.as_object_mut(), album_id) {
                obj.insert("albumId".into(), json!(id));
            }
            json_ok(conn, &v)?;
        }
        ("GET", "/api/library") => {
            let albums = crate::library::library_albums(app.clone(), app.state::<Library>());
            let tracks = crate::library::library_tracks(app.state::<Library>());
            json_ok(conn, &json!({"albums": albums, "tracks": tracks}))?;
        }
        ("GET", p) if p.starts_with("/cover/") => {
            let id = p.trim_start_matches("/cover");
            let r = app.state::<Library>().cover_response(app, id);
            let ct = r.headers().get("Content-Type").and_then(|v| v.to_str().ok()).unwrap_or("image/jpeg").to_string();
            if r.status().is_success() {
                http::respond(conn, "200 OK", &ct, &[("Cache-Control", "max-age=86400")], r.body())?;
            } else {
                http::respond(conn, "404 Not Found", "text/plain", &[], b"")?;
            }
        }
        ("GET", "/api/volume") => {
            let uid = state.current.lock().unwrap().as_ref().map(|c| c.device_uid.clone());
            let v = uid.and_then(crate::output_volume);
            json_ok(conn, &json!({"volume": v}))?;
        }
        ("POST", "/api/volume") => {
            let v: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let uid = state.current.lock().unwrap().as_ref().map(|c| c.device_uid.clone());
            if let (Some(uid), Some(vol)) = (uid, v["volume"].as_u64()) {
                crate::set_output_volume(uid, vol.min(100) as u32);
            }
            json_ok(conn, &json!({}))?;
        }
        ("POST", "/api/do") => {
            let cmd: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            match checked(app, &cmd) {
                Some(cmd) => {
                    let _ = app.emit("remote", cmd);
                    json_ok(conn, &json!({}))?;
                }
                None => http::respond(conn, "400 Bad Request", "application/json", &[], br#"{"error":"command"}"#)?,
            }
        }
        _ => http::respond(conn, "404 Not Found", "text/plain", &[], b"")?,
    }
    Ok(())
}

/// A command from the phone, rebuilt from what is expected (and nothing else): tracks only
/// from the library, numbers in range.
fn checked(app: &AppHandle, cmd: &Value) -> Option<Value> {
    let kind = cmd["kind"].as_str()?;
    Some(match kind {
        "toggle" | "next" | "prev" => json!({"kind": kind}),
        "seek" => json!({"kind": kind, "secs": cmd["secs"].as_f64().filter(|s| s.is_finite() && *s >= 0.0)?}),
        "jump" => json!({"kind": kind, "index": cmd["index"].as_u64()?}),
        "play" | "enqueue" => {
            let paths: Vec<String> = cmd["paths"].as_array()?.iter().filter_map(|p| p.as_str().map(str::to_string)).take(5000).collect();
            let known = app.state::<Library>().known_paths();
            if paths.is_empty() || !paths.iter().all(|p| known.contains(p)) {
                return None;
            }
            let index = cmd["index"].as_u64().unwrap_or(0).min(paths.len() as u64 - 1);
            json!({"kind": kind, "paths": paths, "index": index})
        }
        _ => return None,
    })
}

/// Compare without stopping at the first difference.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn file_title(path: &str) -> String {
    Path::new(hifi_engine::cue::split(path).0).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string())
}

/// The UI's own dictionaries, for the page.
fn locale(code: &str) -> Option<&'static str> {
    macro_rules! locales {
        ($($code:literal),*) => {
            match code {
                $($code => Some(include_str!(concat!("../../src/lib/locales/", $code, ".json"))),)*
                _ => None,
            }
        };
    }
    locales!(
        "ar", "cs", "da", "de", "el", "en", "es", "fi", "fr", "hu", "it", "ja", "ko", "nb", "nl", "pl", "pt-br", "ro", "ru", "sv", "tr", "uk",
        "zh-hans", "zh-hant"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_compare_whole() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "ab"));
        assert!(!same("", "a"));
    }

    #[test]
    fn the_address_becomes_a_qr_code() {
        let svg = qr_svg("http://192.168.0.2:48173/?t=0123456789abcdef0123456789abcdef").unwrap();
        assert!(svg.starts_with("<?xml") || svg.starts_with("<svg"), "{}", &svg[..40]);
        assert!(svg.contains("<path") || svg.contains("<rect"));
    }

    #[test]
    fn every_language_is_served() {
        for code in ["en", "ru", "zh-hant", "pt-br", "ar"] {
            let text = locale(code).unwrap();
            let v: Value = serde_json::from_str(text).unwrap();
            assert!(v["remote.title"].is_string(), "{code}");
        }
        assert!(locale("../secret").is_none());
    }
}
