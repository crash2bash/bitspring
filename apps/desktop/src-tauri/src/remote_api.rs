//! The local API, version 1: the phone remote's server (see remote.rs) under addresses and
//! answers that stay as they are documented (docs/api.md, bitspring.app/api), for home
//! automation, control surfaces and scripts.
//!
//! Nothing here widens what the remote already allows: it runs only while the user has the
//! remote switched on, answers only with the token, and plays only what is in the library.
//! Commands go to the window, which carries them out as its own buttons would.

use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use hifi_engine::net::http;
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;
use crate::library::Library;

/// Bumped only for a change that breaks what version 1 promised; additions don't count.
const VERSION: u32 = 1;
/// How often the event stream looks for a change.
const EVENT_TICK: Duration = Duration::from_millis(250);
/// A line the stream sends when nothing happened for this long, so a dead connection shows.
const EVENT_PING: Duration = Duration::from_secs(15);

fn json_response(conn: &mut TcpStream, status: &str, v: &Value) -> std::io::Result<()> {
    http::respond(conn, status, "application/json; charset=utf-8", &[("Cache-Control", "no-store")], v.to_string().as_bytes())
}

fn refuse(conn: &mut TcpStream, status: &str, error: &str) -> std::io::Result<()> {
    json_response(conn, status, &json!({ "error": error }))
}

/// A request under `/api/v1/` (`path` is what follows), its token already checked. `stop`
/// is set when the server closes (the remote switched off, or its address changed).
pub fn handle(app: &AppHandle, conn: &mut TcpStream, req: &http::Request, path: &str, stop: &AtomicBool) -> anyhow::Result<()> {
    let body = || serde_json::from_slice::<Value>(&req.body).unwrap_or(Value::Null);
    match (req.method.as_str(), path) {
        ("GET", "info") => json_response(conn, "200 OK", &json!({ "app": "BitSpring", "version": app.package_info().version.to_string(), "api": VERSION }))?,
        ("GET", "state") => json_response(conn, "200 OK", &state(app))?,
        ("GET", "queue") => json_response(conn, "200 OK", &queue(app))?,
        ("GET", "library") => {
            let albums = crate::library::library_albums(app.clone(), app.state::<Library>());
            let tracks = crate::library::library_tracks(app.state::<Library>());
            json_response(conn, "200 OK", &json!({ "albums": albums, "tracks": tracks }))?;
        }
        ("GET", "cover/current") => match current_cover(app) {
            Some((mime, bytes)) => http::respond(conn, "200 OK", mime, &[("Cache-Control", "no-store"), NOSNIFF], &bytes)?,
            None => refuse(conn, "404 Not Found", "no cover")?,
        },
        ("GET", p) if p.starts_with("cover/") => {
            let r = app.state::<Library>().cover_response(app, p.trim_start_matches("cover"));
            let ct = r.headers().get("Content-Type").and_then(|v| v.to_str().ok()).unwrap_or("image/jpeg").to_string();
            if r.status().is_success() {
                let ct = image_type(r.body()).unwrap_or(if ct == "image/png" { "image/png" } else { "image/jpeg" });
                http::respond(conn, "200 OK", ct, &[("Cache-Control", "max-age=86400"), NOSNIFF], r.body())?;
            } else {
                refuse(conn, "404 Not Found", "no cover")?;
            }
        }
        ("GET", "volume") => json_response(conn, "200 OK", &json!({ "volume": output_uid(app).and_then(crate::output_volume) }))?,
        ("POST", "volume") => match body()["volume"].as_f64().filter(|v| v.is_finite()) {
            None => refuse(conn, "400 Bad Request", "volume: a number from 0 to 100")?,
            Some(v) => match output_uid(app) {
                Some(uid) => {
                    crate::set_output_volume(uid, v.clamp(0.0, 100.0).round() as u32);
                    json_response(conn, "200 OK", &json!({ "ok": true }))?;
                }
                None => refuse(conn, "409 Conflict", "nothing is playing")?,
            },
        },
        ("GET", "events") => events(app, conn, stop)?,
        ("POST", name) => match command(name, &body()).and_then(|c| crate::remote::checked(app, &c)) {
            Some(c) => {
                let _ = app.emit("remote", c);
                json_response(conn, "200 OK", &json!({ "ok": true }))?;
            }
            None if COMMANDS.contains(&name) => refuse(conn, "400 Bad Request", "the command's values are missing or out of range")?,
            None if READS.contains(&name) || name.starts_with("cover/") => refuse(conn, "405 Method Not Allowed", "this address is read with GET")?,
            None => refuse(conn, "404 Not Found", "no such address")?,
        },
        (_, name) if COMMANDS.contains(&name) || READS.contains(&name) => refuse(conn, "405 Method Not Allowed", "wrong method")?,
        _ => refuse(conn, "404 Not Found", "no such address")?,
    }
    Ok(())
}

/// What is read (with GET).
const READS: [&str; 6] = ["info", "state", "queue", "library", "volume", "events"];
const COMMANDS: [&str; 12] = ["play", "pause", "toggle", "next", "previous", "stop", "seek", "shuffle", "repeat", "queue/jump", "queue/play", "queue/add"];

/// A command of the API as the one the window understands (see `remote::checked`, which
/// still has to pass it).
fn command(name: &str, body: &Value) -> Option<Value> {
    Some(match name {
        "play" => json!({ "kind": "resume" }),
        "pause" | "toggle" | "next" | "stop" => json!({ "kind": name }),
        "previous" => json!({ "kind": "prev" }),
        "seek" => json!({ "kind": "seek", "secs": body["position"] }),
        "shuffle" => json!({ "kind": "shuffle", "on": body["on"] }),
        "repeat" => json!({ "kind": "repeat", "mode": body["mode"] }),
        "queue/jump" => json!({ "kind": "jump", "index": body["index"] }),
        "queue/play" => json!({ "kind": "play", "paths": body["paths"], "index": body["index"] }),
        "queue/add" => json!({ "kind": "enqueue", "paths": body["paths"] }),
        _ => return None,
    })
}

fn output_uid(app: &AppHandle) -> Option<String> {
    app.state::<AppState>().current.lock().unwrap().as_ref().map(|c| c.device_uid.clone())
}

/// A browser must take a cover for the image we say it is, whatever its bytes look like.
const NOSNIFF: (&str, &str) = ("X-Content-Type-Options", "nosniff");

/// What kind of image `bytes` are, by how they begin. A cover's type as its file's tags
/// name it is never passed on: the tags are whatever the file's maker wrote, and a type
/// like text/html would make a browser run the "cover" as a page of this server.
fn image_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        [b'B', b'M', ..] => Some("image/bmp"),
        _ => None,
    }
}

/// The playing track's own cover (which a file outside the library has too). None when it
/// has none — or what its tags call a cover isn't an image.
fn current_cover(app: &AppHandle) -> Option<(&'static str, Vec<u8>)> {
    let now = crate::now_playing(app.state::<AppState>())?;
    let cover = now.cover?;
    let (_, data) = cover.strip_prefix("data:")?.split_once(";base64,")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    Some((image_type(&bytes)?, bytes))
}

/// What plays, and how: the answer of `GET state` and of every event.
pub(crate) fn state(app: &AppHandle) -> Value {
    let st = app.state::<AppState>();
    let status = crate::status(app.state::<AppState>(), app.state::<std::sync::Arc<crate::stats::Plays>>());
    let now = crate::now_playing(app.state::<AppState>());
    let (length, index, shuffle, repeat) = {
        let q = st.queue.lock().unwrap();
        (q.paths.len(), q.index, q.shuffle, q.repeat)
    };
    let word = if !status.active || status.finished {
        "stopped"
    } else if status.paused {
        "paused"
    } else {
        "playing"
    };
    let track = now.as_ref().map(|n| {
        let album_id = app.state::<Library>().tracks_by_path(std::slice::from_ref(&n.path)).remove(&n.path).map(|t| t.album_id);
        json!({
            "path": n.path,
            "title": n.title,
            "artist": n.artist,
            "album": n.album,
            "year": n.year,
            "duration": n.duration,
            "albumId": album_id,
            // (Changes with the track, so a client that caches by address shows the new one.)
            "cover": n.cover.as_ref().map(|_| format!("/api/v1/cover/current?rev={}", n.rev)),
        })
    });
    let signal = now.as_ref().map(|n| {
        let s = &n.signal;
        json!({
            "output": n.device_name,
            "sourceRate": s.dsd_rate.unwrap_or(s.source_rate),
            "sourceBits": s.source_bits,
            "outputRate": s.device_rate,
            "outputBits": s.dac_bits,
            "bitPerfect": s.bit_perfect,
            "lossy": s.source_lossy,
        })
    });
    json!({
        "state": word,
        "position": if status.active { status.position } else { 0.0 },
        "duration": now.as_ref().map_or(status.duration, |n| n.duration),
        "buffering": status.buffering,
        "track": track,
        "signal": signal,
        "queue": { "index": index, "length": length },
        "shuffle": shuffle,
        "repeat": repeat,
    })
}

fn queue(app: &AppHandle) -> Value {
    let st = app.state::<AppState>();
    let (paths, index) = {
        let q = st.queue.lock().unwrap();
        (q.paths.clone(), q.index)
    };
    let known = app.state::<Library>().tracks_by_path(&paths);
    let tracks: Vec<Value> = paths
        .iter()
        .map(|p| match known.get(p) {
            Some(t) => json!({ "path": p, "title": t.title, "artist": t.artist, "duration": t.duration, "albumId": t.album_id }),
            None => json!({ "path": p, "title": crate::remote::file_title(p), "artist": null, "duration": null, "albumId": null }),
        })
        .collect();
    json!({ "index": index, "tracks": tracks })
}

/// `GET events`: the state, sent again whenever it changes (Server-Sent Events, each one an
/// event named `state`). The position runs on by itself while playing, so it is news only
/// when it jumps: a seek.
fn events(app: &AppHandle, conn: &mut TcpStream, stop: &AtomicBool) -> std::io::Result<()> {
    conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\nX-Accel-Buffering: no\r\n\r\n")?;
    conn.flush()?;
    let mut last: Option<(Value, f64, Instant)> = None;
    let mut quiet = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let now = state(app);
        if changed(last.as_ref(), &now) {
            conn.write_all(format!("event: state\ndata: {now}\n\n").as_bytes())?;
            conn.flush()?;
            quiet = Instant::now();
        } else if quiet.elapsed() >= EVENT_PING {
            conn.write_all(b": ping\n\n")?;
            conn.flush()?;
            quiet = Instant::now();
        }
        let position = now["position"].as_f64().unwrap_or(0.0);
        last = Some((now, position, Instant::now()));
        std::thread::sleep(EVENT_TICK);
    }
    Ok(())
}

/// Whether `now` says something `last` (the state a tick ago, its position, and when) didn't.
fn changed(last: Option<&(Value, f64, Instant)>, now: &Value) -> bool {
    let Some((before, position, at)) = last else { return true };
    let without_position = |v: &Value| {
        let mut v = v.clone();
        if let Some(o) = v.as_object_mut() {
            o.remove("position");
        }
        v
    };
    if without_position(before) != without_position(now) {
        return true;
    }
    // Where the position would be by now if nothing but time moved it.
    let expected = if now["state"] == "playing" { position + at.elapsed().as_secs_f64() } else { *position };
    (now["position"].as_f64().unwrap_or(0.0) - expected).abs() > 1.5
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_become_what_the_window_understands() {
        assert_eq!(command("play", &Value::Null), Some(json!({ "kind": "resume" })));
        assert_eq!(command("previous", &Value::Null), Some(json!({ "kind": "prev" })));
        assert_eq!(command("seek", &json!({ "position": 61.5 })), Some(json!({ "kind": "seek", "secs": 61.5 })));
        assert_eq!(command("repeat", &json!({ "mode": "one" })), Some(json!({ "kind": "repeat", "mode": "one" })));
        assert_eq!(command("queue/add", &json!({ "paths": ["/a.flac"] })), Some(json!({ "kind": "enqueue", "paths": ["/a.flac"] })));
        assert_eq!(command("format-disk", &Value::Null), None);
        // Every documented command is one `command` knows.
        for name in COMMANDS {
            assert!(command(name, &Value::Null).is_some(), "{name}");
        }
    }

    /// A cover goes out as the image its bytes are, never as the type a file's tags claim.
    #[test]
    fn a_cover_is_served_by_what_its_bytes_are() {
        assert_eq!(image_type(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 16]), Some("image/jpeg"));
        assert_eq!(image_type(b"\x89PNG\r\n\x1a\n"), Some("image/png"));
        assert_eq!(image_type(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(image_type(b"<html><script>alert(1)</script>"), None);
        assert_eq!(image_type(b"<svg xmlns='http://www.w3.org/2000/svg'>"), None);
        assert_eq!(image_type(b""), None);
    }

    #[test]
    fn an_event_is_sent_for_a_change_and_for_a_jump_only() {
        let playing = |position: f64, title: &str| json!({ "state": "playing", "position": position, "track": { "title": title } });
        let a = playing(10.0, "One");
        assert!(changed(None, &a));
        let just_now = (a.clone(), 10.0, Instant::now());
        // Time passing is no news; another track, a pause or a seek is.
        assert!(!changed(Some(&just_now), &playing(10.2, "One")));
        assert!(changed(Some(&just_now), &playing(10.2, "Two")));
        assert!(changed(Some(&just_now), &playing(95.0, "One")));
        assert!(changed(Some(&just_now), &json!({ "state": "paused", "position": 10.2, "track": { "title": "One" } })));
        // A quarter of a second later the position is expected to have moved on.
        let earlier = (a, 10.0, Instant::now() - Duration::from_millis(250));
        assert!(!changed(Some(&earlier), &playing(10.25, "One")));
    }
}
