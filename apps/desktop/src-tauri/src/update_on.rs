//! Updates for the builds from the website (Mac DMG, Windows installer, Android APK).
//!
//! Every release on GitHub carries `latest.json`: the version, its notes, and per platform a
//! download and, for Mac and Windows, a signature made with our updater key. Mac and Windows go
//! through Tauri's updater, which refuses anything the key didn't sign. Android downloads the
//! APK and hands it to the system installer, which only accepts an update signed with the same
//! key as the installed app.
//!
//! Nothing installs on its own: the app checks, says what is new, and installs when asked —
//! never in the middle of a record.
//!
//! App Store builds (Mac App Store, iPhone) have none of this — see update_off.rs.

use std::sync::atomic::{AtomicU64, Ordering};

use hifi_engine::UserError;
use serde::Serialize;
use tauri::AppHandle;

/// Bytes downloaded / expected, for a progress bar while installing.
static DONE: AtomicU64 = AtomicU64::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    version: String,
    notes: Option<String>,
    date: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheck {
    /// This build updates itself (false in App Store builds).
    enabled: bool,
    current: String,
    update: Option<UpdateInfo>,
}

fn failed(e: impl std::fmt::Display) -> UserError {
    UserError::UpdateFailed { detail: e.to_string() }
}

#[tauri::command]
pub async fn update_check(app: AppHandle) -> Result<UpdateCheck, UserError> {
    let current = app.package_info().version.to_string();
    let update = platform::check(&app, &current).await?;
    Ok(UpdateCheck { enabled: true, current, update })
}

/// Download and install the newer version, then restart into it (Android: open the system
/// installer, which takes over).
#[tauri::command]
pub async fn update_install(app: AppHandle) -> Result<(), UserError> {
    DONE.store(0, Ordering::Relaxed);
    TOTAL.store(0, Ordering::Relaxed);
    platform::install(&app).await
}

/// [downloaded, total] bytes of the running install (total 0 = not known yet).
#[tauri::command]
pub fn update_progress() -> [u64; 2] {
    [DONE.load(Ordering::Relaxed), TOTAL.load(Ordering::Relaxed)]
}

#[cfg(not(target_os = "android"))]
mod platform {
    use super::*;
    use tauri_plugin_updater::UpdaterExt;

    pub async fn check(app: &AppHandle, _current: &str) -> Result<Option<UpdateInfo>, UserError> {
        let update = app.updater().map_err(failed)?.check().await.map_err(failed)?;
        Ok(update.map(|u| UpdateInfo { version: u.version.clone(), notes: u.body.clone(), date: u.date.map(|d| d.to_string()) }))
    }

    pub async fn install(app: &AppHandle) -> Result<(), UserError> {
        let Some(update) = app.updater().map_err(failed)?.check().await.map_err(failed)? else {
            return Ok(());
        };
        update
            .download_and_install(
                |chunk, total| {
                    DONE.fetch_add(chunk as u64, Ordering::Relaxed);
                    if let Some(t) = total {
                        TOTAL.store(t, Ordering::Relaxed);
                    }
                },
                || {},
            )
            .await
            .map_err(failed)?;
        // Let go of the DAC (hog mode, sample rate) before the new version starts.
        {
            use tauri::Manager;
            drop(app.state::<crate::AppState>().player.lock().unwrap().take());
        }
        app.restart()
    }
}

#[cfg(target_os = "android")]
mod platform {
    use super::*;
    use std::io::{Read, Write};
    use std::time::Duration;

    /// The same manifest Tauri's updater reads on the desktop.
    const MANIFEST: &str = "https://github.com/crash2bash/hi-fi/releases/latest/download/latest.json";
    const PLATFORM: &str = "android-aarch64";
    /// More than any APK of ours will ever be; stops a runaway download.
    const MAX_APK: u64 = 300 * 1024 * 1024;

    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(30)))
            // A stalled download ends with an error instead of hanging forever (an APK takes
            // well under this even on a slow connection).
            .timeout_recv_body(Some(Duration::from_secs(15 * 60)))
            .user_agent(concat!("Hifi/", env!("CARGO_PKG_VERSION")))
            .build()
            .into()
    }

    /// "0.10.2" → ([0, 10, 2], true). The flag is "a release": "0.3.0-beta.1" sorts before
    /// "0.3.0", as in semver. Build metadata ("+…") is ignored.
    fn parse(v: &str) -> (Vec<u64>, bool) {
        let v = v.trim_start_matches('v').split('+').next().unwrap_or("");
        let (core, pre) = match v.split_once('-') {
            Some((c, _)) => (c, true),
            None => (v, false),
        };
        (core.split('.').map(|p| p.parse().unwrap_or(0)).collect(), !pre)
    }

    struct Latest {
        version: String,
        notes: Option<String>,
        date: Option<String>,
        url: String,
    }

    fn latest() -> Result<Option<Latest>, UserError> {
        let mut resp = agent().get(MANIFEST).call().map_err(failed)?;
        let bytes = resp.body_mut().with_config().limit(1 << 20).read_to_vec().map_err(failed)?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(failed)?;
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
        let Some(version) = s("version") else { return Ok(None) };
        let Some(url) = v.pointer(&format!("/platforms/{PLATFORM}/url")).and_then(|x| x.as_str()) else { return Ok(None) };
        Ok(Some(Latest { version, notes: s("notes"), date: s("pub_date"), url: url.to_string() }))
    }

    fn newer(l: &Latest, current: &str) -> bool {
        parse(&l.version) > parse(current)
    }

    pub async fn check(_app: &AppHandle, current: &str) -> Result<Option<UpdateInfo>, UserError> {
        let current = current.to_string();
        tauri::async_runtime::spawn_blocking(move || {
            Ok(latest()?.filter(|l| newer(l, &current)).map(|l| UpdateInfo { version: l.version, notes: l.notes, date: l.date }))
        })
        .await
        .map_err(failed)?
    }

    pub async fn install(app: &AppHandle) -> Result<(), UserError> {
        let current = app.package_info().version.to_string();
        tauri::async_runtime::spawn_blocking(move || {
            let Some(l) = latest()?.filter(|l| newer(l, &current)) else { return Ok(()) };
            // Only from our own release downloads.
            if !l.url.starts_with("https://github.com/crash2bash/hi-fi/releases/") {
                return Err(failed("unexpected download address"));
            }
            // Before downloading: the first time, Android must be told to let Hifi install apps.
            if !crate::android_media::install_allowed() {
                return Err(UserError::InstallNotAllowed);
            }
            let dir = crate::android_media::cache_dir().ok_or_else(|| failed("no cache folder"))?.join("updates");
            let path = dir.join(format!("Hifi-{}.apk", l.version));
            // Downloaded already (the installer was cancelled last time): hand it over again.
            // Only a finished download is ever named .apk; it arrives as .part.
            if path.is_file() {
                return if crate::android_media::install_apk(&path.to_string_lossy()) { Ok(()) } else { Err(UserError::InstallNotAllowed) };
            }
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).map_err(failed)?;
            let part = path.with_extension("apk.part");
            let resp = agent().get(&l.url).call().map_err(failed)?;
            if let Some(n) = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()) {
                TOTAL.store(n, Ordering::Relaxed);
            }
            let mut body = resp.into_body().into_with_config().limit(MAX_APK).reader();
            let mut file = std::fs::File::create(&part).map_err(failed)?;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = body.read(&mut buf).map_err(failed)?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n]).map_err(failed)?;
                DONE.fetch_add(n as u64, Ordering::Relaxed);
            }
            file.sync_all().map_err(failed)?;
            drop(file);
            // A body shorter than its Content-Length is already a read error above (ureq checks).
            std::fs::rename(&part, &path).map_err(failed)?;
            if crate::android_media::install_apk(&path.to_string_lossy()) { Ok(()) } else { Err(UserError::InstallNotAllowed) }
        })
        .await
        .map_err(failed)?
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn versions_compare_as_numbers() {
            assert!(super::parse("0.10.0") > super::parse("0.9.3"));
            assert!(super::parse("v1.0.0") > super::parse("0.99.99"));
            assert!(super::parse("0.2.0") == super::parse("0.2.0"));
            assert!(super::parse("0.3.0") > super::parse("0.3.0-beta.1"));
            assert!(super::parse("0.3.0-beta.1") > super::parse("0.2.9"));
            assert!(super::parse("0.3.0+build5") == super::parse("0.3.0"));
        }
    }
}
