//! TIDAL, as its Android player speaks it (the official developer API gives independent apps
//! 30-second previews only).
//!
//! - Sign-in: TIDAL's own login page with PKCE, under the Android player's client id (the one
//!   tidalapi uses for lossless). The page redirects to tidal.com/android/login/auth?code=….
//! - A track: `playbackinfopostpaywall` hands a manifest — BTS (one FLAC file) or MPEG-DASH
//!   (FLAC in fragmented MP4 segments). Where that answer is encrypted we ask the v2
//!   `trackManifests` endpoint instead; encrypted streams are refused, never decrypted.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::http::{self, api, json};
use crate::{AlbumPage, LoginStart, PlaylistPage, Quality, SAlbum, SPlaylist, STrack, SearchResult, Service};
use hifi_engine::{Hint, MediaSource, UserError};

const AUTH: &str = "https://login.tidal.com/authorize";
const TOKEN: &str = "https://auth.tidal.com/v1/oauth2/token";
const API: &str = "https://api.tidal.com/v1/";
const OPENAPI: &str = "https://openapi.tidal.com/v2/";
const REDIRECT: &str = "https://tidal.com/android/login/auth";
const SCOPE: &str = "r_usr+w_usr+w_sub";

/// The Android player's PKCE client, stored doubly base64-encoded exactly as tidalapi ships it.
fn client() -> (String, String) {
    let b = base64::engine::general_purpose::STANDARD;
    let twice = |a: &str, c: &str| {
        let mut joined = b.decode(a).unwrap_or_default();
        joined.extend(b.decode(c).unwrap_or_default());
        String::from_utf8(b.decode(joined).unwrap_or_default()).unwrap_or_default()
    };
    (
        twice("TmtKRVUxSmtjRXM=", "NWFIRkZRbFJuVlE9PQ=="),
        twice("ZUdWMVVHMVpOMjVpY0ZvNVNVbGlURUZqVVQ=", "a3pjMmhyWVRGV1RtaGxWVUZ4VGpaSlkzTjZhbFJIT0QwPQ=="),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: u64,
    pub user_id: i64,
    pub country: String,
    pub user: Option<String>,
    pub unique_key: String,
}

pub struct Pending {
    verifier: String,
    unique_key: String,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("system randomness");
    b
}

pub fn login_start() -> (LoginStart, Pending) {
    let url_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let verifier = url_b64.encode(random_bytes::<32>());
    let challenge = url_b64.encode(Sha256::digest(verifier.as_bytes()));
    let unique_key: String = random_bytes::<8>().iter().map(|b| format!("{b:02x}")).collect();
    let (id, _) = client();
    let url = format!(
        "{AUTH}?response_type=code&redirect_uri={}&client_id={id}&lang=EN&appMode=android&client_unique_key={unique_key}&code_challenge={challenge}&code_challenge_method=S256&restrict_signup=true",
        http::enc(REDIRECT)
    );
    (LoginStart { url, redirect: REDIRECT.to_string() }, Pending { verifier, unique_key })
}

fn account_from(v: &Value, unique_key: &str, old_refresh: Option<&str>) -> Result<Account> {
    let access = v.get("access_token").and_then(Value::as_str).ok_or_else(|| anyhow!("TIDAL returned no token"))?;
    Ok(Account {
        access_token: access.to_string(),
        refresh_token: v.get("refresh_token").and_then(Value::as_str).or(old_refresh).unwrap_or_default().to_string(),
        expires_at: now() + v.get("expires_in").and_then(Value::as_u64).unwrap_or(3600),
        user_id: v.pointer("/user/userId").and_then(Value::as_i64).unwrap_or(0),
        country: v.pointer("/user/countryCode").and_then(Value::as_str).unwrap_or("US").to_string(),
        user: v.pointer("/user/username").or_else(|| v.pointer("/user/email")).and_then(Value::as_str).map(str::to_string),
        unique_key: unique_key.to_string(),
    })
}

pub fn finish_login(p: &Pending, code: &str) -> Result<Account> {
    let (id, _) = client();
    let v = json(api().post(TOKEN).send_form([
        ("code", code),
        ("client_id", id.as_str()),
        ("grant_type", "authorization_code"),
        ("redirect_uri", REDIRECT),
        ("scope", SCOPE),
        ("code_verifier", p.verifier.as_str()),
        ("client_unique_key", p.unique_key.as_str()),
    ]))
    .context("signing in to TIDAL")?;
    let mut a = account_from(&v, &p.unique_key, None)?;
    if a.user_id == 0 {
        // Older answers leave the user out: ask the session.
        let s = json(api().get(format!("{API}sessions")).header("Authorization", format!("Bearer {}", a.access_token)).call())?;
        a.user_id = s.get("userId").and_then(Value::as_i64).unwrap_or(0);
        if let Some(c) = s.get("countryCode").and_then(Value::as_str) {
            a.country = c.to_string();
        }
    }
    Ok(a)
}

fn refresh(a: &Account) -> Result<Account> {
    let (id, secret) = client();
    let v = json(api().post(TOKEN).send_form([
        ("grant_type", "refresh_token"),
        ("refresh_token", a.refresh_token.as_str()),
        ("client_id", id.as_str()),
        ("client_secret", secret.as_str()),
        ("scope", SCOPE),
    ]))
    .map_err(|e| if matches!(http::status_of(&e), Some(400 | 401)) { anyhow!(UserError::SignInExpired { service: "TIDAL".into() }) } else { e })?;
    let mut fresh = account_from(&v, &a.unique_key, Some(&a.refresh_token))?;
    // A refresh answer may not repeat who the user is.
    if fresh.user_id == 0 {
        fresh.user_id = a.user_id;
        fresh.country = a.country.clone();
        fresh.user = a.user.clone();
    }
    Ok(fresh)
}

pub struct Client {
    pub account: Account,
}

fn s(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr).and_then(|x| match x {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

/// TIDAL image ids are UUIDs; the file lives at their dashes turned into slashes.
fn image(uuid: Option<String>, size: u32) -> Option<String> {
    uuid.map(|u| format!("https://resources.tidal.com/images/{}/{size}x{size}.jpg", u.replace('-', "/")))
}

fn titled(v: &Value) -> String {
    let t = s(v, "/title").unwrap_or_default();
    match s(v, "/version") {
        Some(ver) if !t.contains(&ver) => format!("{t} ({ver})"),
        _ => t,
    }
}

fn hi_res(v: &Value) -> bool {
    v.pointer("/mediaMetadata/tags").and_then(Value::as_array).is_some_and(|t| t.iter().any(|x| x.as_str() == Some("HIRES_LOSSLESS")))
        || v.get("audioQuality").and_then(Value::as_str) == Some("HI_RES_LOSSLESS")
}

fn album_of(v: &Value) -> SAlbum {
    let hi = hi_res(v);
    SAlbum {
        id: s(v, "/id").unwrap_or_default(),
        service: Some(Service::Tidal),
        title: titled(v),
        artist: s(v, "/artist/name").or_else(|| s(v, "/artists/0/name")).unwrap_or_default(),
        year: s(v, "/releaseDate").map(|d| d.chars().take(4).collect()),
        genre: None,
        cover: image(s(v, "/cover"), 640),
        track_count: v.get("numberOfTracks").and_then(Value::as_u64).unwrap_or(0) as usize,
        duration: v.get("duration").and_then(Value::as_f64).unwrap_or(0.0),
        // The catalogue only says "hi-res"; the stream itself tells the exact rate.
        bits: Some(if hi { 24 } else { 16 }),
        rate: (!hi).then_some(44_100),
        hi_res: hi,
    }
}

fn track_of(v: &Value, album: Option<&SAlbum>) -> STrack {
    let own = v.get("album").filter(|a| a.is_object()).map(album_of);
    let a = own.as_ref().or(album);
    let id = s(v, "/id").unwrap_or_default();
    let hi = hi_res(v);
    STrack {
        uri: format!("tidal:track:{id}"),
        service: Some(Service::Tidal),
        title: titled(v),
        artist: s(v, "/artist/name").or_else(|| s(v, "/artists/0/name")),
        album: a.map(|a| a.title.clone()),
        album_id: a.map(|a| a.id.clone()),
        album_artist: album.map(|a| a.artist.clone()),
        year: album.and_then(|a| a.year.clone()).or_else(|| s(v, "/streamStartDate").map(|d| d.chars().take(4).collect())),
        duration: v.get("duration").and_then(Value::as_f64).unwrap_or(0.0),
        track_number: v.get("trackNumber").and_then(Value::as_u64),
        disc_number: v.get("volumeNumber").and_then(Value::as_u64),
        cover: a.and_then(|a| a.cover.clone()).or_else(|| image(s(v, "/album/cover"), 640)),
        isrc: s(v, "/isrc"),
        bits: Some(if hi { 24 } else { 16 }),
        rate: (!hi).then_some(44_100),
        hi_res: hi,
        streamable: v.get("allowStreaming").and_then(Value::as_bool).unwrap_or(true) && v.get("streamReady").and_then(Value::as_bool).unwrap_or(true),
        id,
    }
}

fn items<'a>(v: &'a Value, ptr: &str) -> impl Iterator<Item = &'a Value> {
    v.pointer(ptr).and_then(Value::as_array).into_iter().flatten()
}

impl Client {
    /// The account's token, refreshed first if it is about to run out. Returns whether it was.
    pub fn new(account: Account) -> Result<(Self, bool)> {
        if account.expires_at > now() + 60 {
            return Ok((Self { account }, false));
        }
        Ok((Self { account: refresh(&account)? }, true))
    }

    fn get(&self, path: &str, params: &[(&str, &str)]) -> Result<Value> {
        let mut req = api()
            .get(format!("{API}{path}"))
            .header("Authorization", format!("Bearer {}", self.account.access_token))
            .query("countryCode", &self.account.country);
        for (k, v) in params {
            req = req.query(*k, *v);
        }
        json(req.call()).with_context(|| format!("TIDAL {path}"))
    }

    /// All pages of a list endpoint (`items`, `totalNumberOfItems`), up to `cap` entries.
    fn pages(&self, path: &str, params: &[(&str, &str)], cap: usize) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        loop {
            let offset = out.len().to_string();
            let mut p = params.to_vec();
            p.push(("limit", "100"));
            p.push(("offset", &offset));
            let v = self.get(path, &p)?;
            let before = out.len();
            out.extend(items(&v, "/items").cloned());
            let total = v.get("totalNumberOfItems").and_then(Value::as_u64).unwrap_or(0) as usize;
            if out.len() == before || out.len() >= total.min(cap) {
                return Ok(out);
            }
        }
    }

    pub fn search(&self, query: &str) -> Result<SearchResult> {
        let v = self.get("search", &[("query", query), ("types", "ALBUMS,TRACKS"), ("limit", "40")])?;
        Ok(SearchResult {
            albums: items(&v, "/albums/items").map(album_of).collect(),
            tracks: items(&v, "/tracks/items").map(|t| track_of(t, None)).collect(),
        })
    }

    pub fn album(&self, id: &str) -> Result<AlbumPage> {
        let album = album_of(&self.get(&format!("albums/{id}"), &[])?);
        let tracks = self.pages(&format!("albums/{id}/tracks"), &[], 1000)?.iter().map(|t| track_of(t, Some(&album))).collect();
        Ok(AlbumPage { album, tracks })
    }

    pub fn track(&self, id: &str) -> Result<STrack> {
        Ok(track_of(&self.get(&format!("tracks/{id}"), &[])?, None))
    }

    pub fn favorite_albums(&self) -> Result<Vec<SAlbum>> {
        let uid = self.account.user_id;
        let list = self.pages(&format!("users/{uid}/favorites/albums"), &[("order", "DATE"), ("orderDirection", "DESC")], 2000)?;
        Ok(list.iter().filter_map(|i| i.get("item")).map(album_of).collect())
    }

    pub fn playlists(&self) -> Result<Vec<SPlaylist>> {
        let uid = self.account.user_id;
        // The user's own lists and the ones they follow; older accounts only answer the first.
        let list = match self.pages(&format!("users/{uid}/playlistsAndFavoritePlaylists"), &[], 1000) {
            Ok(l) => l.into_iter().filter_map(|i| i.get("playlist").cloned()).collect(),
            Err(_) => self.pages(&format!("users/{uid}/playlists"), &[], 1000)?,
        };
        Ok(list.iter().map(playlist_of).collect())
    }

    pub fn playlist(&self, id: &str) -> Result<PlaylistPage> {
        let head = playlist_of(&self.get(&format!("playlists/{id}"), &[])?);
        let tracks = self.pages(&format!("playlists/{id}/tracks"), &[], 10_000)?.iter().map(|t| track_of(t, None)).collect();
        Ok(PlaylistPage { playlist: head, tracks })
    }

    /// The track as a byte stream: FLAC (BTS) or FLAC in fragmented MP4 (DASH).
    pub fn stream(&self, id: &str, quality: Quality) -> Result<(Box<dyn MediaSource>, Hint)> {
        let q = if quality == Quality::Cd { "LOSSLESS" } else { "HI_RES_LOSSLESS" };
        let first = self
            .get(&format!("tracks/{id}/playbackinfopostpaywall"), &[("audioquality", q), ("playbackmode", "STREAM"), ("assetpresentation", "FULL")])
            .and_then(|v| v1_manifest(&v));
        match first {
            Ok(m) => open_manifest(m),
            Err(e) => {
                log::info!("TIDAL v1 stream for {id}: {e}; trying v2 trackManifests");
                match self.v2_manifest(id, quality) {
                    Ok(m) => open_manifest(m),
                    // The first reason is usually the telling one.
                    Err(e2) => Err(if refused(&e) { e } else { e2 }),
                }
            }
        }
    }

    fn v2_manifest(&self, id: &str, quality: Quality) -> Result<Manifest> {
        let mut req = api()
            .get(format!("{OPENAPI}trackManifests/{id}"))
            .header("Authorization", format!("Bearer {}", self.account.access_token))
            .header("Accept", "application/vnd.api+json")
            .query("manifestType", "MPEG_DASH")
            .query("uriScheme", "DATA")
            .query("usage", "PLAYBACK")
            .query("adaptive", "false");
        if quality != Quality::Cd {
            req = req.query("formats", "FLAC_HIRES");
        }
        req = req.query("formats", "FLAC");
        let v = json(req.call()).context("TIDAL trackManifests")?;
        let attr = v.pointer("/data/attributes").ok_or_else(|| anyhow!("TIDAL sent no manifest"))?;
        if attr.get("trackPresentation").and_then(Value::as_str) == Some("PREVIEW") {
            bail!(UserError::StreamPreview { service: "TIDAL".into() });
        }
        if attr.get("drmData").is_some_and(|d| !d.is_null()) {
            bail!(UserError::StreamEncrypted { service: "TIDAL".into() });
        }
        let uri = attr.get("uri").and_then(Value::as_str).ok_or_else(|| anyhow!("TIDAL sent no manifest"))?;
        let bytes = match http::data_url(uri) {
            Some(b) => b,
            None => http::bytes(uri, 4 << 20)?,
        };
        Ok(Manifest::Dash(String::from_utf8_lossy(&bytes).into_owned()))
    }
}

fn playlist_of(v: &Value) -> SPlaylist {
    SPlaylist {
        id: s(v, "/uuid").unwrap_or_default(),
        service: Some(Service::Tidal),
        name: s(v, "/title").unwrap_or_default(),
        track_count: v.get("numberOfTracks").and_then(Value::as_u64).unwrap_or(0) as usize,
        cover: image(s(v, "/squareImage"), 320).or_else(|| s(v, "/image").map(|u| format!("https://resources.tidal.com/images/{}/320x214.jpg", u.replace('-', "/")))),
        owner: s(v, "/creator/name"),
    }
}

/// The service won't give this track as a plain stream: no point trying another way.
fn refused(e: &anyhow::Error) -> bool {
    matches!(UserError::of(e), UserError::StreamPreview { .. } | UserError::StreamEncrypted { .. })
}

#[derive(Debug)]
enum Manifest {
    /// One plain file.
    File { url: String, mp4: bool },
    Dash(String),
}

fn v1_manifest(v: &Value) -> Result<Manifest> {
    if v.get("assetPresentation").and_then(Value::as_str) == Some("PREVIEW") {
        bail!(UserError::StreamPreview { service: "TIDAL".into() });
    }
    let mime = v.get("manifestMimeType").and_then(Value::as_str).unwrap_or("");
    let raw = v.get("manifest").and_then(Value::as_str).ok_or_else(|| anyhow!("no manifest"))?;
    let text = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(raw)?)?;
    if mime.contains("dash+xml") {
        return Ok(Manifest::Dash(text));
    }
    if !mime.contains("vnd.tidal.bts") {
        bail!("manifest {mime} (an encrypted stream)");
    }
    let m: Value = serde_json::from_str(&text)?;
    let enc = m.get("encryptionType").and_then(Value::as_str).unwrap_or("NONE");
    if enc != "NONE" {
        bail!("the stream is encrypted ({enc})");
    }
    let url = items(&m, "/urls").find_map(Value::as_str).ok_or_else(|| anyhow!("the manifest has no address"))?;
    let codec = m.get("codecs").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    let mp4 = m.get("mimeType").and_then(Value::as_str).is_some_and(|t| t.contains("mp4"));
    if !codec.contains("flac") && !codec.is_empty() {
        log::info!("TIDAL serves {codec} (not FLAC) for this track");
    }
    Ok(Manifest::File { url: url.to_string(), mp4 })
}

fn open_manifest(m: Manifest) -> Result<(Box<dyn MediaSource>, Hint)> {
    let mut hint = Hint::new();
    match m {
        Manifest::File { url, mp4 } => {
            hint.with_extension(if mp4 { "mp4" } else { "flac" });
            Ok((Box::new(http::Stream::file(&url)?), hint))
        }
        Manifest::Dash(mpd) => {
            let urls = dash_segments(&mpd)?;
            hint.with_extension("mp4");
            Ok((Box::new(http::Stream::segments(urls, Vec::new())), hint))
        }
    }
}

/// ISO 8601 duration ("PT3M45.120S") in seconds.
fn iso_duration(s: &str) -> Option<f64> {
    let t = s.strip_prefix("PT").or_else(|| s.split_once('T').map(|x| x.1))?;
    let mut total = 0.0;
    let mut num = String::new();
    for c in t.chars() {
        match c {
            'H' => total += num.parse::<f64>().ok()? * 3600.0,
            'M' => total += num.parse::<f64>().ok()? * 60.0,
            'S' => total += num.parse::<f64>().ok()?,
            _ => {
                num.push(c);
                continue;
            }
        }
        num.clear();
    }
    Some(total)
}

/// The init segment and every media segment of the best representation, in order.
fn dash_segments(mpd: &str) -> Result<Vec<String>> {
    let doc = roxmltree::Document::parse(mpd).context("reading the DASH manifest")?;
    let named = |n: &roxmltree::Node, name: &str| n.is_element() && n.tag_name().name() == name;
    if doc.descendants().any(|n| named(&n, "ContentProtection")) {
        bail!(UserError::StreamEncrypted { service: "TIDAL".into() });
    }
    let rep = doc
        .descendants()
        .filter(|n| named(n, "Representation"))
        .max_by_key(|n| n.attribute("bandwidth").and_then(|b| b.parse::<u64>().ok()).unwrap_or(0))
        .ok_or_else(|| anyhow!("the DASH manifest has no representation"))?;
    // The template sits on the representation or on its adaptation set.
    let tmpl = rep
        .children()
        .find(|c| named(c, "SegmentTemplate"))
        .or_else(|| rep.parent().and_then(|p| p.children().find(|c| named(c, "SegmentTemplate"))))
        .ok_or_else(|| anyhow!("the DASH manifest has no segment template"))?;
    let base = doc.descendants().find(|n| named(n, "BaseURL")).and_then(|n| n.text()).map(str::trim).unwrap_or("");
    let rep_id = rep.attribute("id").unwrap_or("");
    let bw = rep.attribute("bandwidth").unwrap_or("");
    let fill = |t: &str, number: Option<u64>| {
        let mut u = t.replace("$RepresentationID$", rep_id).replace("$Bandwidth$", bw);
        if let Some(n) = number {
            u = u.replace("$Number$", &n.to_string());
        }
        if u.starts_with("http") || base.is_empty() { u } else { format!("{base}{u}") }
    };
    let media = tmpl.attribute("media").ok_or_else(|| anyhow!("the DASH template has no media"))?;
    let start = tmpl.attribute("startNumber").and_then(|s| s.parse().ok()).unwrap_or(1u64);
    let count = match tmpl.children().find(|c| named(c, "SegmentTimeline")) {
        Some(tl) => tl
            .children()
            .filter(|c| named(c, "S"))
            .map(|s| 1 + s.attribute("r").and_then(|r| r.parse::<i64>().ok()).unwrap_or(0).max(0) as u64)
            .sum(),
        None => {
            let total = doc.root_element().attribute("mediaPresentationDuration").and_then(iso_duration).ok_or_else(|| anyhow!("the DASH manifest has no duration"))?;
            let timescale: f64 = tmpl.attribute("timescale").and_then(|t| t.parse().ok()).unwrap_or(1.0);
            let dur: f64 = tmpl.attribute("duration").and_then(|t| t.parse().ok()).ok_or_else(|| anyhow!("the DASH template has no segment duration"))?;
            (total * timescale / dur).ceil() as u64
        }
    };
    if count == 0 || count > 100_000 {
        bail!("the DASH manifest lists {count} segments");
    }
    let mut urls = Vec::with_capacity(count as usize + 1);
    if let Some(init) = tmpl.attribute("initialization") {
        urls.push(fill(init, None));
    }
    urls.extend((start..start + count).map(|n| fill(media, Some(n))));
    Ok(urls)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_decodes() {
        let (id, secret) = client();
        assert_eq!(id.len(), 16);
        assert!(!secret.is_empty());
    }

    #[test]
    fn pkce_challenge_is_sha256_of_verifier() {
        let (start, p) = login_start();
        let challenge = http::query_param(&start.url, "code_challenge").unwrap();
        let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(challenge, expect);
        assert!(start.url.contains("redirect_uri=https%3A%2F%2Ftidal.com%2Fandroid%2Flogin%2Fauth"));
    }

    #[test]
    fn dash_timeline() {
        let mpd = r#"<?xml version='1.0' encoding='UTF-8'?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" mediaPresentationDuration="PT2M5.000S" type="static">
 <Period id="0"><AdaptationSet id="0" contentType="audio" mimeType="audio/mp4" segmentAlignment="true">
  <Representation id="FLAC,96000,24" codecs="flac" bandwidth="2300000" audioSamplingRate="96000">
   <SegmentTemplate timescale="96000" initialization="https://sp.tidal.com/a/0.mp4?t=1&amp;x=2" media="https://sp.tidal.com/a/$Number$.mp4?t=1&amp;x=2" startNumber="1">
    <SegmentTimeline><S d="383795" r="2"/><S d="100000"/></SegmentTimeline>
   </SegmentTemplate>
  </Representation>
 </AdaptationSet></Period></MPD>"#;
        let urls = dash_segments(mpd).unwrap();
        assert_eq!(urls.len(), 5);
        assert_eq!(urls[0], "https://sp.tidal.com/a/0.mp4?t=1&x=2");
        assert_eq!(urls[4], "https://sp.tidal.com/a/4.mp4?t=1&x=2");
    }

    #[test]
    fn dash_without_timeline_and_drm_refused() {
        let mpd = r#"<MPD mediaPresentationDuration="PT10S"><Period><AdaptationSet><SegmentTemplate timescale="1000" duration="4000" media="s$Number$.m4s" initialization="init.mp4"/><Representation id="r" bandwidth="1"/></AdaptationSet></Period></MPD>"#;
        assert_eq!(dash_segments(mpd).unwrap(), vec!["init.mp4", "s1.m4s", "s2.m4s", "s3.m4s"]);
        let drm = r#"<MPD><Period><AdaptationSet><ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011"/><Representation id="r"/></AdaptationSet></Period></MPD>"#;
        assert!(refused(&dash_segments(drm).unwrap_err()));
    }

    #[test]
    fn bts_manifest() {
        let m = base64::engine::general_purpose::STANDARD.encode(r#"{"mimeType":"audio/flac","codecs":"flac","encryptionType":"NONE","urls":["https://x/y.flac"]}"#);
        let v = serde_json::json!({"manifestMimeType":"application/vnd.tidal.bts","manifest":m,"assetPresentation":"FULL"});
        assert!(matches!(v1_manifest(&v).unwrap(), Manifest::File { mp4: false, .. }));
        let enc = base64::engine::general_purpose::STANDARD.encode(r#"{"mimeType":"audio/flac","encryptionType":"OLD_AES","urls":["https://x"]}"#);
        let v = serde_json::json!({"manifestMimeType":"application/vnd.tidal.bts","manifest":enc});
        assert!(v1_manifest(&v).is_err());
        let v = serde_json::json!({"manifestMimeType":"application/vnd.tidal.emu","manifest":"e30="});
        assert!(v1_manifest(&v).is_err());
    }
}
