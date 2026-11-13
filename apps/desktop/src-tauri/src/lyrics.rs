//! The playing track's lyrics: the ones kept with the file first (see hifi_engine::lyrics),
//! then — only when the listener switched it on — LRCLIB, a free, open lyrics database that
//! needs no account. It gets the track's title, artist, album and length, nothing else.
//!
//! LRCLIB's answers are kept in the app's folder ("none" for a while too, so a song without
//! lyrics doesn't ask again on every play).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use hifi_engine::lyrics::{self, Lyrics, LrclibRecord};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::CmdResult;

const LRCLIB: &str = "https://lrclib.net/api";
/// Asking again for a song LRCLIB had nothing for: someone may have added it since.
const RETRY_NONE: Duration = Duration::from_secs(14 * 86_400);

#[derive(Serialize, Deserialize)]
struct Cached {
    lyrics: Option<Lyrics>,
    /// Unix seconds.
    at: u64,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// FNV-1a: a short, stable file name for a song.
fn key(artist: &str, title: &str, album: Option<&str>, duration: Option<f64>) -> String {
    let s = format!("{}\u{1f}{}\u{1f}{}\u{1f}{}", artist.to_lowercase(), title.to_lowercase(), album.unwrap_or("").to_lowercase(), duration.map(|d| d.round() as i64).unwrap_or(0));
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(20)))
        // LRCLIB asks apps to name themselves.
        .user_agent(concat!("BitSpring/", env!("CARGO_PKG_VERSION"), " (https://github.com/crash2bash/bitspring)"))
        .http_status_as_error(false)
        .build()
        .into()
}

fn fetch(artist: &str, title: &str, album: Option<&str>, duration: Option<f64>) -> Result<Option<Lyrics>> {
    let agent = agent();
    // The exact recording first (LRCLIB matches the length within a couple of seconds)...
    let mut get = agent.get(format!("{LRCLIB}/get")).query("artist_name", artist).query("track_name", title);
    if let Some(a) = album {
        get = get.query("album_name", a);
    }
    if let Some(d) = duration {
        get = get.query("duration", (d.round() as i64).to_string());
    }
    let mut resp = get.call().context("couldn't reach LRCLIB")?;
    if resp.status() == 200 {
        let text = resp.body_mut().with_config().limit(1 << 20).read_to_string().context("LRCLIB's answer")?;
        let record: LrclibRecord = serde_json::from_str(&text).context("LRCLIB's answer")?;
        if record.instrumental {
            return Ok(None);
        }
        if let Some(l) = record.lyrics() {
            return Ok(Some(l));
        }
    } else if resp.status() != 404 {
        anyhow::bail!("LRCLIB answered {}", resp.status());
    }
    // ...then a search: the album may be named differently there, or not at all.
    let mut resp = agent
        .get(format!("{LRCLIB}/search"))
        .query("artist_name", artist)
        .query("track_name", title)
        .call()
        .context("couldn't reach LRCLIB")?;
    if resp.status() != 200 {
        anyhow::bail!("LRCLIB answered {}", resp.status());
    }
    let text = resp.body_mut().with_config().limit(4 << 20).read_to_string().context("LRCLIB's answer")?;
    let records: Vec<LrclibRecord> = serde_json::from_str(&text).context("LRCLIB's answer")?;
    Ok(lyrics::best_match(&records, duration))
}

fn online(dir: Option<&Path>, artist: &str, title: &str, album: Option<&str>, duration: Option<f64>) -> Result<Option<Lyrics>> {
    let file: Option<PathBuf> = dir.map(|d| d.join("lyrics").join(format!("{}.json", key(artist, title, album, duration))));
    if let Some(c) = file.as_deref().and_then(hifi_engine::store::load_json::<Cached>)
        && (c.lyrics.is_some() || now_secs().saturating_sub(c.at) < RETRY_NONE.as_secs())
    {
        return Ok(c.lyrics);
    }
    let found = fetch(artist, title, album, duration)?;
    if let Some(f) = &file {
        // Best effort: without the copy we only ask again next time.
        let _ = std::fs::create_dir_all(f.parent().unwrap_or(f));
        let _ = hifi_engine::store::save_json(f, &Cached { lyrics: found.clone(), at: now_secs() });
    }
    Ok(found)
}

/// Lyrics for the track at `path`: its own, else (with `online`) LRCLIB's for its title and
/// artist. `None` when there are none (or the song has no words).
#[tauri::command]
pub async fn lyrics(
    app: AppHandle,
    path: String,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    duration: Option<f64>,
    online: bool,
) -> CmdResult<Option<Lyrics>> {
    let dir = app.path().app_data_dir().ok();
    tauri::async_runtime::spawn_blocking(move || -> Result<Option<Lyrics>> {
        // A streamed track's "path" is a URI (qobuz:track:1): nothing kept beside it.
        let local = Path::new(&path);
        if local.is_absolute()
            && let Some(l) = lyrics::read_local(local)
        {
            return Ok(Some(l));
        }
        let artist = artist.as_deref().map(str::trim).filter(|a| !a.is_empty());
        let title = title.trim();
        match artist {
            Some(artist) if online && !title.is_empty() => {
                let duration = duration.filter(|d| d.is_finite() && *d > 0.0);
                self::online(dir.as_deref(), artist, title, album.as_deref().map(str::trim).filter(|a| !a.is_empty()), duration)
            }
            _ => Ok(None),
        }
    })
    .await
    .map_err(crate::err)?
    .map_err(crate::err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn song_keys_are_stable_and_ignore_case() {
        assert_eq!(key("A", "B", Some("C"), Some(200.4)), key("a", "b", Some("c"), Some(200.0)));
        assert_ne!(key("A", "B", None, Some(200.0)), key("A", "B", None, Some(260.0)));
    }

    /// Goes online: `cargo test -- --ignored lrclib`.
    #[test]
    #[ignore]
    fn lrclib_finds_a_well_known_song() {
        let l = fetch("Coldplay", "Yellow", Some("Parachutes"), Some(269.0)).unwrap().unwrap();
        assert!(l.synced && l.lines.len() > 10);
        // Off by an album name and a few seconds: the search still finds it.
        let l = fetch("Coldplay", "Yellow", Some("No Such Album"), Some(267.0)).unwrap().unwrap();
        assert!(l.synced);
        // Instrumental: nothing to show.
        assert!(fetch("Miles Davis", "So What", None, Some(564.0)).unwrap().is_none());
    }
}
