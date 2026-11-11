//! The app's small JSON files: playlists, EQ presets, streaming sign-ins, the library index.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Reads `path` as JSON. `None` when there is no file yet (a fresh start).
///
/// A file that exists but can't be read as `T` (damaged, or written by a newer Hifi before a
/// downgrade) is moved aside to `<name>.bad-<unix time>` and `None` is returned. Treating it as
/// empty instead would let the next save overwrite the user's playlists or presets with
/// nothing; this way the data is still on disk.
pub fn load_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            log::warn!("can't read {}: {e}", path.display());
            set_aside(path);
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            log::warn!("can't parse {}: {e}", path.display());
            set_aside(path);
            None
        }
    }
}

fn set_aside(path: &Path) {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".bad-{t}"));
    let to: PathBuf = path.with_file_name(name);
    match std::fs::rename(path, &to) {
        Ok(()) => log::warn!("kept the unreadable file as {}", to.display()),
        Err(e) => log::warn!("can't move {} aside: {e}", path.display()),
    }
}

/// Writes `value` as JSON to a temporary file next to `path`, then renames it over `path`, so a
/// crash mid-write never leaves half a file. On Unix the file is readable by its owner only:
/// it is the app's own data, and nothing else on the machine needs to read it. (Streaming
/// sign-ins are written by the streaming crate itself, also owner-only.)
pub fn save_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    let mut tmp = path.file_name().unwrap_or_default().to_os_string();
    tmp.push(".tmp");
    let tmp = path.with_file_name(tmp);
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
    let mut file = open.open(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_is_none_and_damaged_is_kept() {
        let dir = std::env::temp_dir().join(format!("hifi-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("playlists.json");
        assert_eq!(load_json::<Vec<String>>(&path), None);

        save_json(&path, &vec!["a".to_string()]).unwrap();
        assert_eq!(load_json::<Vec<String>>(&path), Some(vec!["a".to_string()]));

        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(load_json::<Vec<String>>(&path), None);
        assert!(!path.exists());
        let kept: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(kept.iter().any(|n| n.starts_with("playlists.json.bad-")), "{kept:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
