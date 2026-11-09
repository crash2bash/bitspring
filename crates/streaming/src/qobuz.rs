//! Qobuz, as its web player speaks it.
//!
//! - The web player's app id and request-signing secrets are read from its own script
//!   (play.qobuz.com/login → bundle.js), again whenever Qobuz ships a new player.
//! - Sign-in is Qobuz's own OAuth page; it redirects to a local address with a code, which
//!   `oauth/callback` trades for the user's token.
//! - A track plays from `track/getFileUrl`: a plain FLAC file at up to 24/192. (The newer
//!   `file/url` delivers encrypted segments; we don't decrypt anything.)
//!
//! Field names follow qobine and Strawberry, both open source.

use anyhow::{Context, Result, anyhow, bail};
use md5::{Digest, Md5};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::http::{self, api, json};
use hifi_engine::UserError;
use crate::{AlbumPage, LoginStart, PlaylistPage, Quality, SAlbum, SPlaylist, STrack, SearchResult, Service};

const API: &str = "https://www.qobuz.com/api.json/0.2/";
const PLAY: &str = "https://play.qobuz.com";
/// Constant of the web player's OAuth exchange (as in qobine).
const OAUTH_PRIVATE_KEY: &str = "6lz8C03UDIC7";
/// A track every region can fetch, to find which secret signs for this account.
const PROBE_TRACK: &str = "64868955";

/// The web player's identity, read from its script.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebApp {
    pub bundle: String,
    pub app_id: String,
    pub secrets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub app_id: String,
    pub token: String,
    pub user_id: i64,
    pub user: Option<String>,
    /// The secret that signs file requests for this account's region.
    pub secret: Option<String>,
    /// E.g. the plan streams MP3 only.
    pub note: Option<String>,
}

fn get(path: &str, app_id: &str, token: Option<&str>, params: &[(&str, &str)]) -> Result<Value> {
    let mut req = api().get(format!("{API}{path}")).header("X-App-Id", app_id);
    if let Some(t) = token {
        req = req.header("X-User-Auth-Token", t);
    }
    for (k, v) in params {
        req = req.query(*k, *v);
    }
    json(req.call()).with_context(|| format!("Qobuz {path}"))
}

/// The bundle the login page loads; it changes with every web player release.
fn bundle_path() -> Result<String> {
    let html = api().get(format!("{PLAY}/login")).call()?.body_mut().read_to_string()?;
    let re = Regex::new(r#"<script src="(/resources/\d+\.\d+\.\d+-[a-z0-9]\d{3}/bundle\.js)"></script>"#)?;
    Ok(re.captures(&html).and_then(|c| c.get(1)).ok_or_else(|| anyhow!(UserError::StreamChanged { service: "Qobuz".into() }))?.as_str().to_string())
}

/// App id and secrets from the bundle (decoded the way qobuz-dl and qobine do: per timezone,
/// seed + info + extras, minus the last 44 characters, base64).
fn read_bundle(script: &str) -> Result<(String, Vec<String>)> {
    use base64::Engine as _;
    let app_id = Regex::new(r#"production:\{api:\{appId:"(\d{9})",appSecret:"\w{32}""#)?
        .captures(script)
        .and_then(|c| c.get(1))
        .ok_or_else(|| anyhow!(UserError::StreamChanged { service: "Qobuz".into() }))?
        .as_str()
        .to_string();
    let seeds = Regex::new(r#"[a-z]\.initialSeed\("([\w=]+)",window\.utimezone\.([a-z]+)\)"#)?;
    let mut secrets = Vec::new();
    for c in seeds.captures_iter(script) {
        let seed = &c[1];
        let mut tz = c[2].to_string();
        if let Some(f) = tz.get_mut(0..1) {
            f.make_ascii_uppercase();
        }
        let info = Regex::new(&format!(r#"name:"\w+/{tz}[a-z]?",info:"([\w=]+)",extras:"([\w=]+)""#))?;
        for i in info.captures_iter(script) {
            let chars = format!("{seed}{}{}", &i[1], &i[2]);
            if chars.len() <= 44 {
                continue;
            }
            let Ok(bytes) = base64::engine::general_purpose::URL_SAFE.decode(&chars[..chars.len() - 44]) else { continue };
            if let Ok(s) = String::from_utf8(bytes) {
                if !secrets.contains(&s) {
                    secrets.push(s);
                }
            }
        }
    }
    if secrets.is_empty() {
        bail!(UserError::StreamChanged { service: "Qobuz".into() });
    }
    Ok((app_id, secrets))
}

/// The web player identity: cached while Qobuz still serves the same player.
pub fn web_app(cached: Option<&WebApp>) -> Result<WebApp> {
    let bundle = match bundle_path() {
        Ok(b) => b,
        Err(e) => return cached.cloned().ok_or(e),
    };
    if let Some(c) = cached.filter(|c| c.bundle == bundle) {
        return Ok(c.clone());
    }
    let script = api().get(format!("{PLAY}{bundle}")).call()?.body_mut().with_config().limit(32 << 20).read_to_string()?;
    let (app_id, secrets) = read_bundle(&script)?;
    Ok(WebApp { bundle, app_id, secrets })
}

pub fn login_start(app: &WebApp, port: u16) -> LoginStart {
    let redirect = format!("http://localhost:{port}");
    LoginStart {
        url: format!("https://www.qobuz.com/signin/oauth?ext_app_id={}&redirect_url={}", app.app_id, http::enc(&redirect)),
        redirect,
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn sign_file_url(track: &str, format: u32, ts: &str, secret: &str) -> String {
    let s = format!("trackgetFileUrlformat_id{format}intentstreamtrack_id{track}{ts}{secret}");
    hex(&Md5::digest(s.as_bytes()))
}

fn file_url_call(app_id: &str, token: &str, secret: &str, track: &str, format: u32) -> Result<Value> {
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs().to_string();
    let sig = sign_file_url(track, format, &ts, secret);
    let f = format.to_string();
    get(
        "track/getFileUrl",
        app_id,
        Some(token),
        &[("request_ts", &ts), ("request_sig", &sig), ("track_id", track), ("format_id", &f), ("intent", "stream")],
    )
}

pub fn finish_login(app: &WebApp, code: &str) -> Result<Account> {
    let v = get("oauth/callback", &app.app_id, None, &[("code", code), ("private_key", OAUTH_PRIVATE_KEY)])?;
    let token = v
        .get("token")
        .or_else(|| v.get("user_auth_token"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Qobuz returned no token"))?
        .to_string();
    let user_id = v
        .get("user_id")
        .and_then(|x| x.as_i64().or_else(|| x.as_str()?.parse().ok()))
        .or_else(|| v.pointer("/user/id").and_then(Value::as_i64))
        .unwrap_or(0);
    // Who signed in, and what the plan allows.
    let me = get("user/get", &app.app_id, Some(&token), &[]).ok();
    let user = me
        .as_ref()
        .and_then(|m| m.get("display_name").or_else(|| m.get("login")).or_else(|| m.get("email")))
        .and_then(Value::as_str)
        .map(str::to_string);
    let lossless = me.as_ref().and_then(|m| m.pointer("/credential/parameters/lossless_streaming")).and_then(Value::as_bool);
    let note = (lossless == Some(false)).then(|| "noLossless".to_string());
    // Which of the per-region secrets signs for us.
    let secret = app.secrets.iter().find(|s| file_url_call(&app.app_id, &token, s, PROBE_TRACK, 5).is_ok()).cloned();
    if secret.is_none() {
        log::warn!("no Qobuz secret signs for this account");
    }
    Ok(Account { app_id: app.app_id.clone(), token, user_id, user, secret, note })
}

pub struct Client {
    pub app: WebApp,
    pub account: Account,
}

fn s(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr).and_then(|x| match x {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

fn rate_hz(v: &Value, ptr: &str) -> Option<u32> {
    v.pointer(ptr).and_then(Value::as_f64).map(|k| (k * 1000.0).round() as u32).filter(|r| *r > 0)
}

/// "Title (Version)", the way Qobuz shows it.
fn titled(v: &Value) -> String {
    let t = s(v, "/title").unwrap_or_default();
    match s(v, "/version") {
        Some(ver) if !t.contains(&ver) => format!("{t} ({ver})"),
        _ => t,
    }
}

fn album_of(v: &Value) -> SAlbum {
    let bits = v.get("maximum_bit_depth").and_then(Value::as_u64).map(|b| b as u32);
    let rate = rate_hz(v, "/maximum_sampling_rate");
    SAlbum {
        id: s(v, "/id").unwrap_or_default(),
        service: Some(Service::Qobuz),
        title: titled(v),
        artist: s(v, "/artist/name").unwrap_or_default(),
        year: s(v, "/release_date_original").map(|d| d.chars().take(4).collect()),
        genre: s(v, "/genre/name"),
        cover: s(v, "/image/large").or_else(|| s(v, "/image/small")),
        track_count: v.get("tracks_count").and_then(Value::as_u64).unwrap_or(0) as usize,
        duration: v.get("duration").and_then(Value::as_f64).unwrap_or(0.0),
        hi_res: v.get("hires_streamable").and_then(Value::as_bool).unwrap_or(false) || bits.is_some_and(|b| b > 16),
        bits,
        rate,
    }
}

fn track_of(v: &Value, album: Option<&SAlbum>) -> STrack {
    let own_album = v.get("album").map(album_of);
    let a = own_album.as_ref().or(album);
    let id = s(v, "/id").unwrap_or_default();
    let bits = v.get("maximum_bit_depth").and_then(Value::as_u64).map(|b| b as u32).or(a.and_then(|a| a.bits));
    STrack {
        uri: format!("qobuz:track:{id}"),
        service: Some(Service::Qobuz),
        title: titled(v),
        artist: s(v, "/performer/name").or_else(|| a.map(|a| a.artist.clone())),
        album: a.map(|a| a.title.clone()),
        album_id: a.map(|a| a.id.clone()),
        album_artist: a.map(|a| a.artist.clone()),
        year: a.and_then(|a| a.year.clone()),
        duration: v.get("duration").and_then(Value::as_f64).unwrap_or(0.0),
        track_number: v.get("track_number").and_then(Value::as_u64),
        disc_number: v.get("media_number").and_then(Value::as_u64),
        cover: a.and_then(|a| a.cover.clone()),
        isrc: s(v, "/isrc"),
        hi_res: v.get("hires_streamable").and_then(Value::as_bool).unwrap_or(false) || bits.is_some_and(|b| b > 16),
        bits,
        rate: rate_hz(v, "/maximum_sampling_rate").or(a.and_then(|a| a.rate)),
        streamable: v.get("streamable").and_then(Value::as_bool).unwrap_or(true),
        id,
    }
}

fn items<'a>(v: &'a Value, ptr: &str) -> impl Iterator<Item = &'a Value> {
    v.pointer(ptr).and_then(Value::as_array).into_iter().flatten()
}

impl Client {
    fn get(&self, path: &str, params: &[(&str, &str)]) -> Result<Value> {
        get(path, &self.account.app_id, Some(&self.account.token), params).map_err(|e| {
            if http::status_of(&e) == Some(401) { anyhow!(UserError::SignInExpired { service: "Qobuz".into() }) } else { e }
        })
    }

    pub fn search(&self, query: &str) -> Result<SearchResult> {
        let v = self.get("catalog/search", &[("query", query), ("limit", "40")])?;
        Ok(SearchResult {
            albums: items(&v, "/albums/items").map(album_of).collect(),
            tracks: items(&v, "/tracks/items").map(|t| track_of(t, None)).collect(),
        })
    }

    pub fn album(&self, id: &str) -> Result<AlbumPage> {
        let v = self.get("album/get", &[("album_id", id), ("limit", "500")])?;
        let album = album_of(&v);
        let tracks = items(&v, "/tracks/items").map(|t| track_of(t, Some(&album))).collect();
        Ok(AlbumPage { album, tracks })
    }

    pub fn track(&self, id: &str) -> Result<STrack> {
        Ok(track_of(&self.get("track/get", &[("track_id", id)])?, None))
    }

    pub fn favorite_albums(&self) -> Result<Vec<SAlbum>> {
        let v = self.get("favorite/getUserFavorites", &[("type", "albums"), ("limit", "500")])?;
        Ok(items(&v, "/albums/items").map(album_of).collect())
    }

    pub fn playlists(&self) -> Result<Vec<SPlaylist>> {
        let v = self.get("playlist/getUserPlaylists", &[("limit", "500")])?;
        Ok(items(&v, "/playlists/items").map(playlist_of).collect())
    }

    pub fn playlist(&self, id: &str) -> Result<PlaylistPage> {
        let mut tracks = Vec::new();
        let mut head = None;
        // Long playlists come in pages.
        loop {
            let offset = tracks.len().to_string();
            let v = self.get("playlist/get", &[("playlist_id", id), ("extra", "tracks"), ("limit", "500"), ("offset", &offset)])?;
            let before = tracks.len();
            tracks.extend(items(&v, "/tracks/items").map(|t| track_of(t, None)));
            let total = v.pointer("/tracks/total").and_then(Value::as_u64).unwrap_or(0) as usize;
            head.get_or_insert_with(|| playlist_of(&v));
            if tracks.len() == before || tracks.len() >= total {
                break;
            }
        }
        Ok(PlaylistPage { playlist: head.unwrap_or_default(), tracks })
    }

    /// The FLAC file's address. Asks for the best the quality setting allows; Qobuz answers
    /// with the best the album has up to that.
    pub fn file_url(&self, id: &str, quality: Quality) -> Result<String> {
        let secret = match &self.account.secret {
            Some(s) => s.clone(),
            None => self
                .app
                .secrets
                .iter()
                .find(|s| file_url_call(&self.account.app_id, &self.account.token, s, PROBE_TRACK, 5).is_ok())
                .cloned()
                .ok_or_else(|| anyhow!(UserError::StreamChanged { service: "Qobuz".into() }))?,
        };
        let format = match quality {
            Quality::Cd => 6,
            Quality::Hires96 => 7,
            Quality::Max => 27,
        };
        let v = file_url_call(&self.account.app_id, &self.account.token, &secret, id, format).map_err(|e| match http::status_of(&e) {
            Some(400) => anyhow!(UserError::StreamChanged { service: "Qobuz".into() }),
            Some(401) => anyhow!(UserError::SignInExpired { service: "Qobuz".into() }),
            Some(403) | Some(404) => anyhow!(UserError::StreamRestricted { service: "Qobuz".into() }),
            _ => e,
        })?;
        let sample = v.get("sample").and_then(Value::as_bool).unwrap_or(false);
        let restricted: Vec<String> = items(&v, "/restrictions").filter_map(|r| s(r, "/code")).collect();
        if sample || restricted.iter().any(|c| c.contains("UserUncredentialed") || c.contains("TrackRestrictedByPurchaseCredentials")) {
            bail!(UserError::StreamPreview { service: "Qobuz".into() });
        }
        let url = s(&v, "/url").ok_or_else(|| if restricted.is_empty() { anyhow!("Qobuz gave no file address") } else { anyhow!(UserError::StreamRestricted { service: "Qobuz".into() }) })?;
        if v.get("mime_type").and_then(Value::as_str).is_some_and(|m| m.contains("mpeg")) && format != 5 {
            log::info!("Qobuz serves MP3 for {id}: the plan or the album has no lossless");
        }
        Ok(url)
    }

    /// Tell Qobuz the track started, as its own player does (it counts plays for the artists).
    pub fn report_start(&self, id: &str, quality: Quality) {
        let format = match quality {
            Quality::Cd => 6,
            Quality::Hires96 => 7,
            Quality::Max => 27,
        };
        let date = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let events = serde_json::json!([{ "track_id": id.parse::<u64>().unwrap_or(0), "date": date, "user_id": self.account.user_id, "format_id": format }]).to_string();
        let (app_id, token) = (self.account.app_id.clone(), self.account.token.clone());
        std::thread::spawn(move || {
            let r = api()
                .post(format!("{API}track/reportStreamingStart"))
                .header("X-App-Id", &app_id)
                .header("X-User-Auth-Token", &token)
                .send_form([("events", events.as_str())]);
            if let Err(e) = json(r) {
                log::debug!("Qobuz play report: {e}");
            }
        });
    }
}

fn playlist_of(v: &Value) -> SPlaylist {
    SPlaylist {
        id: s(v, "/id").unwrap_or_default(),
        service: Some(Service::Qobuz),
        name: s(v, "/name").unwrap_or_default(),
        track_count: v.get("tracks_count").and_then(Value::as_u64).unwrap_or(0) as usize,
        cover: items(v, "/images300").chain(items(v, "/images")).find_map(Value::as_str).map(str::to_string),
        owner: s(v, "/owner/name"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_secrets() {
        use base64::Engine as _;
        // A secret split the way the bundle splits it, with 44 characters of padding after.
        let enc = base64::engine::general_purpose::URL_SAFE.encode("0123456789abcdef0123456789abcdef");
        let (seed, rest) = enc.split_at(10);
        let (info, extras) = rest.split_at(10);
        let extras = format!("{extras}{}", "x".repeat(44));
        let script = format!(
            r#"x;production:{{api:{{appId:"123456789",appSecret:"{}"}}}};a.initialSeed("{seed}",window.utimezone.berlin);name:"Europe/Berlin",info:"{info}",extras:"{extras}""#,
            "f".repeat(32)
        );
        let (id, secrets) = read_bundle(&script).unwrap();
        assert_eq!(id, "123456789");
        assert_eq!(secrets, vec!["0123456789abcdef0123456789abcdef".to_string()]);
    }

    #[test]
    fn signature() {
        // md5("trackgetFileUrlformat_id27intentstreamtrack_id1100secret")
        assert_eq!(sign_file_url("1", 27, "100", "secret"), hex(&Md5::digest(b"trackgetFileUrlformat_id27intentstreamtrack_id1100secret")));
    }

    #[test]
    fn parses_tracks() {
        let v: Value = serde_json::from_str(
            r#"{"id":"0060254735180","title":"Kind Of Blue","version":"Legacy Edition","artist":{"name":"Miles Davis"},"image":{"large":"https://x/600.jpg"},
               "release_date_original":"1959-08-17","maximum_bit_depth":24,"maximum_sampling_rate":192,"hires_streamable":true,"tracks_count":2,
               "tracks":{"items":[{"id":19512574,"title":"So What","duration":562,"track_number":1,"media_number":1,"performer":{"name":"Miles Davis"},"isrc":"USSM15900113","maximum_bit_depth":24,"maximum_sampling_rate":192.0,"streamable":true}]}}"#,
        )
        .unwrap();
        let a = album_of(&v);
        assert_eq!(a.title, "Kind Of Blue (Legacy Edition)");
        assert_eq!((a.bits, a.rate, a.hi_res), (Some(24), Some(192_000), true));
        let t = track_of(&v["tracks"]["items"][0], Some(&a));
        assert_eq!(t.uri, "qobuz:track:19512574");
        assert_eq!(t.album.as_deref(), Some("Kind Of Blue (Legacy Edition)"));
        assert_eq!(t.year.as_deref(), Some("1959"));
        assert_eq!(t.isrc.as_deref(), Some("USSM15900113"));
    }
}
