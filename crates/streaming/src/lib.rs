//! Qobuz and TIDAL: sign-in, catalogue, and the track itself as a byte stream for the engine.
//!
//! Only for builds that are not distributed through the App Store: neither service offers
//! independent players a sanctioned way to stream full tracks, so this speaks the protocols
//! their own web and Android players speak (as Strawberry, Mopidy and qobine do). The user signs
//! in on the service's own page; we only ever see the token it hands back.
//!
//! Streams the services encrypt are not played: only plain FLAC (Qobuz files, TIDAL FLAC over
//! BTS or DASH) — exactly what a bit-perfect chain needs anyway.
//!
//! Track ids are URIs: `qobuz:track:123`, `tidal:track:123`.

pub mod http;
pub mod qobuz;
pub mod tidal;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use hifi_engine::{Hint, Track, TrackMeta, UserError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Service {
    Qobuz,
    Tidal,
}

impl Service {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "qobuz" => Ok(Self::Qobuz),
            "tidal" => Ok(Self::Tidal),
            _ => bail!("unknown service {s}"),
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Self::Qobuz => "qobuz",
            Self::Tidal => "tidal",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Qobuz => "Qobuz",
            Self::Tidal => "TIDAL",
        }
    }
}

/// Best quality asked of the services.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    /// 16/44.1 FLAC.
    Cd,
    /// Up to 24/96.
    Hires96,
    /// Whatever the master is, up to 24/192 (TIDAL: up to 24/192 too).
    #[default]
    Max,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct STrack {
    pub uri: String,
    pub service: Option<Service>,
    pub id: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_id: Option<String>,
    pub album_artist: Option<String>,
    pub year: Option<String>,
    pub duration: f64,
    pub track_number: Option<u64>,
    pub disc_number: Option<u64>,
    /// Cover art URL.
    pub cover: Option<String>,
    pub isrc: Option<String>,
    /// Best resolution the service has (bits, Hz).
    pub bits: Option<u32>,
    pub rate: Option<u32>,
    pub hi_res: bool,
    /// The account may play it (rights in its country).
    pub streamable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SAlbum {
    pub id: String,
    pub service: Option<Service>,
    pub title: String,
    pub artist: String,
    pub year: Option<String>,
    pub genre: Option<String>,
    pub cover: Option<String>,
    pub track_count: usize,
    pub duration: f64,
    pub bits: Option<u32>,
    pub rate: Option<u32>,
    pub hi_res: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SPlaylist {
    pub id: String,
    pub service: Option<Service>,
    pub name: String,
    pub track_count: usize,
    pub cover: Option<String>,
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub albums: Vec<SAlbum>,
    pub tracks: Vec<STrack>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumPage {
    pub album: SAlbum,
    pub tracks: Vec<STrack>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistPage {
    pub playlist: SPlaylist,
    pub tracks: Vec<STrack>,
}

/// What survives a restart: tokens, the web player's app id and secrets, the quality wish.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    qobuz: Option<qobuz::Account>,
    #[serde(default)]
    qobuz_app: Option<qobuz::WebApp>,
    #[serde(default)]
    tidal: Option<tidal::Account>,
    #[serde(default)]
    quality: Quality,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountDto {
    pub service: Service,
    pub signed_in: bool,
    pub user: Option<String>,
    /// The subscription can't stream lossless (Qobuz/TIDAL plans without it).
    pub note: Option<String>,
}

/// A sign-in under way: open `url` in a browser view; when it navigates to an address that
/// starts with `redirect`, hand that address to [`Streaming::finish_login`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStart {
    pub url: String,
    pub redirect: String,
}

/// Both services, their sign-ins kept in one JSON file (readable by the user only).
pub struct Streaming {
    path: PathBuf,
    saved: Mutex<Saved>,
    /// TIDAL PKCE verifier of the sign-in under way.
    pending_tidal: Mutex<Option<tidal::Pending>>,
    covers: Mutex<HashMap<String, Arc<Vec<u8>>>>,
}

impl Streaming {
    pub fn load(path: PathBuf) -> Self {
        let saved = hifi_engine::store::load_json(&path).unwrap_or_default();
        Self { path, saved: Mutex::new(saved), pending_tidal: Mutex::new(None), covers: Mutex::new(HashMap::new()) }
    }

    fn save(&self) {
        let s = self.saved.lock().unwrap();
        let Ok(bytes) = serde_json::to_vec(&*s) else { return };
        drop(s);
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.path.with_extension("tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
            }
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    pub fn quality(&self) -> Quality {
        self.saved.lock().unwrap().quality
    }

    pub fn set_quality(&self, q: Quality) {
        self.saved.lock().unwrap().quality = q;
        self.save();
    }

    pub fn accounts(&self) -> Vec<AccountDto> {
        let s = self.saved.lock().unwrap();
        vec![
            AccountDto {
                service: Service::Qobuz,
                signed_in: s.qobuz.is_some(),
                user: s.qobuz.as_ref().and_then(|a| a.user.clone()),
                note: s.qobuz.as_ref().and_then(|a| a.note.clone()),
            },
            AccountDto {
                service: Service::Tidal,
                signed_in: s.tidal.is_some(),
                user: s.tidal.as_ref().and_then(|a| a.user.clone()),
                note: None,
            },
        ]
    }

    pub fn signed_in(&self, svc: Service) -> bool {
        let s = self.saved.lock().unwrap();
        match svc {
            Service::Qobuz => s.qobuz.is_some(),
            Service::Tidal => s.tidal.is_some(),
        }
    }

    /// Start a sign-in: the address of the service's own sign-in page.
    pub fn start_login(&self, svc: Service, redirect_port: u16) -> Result<LoginStart> {
        match svc {
            Service::Qobuz => {
                let app = self.qobuz_app()?;
                Ok(qobuz::login_start(&app, redirect_port, &nonce()))
            }
            Service::Tidal => {
                let (start, pending) = tidal::login_start();
                *self.pending_tidal.lock().unwrap() = Some(pending);
                Ok(start)
            }
        }
    }

    /// The sign-in page sent the browser to `redirected`: trade its code for a token.
    pub fn finish_login(&self, svc: Service, redirected: &str) -> Result<()> {
        match svc {
            Service::Qobuz => {
                let app = self.qobuz_app()?;
                let code = http::query_param(redirected, "code_autorisation")
                    .or_else(|| http::query_param(redirected, "code"))
                    .ok_or_else(|| anyhow!("Qobuz returned no sign-in code"))?;
                let account = qobuz::finish_login(&app, &code)?;
                self.saved.lock().unwrap().qobuz = Some(account);
            }
            Service::Tidal => {
                let pending = self.pending_tidal.lock().unwrap().take().ok_or_else(|| anyhow!(UserError::SignInCancelled))?;
                let code = http::query_param(redirected, "code").ok_or_else(|| anyhow!("TIDAL returned no sign-in code"))?;
                let account = tidal::finish_login(&pending, &code)?;
                self.saved.lock().unwrap().tidal = Some(account);
            }
        }
        self.save();
        Ok(())
    }

    pub fn logout(&self, svc: Service) {
        {
            let mut s = self.saved.lock().unwrap();
            match svc {
                Service::Qobuz => s.qobuz = None,
                Service::Tidal => s.tidal = None,
            }
        }
        self.save();
    }

    /// The web player's app id and signing secrets, read again when Qobuz ships a new player.
    fn qobuz_app(&self) -> Result<qobuz::WebApp> {
        let cached = self.saved.lock().unwrap().qobuz_app.clone();
        let app = qobuz::web_app(cached.as_ref())?;
        if cached.as_ref().map(|c| &c.bundle) != Some(&app.bundle) {
            self.saved.lock().unwrap().qobuz_app = Some(app.clone());
            self.save();
        }
        Ok(app)
    }

    fn qobuz(&self) -> Result<qobuz::Client> {
        let account = self.saved.lock().unwrap().qobuz.clone().ok_or_else(|| anyhow!(UserError::SignInNeeded { service: "Qobuz".into() }))?;
        let app = match self.saved.lock().unwrap().qobuz_app.clone() {
            Some(a) if a.app_id == account.app_id => a,
            _ => self.qobuz_app()?,
        };
        Ok(qobuz::Client { app, account })
    }

    /// A TIDAL client with a fresh token (refreshed and saved when it ran out).
    fn tidal(&self) -> Result<tidal::Client> {
        let account = self.saved.lock().unwrap().tidal.clone().ok_or_else(|| anyhow!(UserError::SignInNeeded { service: "TIDAL".into() }))?;
        let (client, refreshed) = tidal::Client::new(account)?;
        if refreshed {
            self.saved.lock().unwrap().tidal = Some(client.account.clone());
            self.save();
        }
        Ok(client)
    }

    /// Run a TIDAL call; a 401 means the token died early — refresh once and retry.
    fn with_tidal<R>(&self, f: impl Fn(&tidal::Client) -> Result<R>) -> Result<R> {
        let c = self.tidal()?;
        match f(&c) {
            Err(e) if http::status_of(&e) == Some(401) => {
                let mut account = c.account.clone();
                account.expires_at = 0;
                let (c, _) = tidal::Client::new(account)?;
                self.saved.lock().unwrap().tidal = Some(c.account.clone());
                self.save();
                f(&c)
            }
            r => r,
        }
    }

    pub fn search(&self, svc: Service, query: &str) -> Result<SearchResult> {
        match svc {
            Service::Qobuz => self.qobuz()?.search(query),
            Service::Tidal => self.with_tidal(|c| c.search(query)),
        }
    }

    pub fn album(&self, svc: Service, id: &str) -> Result<AlbumPage> {
        match svc {
            Service::Qobuz => self.qobuz()?.album(id),
            Service::Tidal => self.with_tidal(|c| c.album(id)),
        }
    }

    pub fn favorite_albums(&self, svc: Service) -> Result<Vec<SAlbum>> {
        match svc {
            Service::Qobuz => self.qobuz()?.favorite_albums(),
            Service::Tidal => self.with_tidal(|c| c.favorite_albums()),
        }
    }

    pub fn playlists(&self, svc: Service) -> Result<Vec<SPlaylist>> {
        match svc {
            Service::Qobuz => self.qobuz()?.playlists(),
            Service::Tidal => self.with_tidal(|c| c.playlists()),
        }
    }

    pub fn playlist(&self, svc: Service, id: &str) -> Result<PlaylistPage> {
        match svc {
            Service::Qobuz => self.qobuz()?.playlist(id),
            Service::Tidal => self.with_tidal(|c| c.playlist(id)),
        }
    }

    pub fn track(&self, uri: &str) -> Result<STrack> {
        let (svc, id) = split_uri(uri)?;
        match svc {
            Service::Qobuz => self.qobuz()?.track(id),
            Service::Tidal => self.with_tidal(|c| c.track(id)),
        }
    }

    /// Look a track up on a service: by ISRC when known, else by artist and title (and length).
    pub fn find(&self, svc: Service, title: &str, artist: Option<&str>, duration: Option<f64>, isrc: Option<&str>) -> Result<Option<STrack>> {
        if let Some(isrc) = isrc.filter(|i| !i.is_empty()) {
            let hits = self.search(svc, isrc)?.tracks;
            if let Some(t) = hits.into_iter().find(|t| t.isrc.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(isrc))) {
                return Ok(Some(t));
            }
        }
        let q = match artist {
            Some(a) => format!("{a} {title}"),
            None => title.to_string(),
        };
        let hits = self.search(svc, &q)?.tracks;
        Ok(best_match(&hits, title, artist, duration).cloned())
    }

    /// Open a track for playback: the byte stream, a format hint, its length, and its tags
    /// (with the cover) from the catalogue.
    pub fn open(&self, uri: &str) -> Result<Arc<Track>> {
        let (svc, id) = split_uri(uri)?;
        let quality = self.quality();
        let (source, hint, info) = match svc {
            Service::Qobuz => {
                let c = self.qobuz()?;
                let info = c.track(id)?;
                let url = c.file_url(id, quality)?;
                let mut hint = Hint::new();
                hint.with_extension("flac");
                c.report_start(id, quality);
                (Box::new(http::Stream::file(&url)?) as Box<dyn hifi_engine::MediaSource>, hint, info)
            }
            Service::Tidal => self.with_tidal(|c| {
                let info = c.track(id)?;
                let (source, hint) = c.stream(id, quality)?;
                Ok((source, hint, info))
            })?,
        };
        if !info.streamable {
            log::info!("{uri} is marked not streamable; trying anyway");
        }
        let cover = info.cover.as_deref().and_then(|u| self.cover(u));
        let meta = TrackMeta {
            title: Some(info.title.clone()),
            artist: info.artist.clone(),
            album: info.album.clone(),
            album_artist: info.album_artist.clone(),
            year: info.year.clone(),
            track_number: info.track_number,
            disc_number: info.disc_number,
            cover: cover.map(|b| ("image/jpeg".to_string(), (*b).clone())),
            genre: None,
        };
        hifi_engine::open_stream(source, hint, Some(info.duration).filter(|d| *d > 0.0), meta)
    }

    fn cover(&self, url: &str) -> Option<Arc<Vec<u8>>> {
        if let Some(b) = self.covers.lock().unwrap().get(url) {
            return Some(b.clone());
        }
        let b = Arc::new(http::bytes(url, 8 << 20).map_err(|e| log::warn!("cover {url}: {e}")).ok()?);
        let mut c = self.covers.lock().unwrap();
        if c.len() > 64 {
            c.clear();
        }
        c.insert(url.to_string(), b.clone());
        Some(b)
    }
}

/// 128 random bits as hex.
fn nonce() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("system randomness");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn split_uri(uri: &str) -> Result<(Service, &str)> {
    let (svc, rest) = uri.split_once(":track:").ok_or_else(|| anyhow!("not a streaming track: {uri}"))?;
    Ok((Service::parse(svc)?, rest))
}

pub fn is_stream_uri(uri: &str) -> bool {
    split_uri(uri).is_ok()
}

fn norm(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).filter(|c| c.is_alphanumeric()).collect()
}

/// The search hit that is the track asked for: same title (ignoring versions in brackets),
/// an artist in common, a length within a few seconds when both are known.
fn best_match<'a>(hits: &'a [STrack], title: &str, artist: Option<&str>, duration: Option<f64>) -> Option<&'a STrack> {
    let bare = |s: &str| {
        let cut = s.find(['(', '[']).map_or(s, |i| &s[..i]);
        let cut = cut.split(" - ").next().unwrap_or(cut);
        norm(cut)
    };
    let want = bare(title);
    let artists: Vec<String> = artist
        .map(|a| a.split([',', '&', ';', '/']).map(norm).filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    hits.iter()
        .filter(|t| bare(&t.title) == want)
        .filter(|t| {
            artists.is_empty() || {
                let theirs = norm(t.artist.as_deref().unwrap_or("")) + &norm(t.album_artist.as_deref().unwrap_or(""));
                artists.iter().any(|a| theirs.contains(a.as_str()))
            }
        })
        .filter(|t| duration.is_none_or(|d| t.duration <= 0.0 || (t.duration - d).abs() <= 6.0))
        .max_by_key(|t| (t.streamable, norm(&t.title) == norm(title), t.hi_res))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_right_hit() {
        let t = |title: &str, artist: &str, d: f64, hi: bool| STrack { title: title.into(), artist: Some(artist.into()), duration: d, hi_res: hi, streamable: true, ..Default::default() };
        let hits = vec![t("So What (Live)", "Someone", 560.0, true), t("So What", "Miles Davis", 562.0, false), t("So What", "Miles Davis", 562.0, true)];
        let m = best_match(&hits, "So What", Some("Miles Davis"), Some(563.0)).unwrap();
        assert!(m.hi_res && m.artist.as_deref() == Some("Miles Davis"));
        assert!(best_match(&hits, "So What", Some("Pink Floyd"), None).is_none());
        assert!(best_match(&hits, "So What", Some("Miles Davis"), Some(300.0)).is_none());
    }
}
