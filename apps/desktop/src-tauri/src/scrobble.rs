//! Scrobbling: telling Last.fm and ListenBrainz what was listened to.
//!
//! Off until the user connects an account in Settings: without one nothing here makes a
//! request or keeps a listen. With one, the service gets the track that plays now (artist,
//! title, album, length) and, once a track counts as listened to (hifi_engine::plays — the
//! rule the play counts use), the same with the time it started. Disconnecting ends it at
//! once and drops what was still waiting to go.
//!
//! Listens that can't be delivered (no network, the service is down) wait in
//! scrobble-queue.json and go out later, oldest first. The accounts are kept in
//! scrobbling.json, the way the streaming sign-ins are: a file of the app's own, readable by
//! its owner only.
//!
//! ListenBrainz takes the user's own token. Last.fm needs an API key and secret of the app:
//! they are given to the build (BITSPRING_LASTFM_KEY, BITSPRING_LASTFM_SECRET) and are not in
//! the repository; a build made without them has no Last.fm.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use hifi_engine::UserError;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{AppHandle, Manager, State};

/// Listens sent in one request (Last.fm takes at most 50).
const BATCH: usize = 50;
/// Listens kept waiting at most; beyond it the oldest go.
const QUEUE_MAX: usize = 10_000;
/// How long a Last.fm sign-in waits for the approval in the browser.
const APPROVAL_WAIT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Service {
    Lastfm,
    Listenbrainz,
}

impl Service {
    const ALL: [Service; 2] = [Service::Lastfm, Service::Listenbrainz];

    fn name(self) -> &'static str {
        match self {
            Service::Lastfm => "Last.fm",
            Service::Listenbrainz => "ListenBrainz",
        }
    }
}

/// What a service is told about a track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Song {
    pub artist: String,
    pub title: String,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub album_artist: Option<String>,
    #[serde(default)]
    pub track_number: Option<u64>,
    /// Seconds; 0 when not known.
    #[serde(default)]
    pub duration: f64,
}

/// A listen waiting to be sent to one service.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Waiting {
    id: u64,
    service: Service,
    /// When the track started (Unix seconds).
    at: u64,
    #[serde(flatten)]
    song: Song,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Account {
    user: String,
    /// ListenBrainz: the user's token. Last.fm: the session key the sign-in gave.
    key: String,
    /// The service no longer accepts the key: nothing is sent until the user connects again.
    #[serde(default)]
    rejected: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Accounts {
    #[serde(default)]
    lastfm: Option<Account>,
    #[serde(default)]
    listenbrainz: Option<Account>,
}

impl Accounts {
    fn get(&self, s: Service) -> Option<&Account> {
        match s {
            Service::Lastfm => self.lastfm.as_ref(),
            Service::Listenbrainz => self.listenbrainz.as_ref(),
        }
    }

    fn slot(&mut self, s: Service) -> &mut Option<Account> {
        match s {
            Service::Lastfm => &mut self.lastfm,
            Service::Listenbrainz => &mut self.listenbrainz,
        }
    }
}

/// Why a request didn't go through.
#[derive(Debug, PartialEq)]
enum Failure {
    /// No answer, or the service has trouble of its own: the same request may work later.
    Later(String),
    /// The service doesn't accept the sign-in (any more).
    Rejected,
    /// The service refuses this request as it is: sending it again would fail again.
    Refused(String),
}

/// Where the services are, and the app's Last.fm key. (Tests point it at a stand-in server.)
#[derive(Clone)]
struct Api {
    listenbrainz: String,
    lastfm: String,
    lastfm_auth: String,
    /// The app's Last.fm API key and secret; None: this build has no Last.fm.
    lastfm_app: Option<(String, String)>,
    /// How long to wait before trying again after this many failures in a row.
    retry: fn(u32) -> Duration,
    /// How often a Last.fm sign-in asks whether the user has approved it in the browser.
    poll: Duration,
}

impl Api {
    fn real() -> Self {
        let given = |v: Option<&'static str>| v.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        Self {
            listenbrainz: "https://api.listenbrainz.org".into(),
            lastfm: "https://ws.audioscrobbler.com/2.0/".into(),
            lastfm_auth: "https://www.last.fm/api/auth/".into(),
            lastfm_app: given(option_env!("BITSPRING_LASTFM_KEY")).zip(given(option_env!("BITSPRING_LASTFM_SECRET"))),
            retry: |failures| Duration::from_secs(match failures {
                0 | 1 => 30,
                2 => 60,
                3 => 2 * 60,
                4 => 5 * 60,
                5 => 15 * 60,
                _ => 30 * 60,
            }),
            poll: Duration::from_secs(3),
        }
        .or_stand_in()
    }

    /// A debug build can be pointed at a stand-in for both services
    /// (BITSPRING_SCROBBLE_STAND_IN=http://127.0.0.1:port), to try the whole way from the
    /// player to a request without touching anybody's real profile. Release builds can't.
    fn or_stand_in(mut self) -> Self {
        #[cfg(debug_assertions)]
        if let Ok(at) = std::env::var("BITSPRING_SCROBBLE_STAND_IN") {
            let at = at.trim_end_matches('/');
            self.listenbrainz = at.to_string();
            self.lastfm = format!("{at}/2.0/");
            self.lastfm_auth = format!("{at}/auth");
            self.lastfm_app.get_or_insert_with(|| ("stand-in".into(), "stand-in".into()));
            self.retry = |_| Duration::from_secs(5);
        }
        self
    }
}

fn agent() -> &'static ureq::Agent {
    static A: OnceLock<ureq::Agent> = OnceLock::new();
    A.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(25)))
            .user_agent(concat!("BitSpring/", env!("CARGO_PKG_VERSION"), " (https://bitspring.app)"))
            .http_status_as_error(false)
            .build()
            .into()
    })
}

/// The answer's status and body; a request that got no answer is for later.
fn answer(resp: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<(u16, String), Failure> {
    let mut resp = resp.map_err(|e| Failure::Later(e.to_string()))?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().with_config().limit(1 << 20).read_to_string().map_err(|e| Failure::Later(e.to_string()))?;
    Ok((status, text))
}

// --- ListenBrainz ------------------------------------------------------------------------------

fn lb_failure(status: u16, text: &str) -> Failure {
    let said = serde_json::from_str::<Value>(text).ok().and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string));
    let detail = said.unwrap_or_else(|| format!("HTTP {status}"));
    match status {
        401 | 403 => Failure::Rejected,
        429 | 500.. => Failure::Later(detail),
        _ => Failure::Refused(detail),
    }
}

/// The name of the user `token` belongs to.
fn lb_validate(api: &Api, token: &str) -> Result<String, Failure> {
    let (status, text) = answer(agent().get(format!("{}/1/validate-token", api.listenbrainz)).header("Authorization", format!("Token {token}")).call())?;
    if status != 200 {
        return Err(lb_failure(status, &text));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| Failure::Later(e.to_string()))?;
    match (v.get("valid").and_then(Value::as_bool), v.get("user_name").and_then(Value::as_str)) {
        (Some(true), Some(user)) => Ok(user.to_string()),
        _ => Err(Failure::Rejected),
    }
}

/// One listen as ListenBrainz takes it; `at`: None for "playing now".
fn lb_listen(song: &Song, at: Option<u64>) -> Value {
    let mut info = json!({
        "media_player": "BitSpring",
        "submission_client": "BitSpring",
        "submission_client_version": env!("CARGO_PKG_VERSION"),
    });
    if song.duration > 0.0 {
        info["duration_ms"] = json!((song.duration * 1000.0).round() as u64);
    }
    if let Some(n) = song.track_number {
        info["tracknumber"] = json!(n);
    }
    let mut meta = json!({ "artist_name": song.artist, "track_name": song.title, "additional_info": info });
    if let Some(album) = &song.album {
        meta["release_name"] = json!(album);
    }
    let mut listen = json!({ "track_metadata": meta });
    if let Some(at) = at {
        listen["listened_at"] = json!(at);
    }
    listen
}

fn lb_submit(api: &Api, token: &str, kind: &str, payload: Vec<Value>) -> Result<(), Failure> {
    let body = json!({ "listen_type": kind, "payload": payload }).to_string();
    let (status, text) = answer(
        agent()
            .post(format!("{}/1/submit-listens", api.listenbrainz))
            .header("Authorization", format!("Token {token}"))
            .header("Content-Type", "application/json")
            .send(body.as_str()),
    )?;
    if status == 200 { Ok(()) } else { Err(lb_failure(status, &text)) }
}

// --- Last.fm -----------------------------------------------------------------------------------

/// The signature Last.fm asks for: the parameters in name order, each name followed by its
/// value, then the app's secret — as an MD5 in hex.
fn lfm_sign(params: &[(String, String)], secret: &str) -> String {
    let mut sorted: Vec<&(String, String)> = params.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hash = Md5::new();
    for (k, v) in sorted {
        hash.update(k.as_bytes());
        hash.update(v.as_bytes());
    }
    hash.update(secret.as_bytes());
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// `%XX`-encode a form value.
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Last.fm's answer when it isn't the result: no answer at all, or one of its error codes.
#[derive(Debug)]
enum LfmFail {
    Net(String),
    Code(i64, String),
}

/// Not approved in the browser yet (while signing in).
const LFM_NOT_AUTHORIZED: i64 = 14;

impl From<LfmFail> for Failure {
    fn from(f: LfmFail) -> Self {
        match f {
            LfmFail::Net(e) => Failure::Later(e),
            // The session (4, 9) or the token it came from (14, 15) isn't accepted; the
            // app's own key isn't (10, 26).
            LfmFail::Code(4 | 9 | 10 | 14 | 15 | 26, _) => Failure::Rejected,
            // The service is down or busy (8, 11, 16), or we ask too often (29).
            LfmFail::Code(8 | 11 | 16 | 29, m) => Failure::Later(m),
            LfmFail::Code(_, m) => Failure::Refused(m),
        }
    }
}

/// Call a Last.fm method, signed. `write`: a POST (what changes something must be one).
fn lfm_call(api: &Api, method: &str, mut params: Vec<(String, String)>, write: bool) -> Result<Value, LfmFail> {
    let (key, secret) = api.lastfm_app.as_ref().ok_or_else(|| LfmFail::Net("this build has no Last.fm".into()))?;
    params.push(("method".into(), method.into()));
    params.push(("api_key".into(), key.clone()));
    let sig = lfm_sign(&params, secret);
    params.push(("api_sig".into(), sig));
    // (Not part of the signature.)
    params.push(("format".into(), "json".into()));
    let resp = if write {
        let body = params.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&");
        agent().post(&api.lastfm).header("Content-Type", "application/x-www-form-urlencoded").send(body.as_str())
    } else {
        let mut get = agent().get(&api.lastfm);
        for (k, v) in &params {
            get = get.query(k, v);
        }
        get.call()
    };
    let (status, text) = answer(resp).map_err(|f| match f {
        Failure::Later(e) | Failure::Refused(e) => LfmFail::Net(e),
        Failure::Rejected => LfmFail::Net("no answer".into()),
    })?;
    let v: Value = serde_json::from_str(&text).map_err(|_| LfmFail::Net(format!("HTTP {status}")))?;
    match v.get("error").and_then(Value::as_i64) {
        Some(code) => Err(LfmFail::Code(code, v.get("message").and_then(Value::as_str).unwrap_or("").to_string())),
        None if status >= 400 => Err(LfmFail::Net(format!("HTTP {status}"))),
        None => Ok(v),
    }
}

fn lfm_song(song: &Song, index: Option<usize>, out: &mut Vec<(String, String)>) {
    let name = |k: &str| index.map_or_else(|| k.to_string(), |i| format!("{k}[{i}]"));
    out.push((name("artist"), song.artist.clone()));
    out.push((name("track"), song.title.clone()));
    if let Some(album) = &song.album {
        out.push((name("album"), album.clone()));
    }
    // Only where it says something the artist doesn't.
    if let Some(aa) = song.album_artist.as_ref().filter(|aa| **aa != song.artist) {
        out.push((name("albumArtist"), aa.clone()));
    }
    if let Some(n) = song.track_number {
        out.push((name("trackNumber"), n.to_string()));
    }
    if song.duration > 0.0 {
        out.push((name("duration"), (song.duration.round() as u64).to_string()));
    }
}

// --- both --------------------------------------------------------------------------------------

impl Api {
    fn now_playing(&self, service: Service, key: &str, song: &Song) -> Result<(), Failure> {
        match service {
            Service::Listenbrainz => lb_submit(self, key, "playing_now", vec![lb_listen(song, None)]),
            Service::Lastfm => {
                let mut params = vec![("sk".to_string(), key.to_string())];
                lfm_song(song, None, &mut params);
                lfm_call(self, "track.updateNowPlaying", params, true).map(drop).map_err(Failure::from)
            }
        }
    }

    fn submit(&self, service: Service, key: &str, listens: &[Waiting]) -> Result<(), Failure> {
        match service {
            Service::Listenbrainz => {
                let kind = if listens.len() == 1 { "single" } else { "import" };
                lb_submit(self, key, kind, listens.iter().map(|w| lb_listen(&w.song, Some(w.at))).collect())
            }
            Service::Lastfm => {
                let mut params = vec![("sk".to_string(), key.to_string())];
                for (i, w) in listens.iter().enumerate() {
                    lfm_song(&w.song, Some(i), &mut params);
                    params.push((format!("timestamp[{i}]"), w.at.to_string()));
                }
                // (A listen Last.fm ignores — too old, say — comes back as ignored in a good
                // answer: it is done with, like the accepted ones.)
                lfm_call(self, "track.scrobble", params, true).map(drop).map_err(Failure::from)
            }
        }
    }
}

/// How the sending to one service is going.
#[derive(Default, Clone, Copy)]
struct Health {
    /// Failures in a row (no network, service down).
    failures: u32,
    retry_at: Option<Instant>,
    /// A batch was refused: its listens go one at a time, so the bad one can be told apart.
    one_by_one: bool,
}

#[derive(Default)]
struct Inner {
    accounts: Accounts,
    queue: VecDeque<Waiting>,
    next_id: u64,
    /// The track to announce as playing now, until the worker has.
    now: Option<Song>,
    health: [Health; 2],
    /// Whose turn it is, so a long backlog for one service doesn't hold up the other.
    turn: usize,
    /// The Last.fm sign-in under way (its number: a newer one, or cancelling, ends it).
    connecting: Option<u64>,
    sign_ins: u64,
    /// Why the last Last.fm sign-in ended without an account.
    sign_in_failed: Option<UserError>,
    stop: bool,
}

struct Shared {
    api: Api,
    dir: Option<PathBuf>,
    state: Mutex<Inner>,
    wake: Condvar,
}

#[derive(Clone)]
pub struct Scrobbling(Arc<Shared>);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    service: Service,
    name: &'static str,
    /// This build can talk to it (Last.fm: it was built with the app's key).
    available: bool,
    /// The connected account.
    user: Option<String>,
    /// The service no longer accepts the sign-in: connect again.
    rejected: bool,
    /// Listens waiting to be sent.
    waiting: usize,
    /// The last try didn't get through (no network, the service is down); it is tried again.
    offline: bool,
    /// Waiting for the user to approve the sign-in in the browser.
    connecting: bool,
    /// Why the last sign-in ended without an account.
    failed: Option<UserError>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn save_accounts(&self, st: &Inner) {
        if let Some(d) = &self.dir {
            // (Owner-only on Unix, like every file store::save_json writes.)
            let _ = hifi_engine::store::save_json(&d.join("scrobbling.json"), &st.accounts);
        }
    }

    fn save_queue(&self, st: &Inner) {
        if let Some(d) = &self.dir {
            // Best effort: a failed write costs the listens of this session if it ends offline.
            let _ = hifi_engine::store::save_json(&d.join("scrobble-queue.json"), &st.queue);
        }
    }
}

impl Scrobbling {
    pub fn load(app: &AppHandle) -> Self {
        Self::open(app.path().app_data_dir().ok(), Api::real())
    }

    /// No accounts, no storage, no worker: for the statistics' tests.
    #[cfg(test)]
    pub fn off() -> Self {
        Self(Arc::new(Shared { api: Api::real(), dir: None, state: Mutex::default(), wake: Condvar::new() }))
    }

    fn open(dir: Option<PathBuf>, api: Api) -> Self {
        let load = |name: &str| dir.as_ref().map(|d| d.join(name));
        let accounts: Accounts = load("scrobbling.json").as_deref().and_then(hifi_engine::store::load_json).unwrap_or_default();
        let queue: VecDeque<Waiting> = load("scrobble-queue.json").as_deref().and_then(hifi_engine::store::load_json).unwrap_or_default();
        let next_id = queue.iter().map(|w| w.id + 1).max().unwrap_or(0);
        let s = Self(Arc::new(Shared { api, dir, state: Mutex::new(Inner { accounts, queue, next_id, ..Inner::default() }), wake: Condvar::new() }));
        let shared = s.0.clone();
        // Sleeps until there is something to send; with no account there never is.
        let _ = std::thread::Builder::new().name("bitspring-scrobble".into()).spawn(move || work(&shared));
        s
    }

    /// End the worker (tests: the app's own lives as long as the app).
    #[cfg(test)]
    fn shutdown(&self) {
        self.0.lock().stop = true;
        self.0.wake.notify_all();
    }

    /// `song` has started playing: tell the connected services.
    pub fn now_playing(&self, song: &Song) {
        let mut st = self.0.lock();
        if Service::ALL.iter().any(|s| st.accounts.get(*s).is_some_and(|a| !a.rejected)) {
            st.now = Some(song.clone());
            drop(st);
            self.0.wake.notify_all();
        }
    }

    /// `song`, started at `at`, counts as listened to: queue it for every connected service.
    pub fn listened(&self, song: &Song, at: u64) {
        let mut st = self.0.lock();
        let mut added = false;
        for service in Service::ALL {
            // (Also for an account the service has rejected: connecting again sends them.)
            if st.accounts.get(service).is_some() {
                let id = st.next_id;
                st.next_id += 1;
                st.queue.push_back(Waiting { id, service, at, song: song.clone() });
                added = true;
            }
        }
        if !added {
            return;
        }
        while st.queue.len() > QUEUE_MAX {
            st.queue.pop_front();
        }
        self.0.save_queue(&st);
        drop(st);
        self.0.wake.notify_all();
    }

    pub fn status(&self) -> Vec<ServiceStatus> {
        let st = self.0.lock();
        Service::ALL
            .iter()
            .map(|&service| {
                let account = st.accounts.get(service);
                let lastfm = service == Service::Lastfm;
                ServiceStatus {
                    service,
                    name: service.name(),
                    available: !lastfm || self.0.api.lastfm_app.is_some(),
                    user: account.map(|a| a.user.clone()),
                    rejected: account.is_some_and(|a| a.rejected),
                    waiting: st.queue.iter().filter(|w| w.service == service).count(),
                    offline: account.is_some() && st.health[service as usize].failures > 0,
                    connecting: lastfm && st.connecting.is_some(),
                    failed: st.sign_in_failed.clone().filter(|_| lastfm),
                }
            })
            .collect()
    }

    fn connected(&self, service: Service, account: Account) {
        let mut st = self.0.lock();
        *st.accounts.slot(service) = Some(account);
        st.health[service as usize] = Health::default();
        self.0.save_accounts(&st);
        drop(st);
        // What waited for this account (while the service rejected the old key) goes now.
        self.0.wake.notify_all();
    }

    /// Connect ListenBrainz with the user's token (from listenbrainz.org → Settings).
    pub fn connect_token(&self, service: Service, token: &str) -> Result<(), UserError> {
        if service != Service::Listenbrainz {
            return Err(UserError::other("this service signs in in the browser"));
        }
        let token = token.trim();
        let name = service.name().to_string();
        let user = lb_validate(&self.0.api, token).map_err(|f| match f {
            Failure::Rejected => UserError::TokenRefused { service: name },
            Failure::Later(detail) | Failure::Refused(detail) => UserError::ServiceUnreachable { service: name, detail },
        })?;
        self.connected(service, Account { user, key: token.to_string(), rejected: false });
        Ok(())
    }

    /// Start a Last.fm sign-in: returns the page to open in the browser, where the user lets
    /// BitSpring scrobble to their account. Meanwhile we keep asking Last.fm whether they
    /// have; the status says how it ended.
    pub fn sign_in(&self, service: Service) -> Result<String, UserError> {
        let api = &self.0.api;
        let name = service.name().to_string();
        let (Service::Lastfm, Some((key, _))) = (service, &api.lastfm_app) else {
            return Err(UserError::other("this version of the app can't sign in to this service"));
        };
        let unreachable = |f: LfmFail| UserError::ServiceUnreachable {
            service: name.clone(),
            detail: match f {
                LfmFail::Net(e) | LfmFail::Code(_, e) => e,
            },
        };
        let v = lfm_call(api, "auth.getToken", Vec::new(), false).map_err(unreachable)?;
        let token = v.get("token").and_then(Value::as_str).ok_or_else(|| UserError::ServiceUnreachable { service: name.clone(), detail: "no token".into() })?.to_string();
        let number = {
            let mut st = self.0.lock();
            st.sign_ins += 1;
            st.connecting = Some(st.sign_ins);
            st.sign_in_failed = None;
            st.sign_ins
        };
        let url = format!("{}?api_key={}&token={}", api.lastfm_auth, enc(key), enc(&token));
        let me = self.clone();
        let _ = std::thread::Builder::new().name("bitspring-lastfm-sign-in".into()).spawn(move || me.await_approval(number, &token));
        Ok(url)
    }

    fn await_approval(&self, number: u64, token: &str) {
        let api = &self.0.api;
        let deadline = Instant::now() + APPROVAL_WAIT;
        let end = |failed: Option<UserError>| {
            let mut st = self.0.lock();
            if st.connecting == Some(number) {
                st.connecting = None;
                st.sign_in_failed = failed;
            }
        };
        loop {
            std::thread::sleep(api.poll);
            if self.0.lock().connecting != Some(number) {
                return; // cancelled, or started anew
            }
            match lfm_call(api, "auth.getSession", vec![("token".into(), token.into())], false) {
                Ok(v) => {
                    let field = |k: &str| v.pointer(&format!("/session/{k}")).and_then(Value::as_str).map(str::to_string);
                    match (field("name"), field("key")) {
                        (Some(user), Some(key)) => {
                            if self.0.lock().connecting == Some(number) {
                                self.connected(Service::Lastfm, Account { user, key, rejected: false });
                            }
                            return end(None);
                        }
                        _ => return end(Some(UserError::ServiceUnreachable { service: "Last.fm".into(), detail: "no session".into() })),
                    }
                }
                // Not approved yet, or the network blinked: ask again.
                Err(LfmFail::Code(LFM_NOT_AUTHORIZED, _)) | Err(LfmFail::Net(_)) => {}
                Err(LfmFail::Code(_, detail)) => return end(Some(UserError::ServiceUnreachable { service: "Last.fm".into(), detail })),
            }
            if Instant::now() >= deadline {
                return end(Some(UserError::SignInCancelled));
            }
        }
    }

    /// Give up the sign-in under way.
    pub fn cancel_sign_in(&self) {
        self.0.lock().connecting = None;
    }

    /// Forget the account and what was waiting to go to it: nothing more is sent there.
    pub fn disconnect(&self, service: Service) {
        let mut st = self.0.lock();
        *st.accounts.slot(service) = None;
        st.queue.retain(|w| w.service != service);
        st.health[service as usize] = Health::default();
        if service == Service::Lastfm {
            st.connecting = None;
            st.sign_in_failed = None;
        }
        self.0.save_accounts(&st);
        self.0.save_queue(&st);
    }
}

/// The worker: announces what plays, and sends the waiting listens, oldest first, a batch at
/// a time — trying again later when a batch doesn't get through.
fn work(shared: &Shared) {
    let mut st = shared.lock();
    loop {
        if st.stop {
            return;
        }
        if let Some(song) = st.now.take() {
            let to: Vec<(Service, String)> =
                Service::ALL.iter().filter_map(|&s| st.accounts.get(s).filter(|a| !a.rejected).map(|a| (s, a.key.clone()))).collect();
            drop(st);
            // Best effort: "playing now" that doesn't get through is not sent later.
            let results: Vec<(Service, String, Result<(), Failure>)> = to.into_iter().map(|(s, key)| { let r = shared.api.now_playing(s, &key, &song); (s, key, r) }).collect();
            st = shared.lock();
            for (s, key, r) in results {
                match r {
                    // The service answers again: what waits for it needn't wait any longer.
                    Ok(()) => st.health[s as usize].retry_at = None,
                    Err(Failure::Rejected) => reject(shared, &mut st, s, &key),
                    Err(_) => {}
                }
            }
            continue;
        }
        let now = Instant::now();
        let mut soonest: Option<Instant> = None;
        let mut job = None;
        for i in 0..Service::ALL.len() {
            let service = Service::ALL[(st.turn + i) % Service::ALL.len()];
            let Some(account) = st.accounts.get(service).filter(|a| !a.rejected) else { continue };
            if !st.queue.iter().any(|w| w.service == service) {
                continue;
            }
            let health = st.health[service as usize];
            if let Some(at) = health.retry_at.filter(|at| *at > now) {
                soonest = Some(soonest.map_or(at, |s| s.min(at)));
                continue;
            }
            let n = if health.one_by_one { 1 } else { BATCH };
            let batch: Vec<Waiting> = st.queue.iter().filter(|w| w.service == service).take(n).cloned().collect();
            job = Some((service, account.key.clone(), batch));
            break;
        }
        let Some((service, key, batch)) = job else {
            st = match soonest {
                Some(at) => shared.wake.wait_timeout(st, at - now).unwrap_or_else(|p| p.into_inner()).0,
                None => shared.wake.wait(st).unwrap_or_else(|p| p.into_inner()),
            };
            continue;
        };
        st.turn = (service as usize + 1) % Service::ALL.len();
        drop(st);
        let result = shared.api.submit(service, &key, &batch);
        st = shared.lock();
        // Disconnected, or connected anew, while the request was out: the answer is nobody's.
        if st.accounts.get(service).is_none_or(|a| a.key != key) {
            continue;
        }
        let sent = |st: &mut Inner, ids: &[u64]| {
            st.queue.retain(|w| !ids.contains(&w.id));
            shared.save_queue(st);
        };
        let ids: Vec<u64> = batch.iter().map(|w| w.id).collect();
        match result {
            Ok(()) => {
                sent(&mut st, &ids);
                // (One at a time only for as long as the batch that was refused lasts.)
                let more = st.queue.iter().any(|w| w.service == service);
                st.health[service as usize] = Health { one_by_one: more && st.health[service as usize].one_by_one, ..Health::default() };
            }
            Err(Failure::Later(why)) => {
                let h = &mut st.health[service as usize];
                h.failures += 1;
                h.retry_at = Some(Instant::now() + (shared.api.retry)(h.failures));
                log::info!("{}: {} listen(s) wait: {why}", service.name(), ids.len());
            }
            Err(Failure::Rejected) => reject(shared, &mut st, service, &key),
            Err(Failure::Refused(why)) if ids.len() > 1 => {
                log::info!("{} refused a batch ({why}): sending one at a time", service.name());
                st.health[service as usize].one_by_one = true;
            }
            Err(Failure::Refused(why)) => {
                log::warn!("{} refused a listen ({why}): dropped", service.name());
                st.health[service as usize].one_by_one = false;
                sent(&mut st, &ids);
            }
        }
    }
}

/// The service doesn't accept `key` any more: stop sending until the user connects again.
fn reject(shared: &Shared, st: &mut Inner, service: Service, key: &str) {
    if let Some(a) = st.accounts.slot(service).as_mut().filter(|a| a.key == key && !a.rejected) {
        a.rejected = true;
        shared.save_accounts(st);
    }
}

// --- commands ----------------------------------------------------------------------------------

#[tauri::command]
pub fn scrobble_status(s: State<'_, Scrobbling>) -> Vec<ServiceStatus> {
    s.status()
}

/// Connect a service that takes a token the user pastes (ListenBrainz).
#[tauri::command(async)]
pub fn scrobble_connect_token(s: State<'_, Scrobbling>, service: Service, token: String) -> Result<Vec<ServiceStatus>, UserError> {
    s.connect_token(service, &token)?;
    Ok(s.status())
}

/// Start the sign-in of a service that approves the app in the browser (Last.fm), and open
/// its page there.
#[tauri::command(async)]
pub fn scrobble_sign_in(app: AppHandle, s: State<'_, Scrobbling>, service: Service) -> Result<Vec<ServiceStatus>, UserError> {
    use tauri_plugin_opener::OpenerExt;
    let url = s.sign_in(service)?;
    if let Err(e) = app.opener().open_url(url, None::<&str>) {
        s.cancel_sign_in();
        return Err(UserError::other(e));
    }
    Ok(s.status())
}

#[tauri::command]
pub fn scrobble_cancel(s: State<'_, Scrobbling>) -> Vec<ServiceStatus> {
    s.cancel_sign_in();
    s.status()
}

#[tauri::command]
pub fn scrobble_disconnect(s: State<'_, Scrobbling>, service: Service) -> Vec<ServiceStatus> {
    s.disconnect(service);
    s.status()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Clone, Debug)]
    struct Request {
        method: String,
        /// Path and query.
        path: String,
        /// The header lines, lower-cased.
        head: String,
        body: String,
    }

    /// A stand-in for a service: answers every request with what `answer` says (status,
    /// body), remembers what it was asked, and can be taken off the network (`down`: it
    /// hangs up without answering).
    struct Fake {
        url: String,
        requests: Arc<Mutex<Vec<Request>>>,
        down: Arc<AtomicBool>,
    }

    impl Fake {
        fn start(answer: impl Fn(&Request) -> (u16, String) + Send + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let down = Arc::new(AtomicBool::new(false));
            let (seen, off) = (requests.clone(), down.clone());
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(mut conn) = conn else { continue };
                    if off.load(Ordering::Acquire) {
                        continue; // dropped: the client sees a connection that closed
                    }
                    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut data = Vec::new();
                    let mut buf = [0u8; 8192];
                    let (head_end, length) = loop {
                        let n = conn.read(&mut buf).unwrap_or(0);
                        if n == 0 {
                            break (data.len(), 0);
                        }
                        data.extend_from_slice(&buf[..n]);
                        if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&data[..at]).to_lowercase();
                            let length = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
                            break (at + 4, length);
                        }
                    };
                    while data.len() < head_end + length {
                        let n = conn.read(&mut buf).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        data.extend_from_slice(&buf[..n]);
                    }
                    let head = String::from_utf8_lossy(&data[..head_end.min(data.len())]).into_owned();
                    let mut first = head.lines().next().unwrap_or("").split_whitespace();
                    let request = Request {
                        method: first.next().unwrap_or("").to_string(),
                        path: first.next().unwrap_or("").to_string(),
                        head: head.to_lowercase(),
                        body: String::from_utf8_lossy(&data[head_end.min(data.len())..]).into_owned(),
                    };
                    let (status, body) = answer(&request);
                    seen.lock().unwrap().push(request);
                    let _ = write!(conn, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                }
            });
            Self { url, requests, down }
        }

        fn asked(&self) -> Vec<Request> {
            self.requests.lock().unwrap().clone()
        }

        fn set_down(&self, down: bool) {
            self.down.store(down, Ordering::Release);
        }
    }

    /// The services at `fake`, with retries after a moment instead of minutes.
    fn api(fake: &Fake) -> Api {
        Api {
            listenbrainz: fake.url.clone(),
            lastfm: format!("{}/2.0/", fake.url),
            lastfm_auth: "https://last.fm.test/api/auth/".into(),
            lastfm_app: Some(("appkey".into(), "appsecret".into())),
            retry: |_| Duration::from_millis(40),
            poll: Duration::from_millis(20),
        }
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bitspring-scrobble-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn song(title: &str) -> Song {
        Song { artist: "Miles Davis".into(), title: title.into(), album: Some("Kind of Blue".into()), album_artist: None, track_number: Some(1), duration: 545.6 }
    }

    fn until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "never happened: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn of(s: &Scrobbling, service: Service) -> ServiceStatus {
        s.status().into_iter().find(|x| x.service == service).unwrap()
    }

    /// ListenBrainz as it answers: the token "good" belongs to alex, any other is refused.
    fn listenbrainz(r: &Request) -> (u16, String) {
        let good = r.head.contains("authorization: token good");
        match (r.path.as_str(), good) {
            ("/1/validate-token", true) => (200, r#"{"code":200,"message":"Token valid.","valid":true,"user_name":"alex"}"#.into()),
            ("/1/validate-token", false) => (200, r#"{"code":200,"message":"Token invalid.","valid":false}"#.into()),
            ("/1/submit-listens", true) => (200, r#"{"status":"ok"}"#.into()),
            ("/1/submit-listens", false) => (401, r#"{"code":401,"error":"Invalid authorization token."}"#.into()),
            _ => (404, "{}".into()),
        }
    }

    /// The titles of the listens a ListenBrainz request carried, with their type.
    fn titles(r: &Request) -> (String, Vec<String>) {
        let v: Value = serde_json::from_str(&r.body).unwrap();
        let titles = v["payload"].as_array().unwrap().iter().map(|l| l["track_metadata"]["track_name"].as_str().unwrap().to_string()).collect();
        (v["listen_type"].as_str().unwrap().to_string(), titles)
    }

    fn submitted(fake: &Fake) -> Vec<(String, Vec<String>)> {
        fake.asked().iter().filter(|r| r.path == "/1/submit-listens").map(titles).collect()
    }

    #[test]
    fn nothing_goes_out_until_an_account_is_connected_and_nothing_after_it_is_disconnected() {
        let fake = Fake::start(listenbrainz);
        let d = tmpdir("off");
        let s = Scrobbling::open(Some(d.clone()), api(&fake));
        // No account: a track plays and is listened to, and nobody hears of it.
        s.now_playing(&song("So What"));
        s.listened(&song("So What"), 1_700_000_000);
        std::thread::sleep(Duration::from_millis(150));
        assert!(fake.asked().is_empty());
        assert_eq!(of(&s, Service::Listenbrainz).waiting, 0);
        assert!(!d.join("scrobble-queue.json").exists());

        // A token ListenBrainz doesn't know connects nothing.
        assert_eq!(s.connect_token(Service::Listenbrainz, "wrong"), Err(UserError::TokenRefused { service: "ListenBrainz".into() }));
        assert_eq!(of(&s, Service::Listenbrainz).user, None);

        // The user's own token (pasted with a stray space) does.
        s.connect_token(Service::Listenbrainz, " good\n").unwrap();
        assert_eq!(of(&s, Service::Listenbrainz).user.as_deref(), Some("alex"));
        s.now_playing(&song("Freddie Freeloader"));
        until("playing now is announced", || submitted(&fake).len() == 1);
        s.listened(&song("Freddie Freeloader"), 1_700_000_600);
        until("the listen is sent", || submitted(&fake).len() == 2 && of(&s, Service::Listenbrainz).waiting == 0);
        assert_eq!(submitted(&fake), [("playing_now".to_string(), vec!["Freddie Freeloader".to_string()]), ("single".to_string(), vec!["Freddie Freeloader".to_string()])]);
        let sent: Value = serde_json::from_str(&fake.asked().last().unwrap().body).unwrap();
        let listen = &sent["payload"][0];
        assert_eq!(listen["listened_at"], 1_700_000_600u64);
        assert_eq!(listen["track_metadata"]["artist_name"], "Miles Davis");
        assert_eq!(listen["track_metadata"]["release_name"], "Kind of Blue");
        assert_eq!(listen["track_metadata"]["additional_info"]["duration_ms"], 545_600);
        assert_eq!(listen["track_metadata"]["additional_info"]["media_player"], "BitSpring");
        // "Playing now" carries no time.
        let announced: Value = serde_json::from_str(&fake.asked()[fake.asked().len() - 2].body).unwrap();
        assert!(announced["payload"][0].get("listened_at").is_none());
        // Last.fm was never connected: not a request went its way.
        assert!(fake.asked().iter().all(|r| !r.path.starts_with("/2.0/")));

        // Disconnected: the requests stop, and the account is gone from the disk too.
        s.disconnect(Service::Listenbrainz);
        let before = fake.asked().len();
        s.now_playing(&song("Blue in Green"));
        s.listened(&song("Blue in Green"), 1_700_001_200);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(fake.asked().len(), before);
        assert_eq!(of(&s, Service::Listenbrainz).waiting, 0);
        s.shutdown();
        let again = Scrobbling::open(Some(d.clone()), api(&fake));
        assert_eq!(of(&again, Service::Listenbrainz).user, None);
        again.shutdown();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn listens_wait_on_disk_while_there_is_no_network_and_go_out_later_in_order() {
        let fake = Fake::start(listenbrainz);
        let d = tmpdir("offline");
        let s = Scrobbling::open(Some(d.clone()), api(&fake));
        s.connect_token(Service::Listenbrainz, "good").unwrap();
        // The network goes away; three tracks are listened to.
        fake.set_down(true);
        for (i, title) in ["So What", "Freddie Freeloader", "Blue in Green"].iter().enumerate() {
            s.listened(&song(title), 1_700_000_000 + 600 * i as u64);
        }
        until("the service is found unreachable", || of(&s, Service::Listenbrainz).offline);
        assert_eq!(of(&s, Service::Listenbrainz).waiting, 3);
        assert!(submitted(&fake).is_empty());
        // They are on disk…
        let on_disk: Vec<Value> = hifi_engine::store::load_json(&d.join("scrobble-queue.json")).unwrap();
        assert_eq!(on_disk.len(), 3);
        // …so the app can be closed and opened again, still offline, and lose nothing.
        s.shutdown();
        let s = Scrobbling::open(Some(d.clone()), api(&fake));
        assert_eq!(of(&s, Service::Listenbrainz).user.as_deref(), Some("alex"));
        assert_eq!(of(&s, Service::Listenbrainz).waiting, 3);
        s.listened(&song("All Blues"), 1_700_001_800);
        until("it is tried and fails again", || of(&s, Service::Listenbrainz).offline);
        assert!(submitted(&fake).is_empty());
        // The network is back: everything goes out by itself, oldest first, in one request.
        fake.set_down(false);
        until("the waiting listens are sent", || of(&s, Service::Listenbrainz).waiting == 0);
        assert_eq!(submitted(&fake), [("import".to_string(), ["So What", "Freddie Freeloader", "Blue in Green", "All Blues"].map(String::from).to_vec())]);
        assert!(!of(&s, Service::Listenbrainz).offline);
        let on_disk: Vec<Value> = hifi_engine::store::load_json(&d.join("scrobble-queue.json")).unwrap();
        assert!(on_disk.is_empty());
        s.shutdown();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_token_the_service_stopped_accepting_shows_and_stops_the_sending() {
        // The token works when it is connected and is revoked afterwards.
        let revoked = Arc::new(AtomicBool::new(false));
        let r2 = revoked.clone();
        let fake = Fake::start(move |r| {
            if r2.load(Ordering::Acquire) && r.head.contains("authorization: token good") {
                return (401, r#"{"code":401,"error":"Invalid authorization token."}"#.into());
            }
            listenbrainz(r)
        });
        let d = tmpdir("rejected");
        let s = Scrobbling::open(Some(d.clone()), api(&fake));
        s.connect_token(Service::Listenbrainz, "good").unwrap();
        revoked.store(true, Ordering::Release);
        s.listened(&song("So What"), 1_700_000_000);
        until("the rejection is noticed", || of(&s, Service::Listenbrainz).rejected);
        // Asked once, not again and again; the listen is kept.
        let asked = fake.asked().len();
        s.listened(&song("All Blues"), 1_700_000_600);
        s.now_playing(&song("All Blues"));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(fake.asked().len(), asked);
        assert_eq!(of(&s, Service::Listenbrainz).waiting, 2);
        // It is still so after a restart.
        s.shutdown();
        let s = Scrobbling::open(Some(d.clone()), api(&fake));
        assert!(of(&s, Service::Listenbrainz).rejected);
        // Connecting again (the token is good again) sends what waited.
        revoked.store(false, Ordering::Release);
        s.connect_token(Service::Listenbrainz, "good").unwrap();
        until("the kept listens are sent", || of(&s, Service::Listenbrainz).waiting == 0);
        assert!(!of(&s, Service::Listenbrainz).rejected);
        assert_eq!(submitted(&fake).last().unwrap().1, ["So What", "All Blues"]);
        s.shutdown();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_listen_the_service_refuses_does_not_hold_up_the_others() {
        let fake = Fake::start(|r| {
            if r.path == "/1/submit-listens" && r.body.contains("Unacceptable") {
                return (400, r#"{"code":400,"error":"JSON document is invalid"}"#.into());
            }
            listenbrainz(r)
        });
        let s = Scrobbling::open(None, api(&fake));
        s.connect_token(Service::Listenbrainz, "good").unwrap();
        fake.set_down(true);
        for (i, title) in ["So What", "Unacceptable", "All Blues"].iter().enumerate() {
            s.listened(&song(title), 1_700_000_000 + i as u64);
        }
        until("the service is found unreachable", || of(&s, Service::Listenbrainz).offline);
        fake.set_down(false);
        until("the queue empties", || of(&s, Service::Listenbrainz).waiting == 0);
        let accepted: Vec<String> = fake.asked().iter().filter(|r| r.path == "/1/submit-listens" && !r.body.contains("Unacceptable")).flat_map(|r| titles(r).1).collect();
        assert_eq!(accepted, ["So What", "All Blues"]);
        s.shutdown();
    }

    /// The parameters of a Last.fm request (a form body or a query), decoded.
    fn form(s: &str) -> Vec<(String, String)> {
        let decode = |v: &str| {
            let b = v.replace('+', " ").into_bytes();
            let (mut out, mut i) = (Vec::new(), 0);
            while i < b.len() {
                if b[i] == b'%' && i + 2 < b.len() && let Ok(x) = u8::from_str_radix(&String::from_utf8_lossy(&b[i + 1..i + 3]), 16) {
                    out.push(x);
                    i += 3;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            String::from_utf8(out).unwrap()
        };
        s.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (decode(k), decode(v))).collect()
    }

    /// The request's parameters, having checked that it is signed with the app's secret.
    fn signed(r: &Request) -> std::collections::HashMap<String, String> {
        let all = form(if r.method == "POST" { &r.body } else { r.path.split_once('?').unwrap().1 });
        let unsigned: Vec<(String, String)> = all.iter().filter(|(k, _)| k != "api_sig" && k != "format").cloned().collect();
        let map: std::collections::HashMap<String, String> = all.into_iter().collect();
        assert_eq!(map["api_sig"], lfm_sign(&unsigned, "appsecret"), "signature of {}", map["method"]);
        assert_eq!(map["api_key"], "appkey");
        assert_eq!(map["format"], "json");
        map
    }

    #[test]
    fn the_last_fm_signature_is_the_sorted_parameters_and_the_secret() {
        let params = [("token", "tok"), ("method", "auth.getSession"), ("api_key", "abc")].map(|(k, v)| (k.to_string(), v.to_string()));
        let expected: String = Md5::digest("api_keyabcmethodauth.getSessiontokentokmysecret".as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(lfm_sign(&params, "mysecret"), expected);
        // Values go in as they are, not as they travel (UTF-8, unescaped).
        let params = [("track", "Für Elise & more"), ("artist", "Beethoven")].map(|(k, v)| (k.to_string(), v.to_string()));
        let expected: String = Md5::digest("artistBeethoventrackFür Elise & mores".as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(lfm_sign(&params, "s"), expected);
    }

    #[test]
    fn last_fm_signs_in_in_the_browser_and_then_scrobbles() {
        // Last.fm as it answers: the token isn't approved for the first two asks.
        let asks = Arc::new(Mutex::new(0));
        let fake = Fake::start(move |r| {
            let p = signed(r);
            match p["method"].as_str() {
                "auth.getToken" => (200, r#"{"token":"tok123"}"#.into()),
                "auth.getSession" => {
                    let mut n = asks.lock().unwrap();
                    *n += 1;
                    if *n <= 2 {
                        (403, r#"{"error":14,"message":"Unauthorized Token - This token has not been authorized"}"#.into())
                    } else {
                        (200, r#"{"session":{"name":"alex","key":"sessionkey","subscriber":0}}"#.into())
                    }
                }
                "track.updateNowPlaying" => (200, r#"{"nowplaying":{}}"#.into()),
                "track.scrobble" if p["sk"] == "sessionkey" => (200, r#"{"scrobbles":{"@attr":{"accepted":2,"ignored":0}}}"#.into()),
                "track.scrobble" => (403, r#"{"error":9,"message":"Invalid session key - Please re-authenticate"}"#.into()),
                _ => (400, r#"{"error":3,"message":"Invalid Method"}"#.into()),
            }
        });
        let d = tmpdir("lastfm");
        let s = Scrobbling::open(Some(d.clone()), api(&fake));
        assert!(of(&s, Service::Lastfm).available);
        // Listened to before the account is there: not kept, not sent.
        s.listened(&song("So What"), 1_700_000_000);
        let url = s.sign_in(Service::Lastfm).unwrap();
        assert_eq!(url, "https://last.fm.test/api/auth/?api_key=appkey&token=tok123");
        assert!(of(&s, Service::Lastfm).connecting);
        until("the approval is seen", || of(&s, Service::Lastfm).user.is_some());
        let st = of(&s, Service::Lastfm);
        assert_eq!((st.user.as_deref(), st.connecting, st.waiting), (Some("alex"), false, 0));

        // Offline for two tracks, then both in one request.
        fake.set_down(true);
        s.listened(&Song { album_artist: Some("Various".into()), ..song("Für Elise & more") }, 1_700_000_600);
        s.listened(&song("All Blues"), 1_700_001_200);
        until("the service is found unreachable", || of(&s, Service::Lastfm).offline);
        fake.set_down(false);
        until("the scrobbles are sent", || of(&s, Service::Lastfm).waiting == 0);
        let scrobble = fake.asked().into_iter().rfind(|r| r.body.contains("track.scrobble")).unwrap();
        assert_eq!(scrobble.method, "POST");
        let p = signed(&scrobble);
        assert_eq!(p["sk"], "sessionkey");
        assert_eq!((p["artist[0]"].as_str(), p["track[0]"].as_str(), p["timestamp[0]"].as_str()), ("Miles Davis", "Für Elise & more", "1700000600"));
        assert_eq!((p["album[0]"].as_str(), p["albumArtist[0]"].as_str(), p["duration[0]"].as_str(), p["trackNumber[0]"].as_str()), ("Kind of Blue", "Various", "546", "1"));
        assert_eq!((p["track[1]"].as_str(), p["timestamp[1]"].as_str()), ("All Blues", "1700001200"));
        assert!(!p.contains_key("albumArtist[1]"));

        // Playing now.
        s.now_playing(&song("Flamenco Sketches"));
        until("playing now is announced", || fake.asked().iter().any(|r| r.body.contains("track.updateNowPlaying")));
        let now = signed(&fake.asked().into_iter().rfind(|r| r.body.contains("track.updateNowPlaying")).unwrap());
        assert_eq!((now["track"].as_str(), now["sk"].as_str()), ("Flamenco Sketches", "sessionkey"));

        // The session survives a restart; a session Last.fm no longer knows shows as rejected.
        s.shutdown();
        let mut saved: Value = hifi_engine::store::load_json(&d.join("scrobbling.json")).unwrap();
        assert_eq!(saved["lastfm"]["user"], "alex");
        saved["lastfm"]["key"] = json!("revoked");
        hifi_engine::store::save_json(&d.join("scrobbling.json"), &saved).unwrap();
        let s = Scrobbling::open(Some(d.clone()), api(&fake));
        s.listened(&song("So What"), 1_700_002_000);
        until("the rejection is noticed", || of(&s, Service::Lastfm).rejected);
        assert_eq!(of(&s, Service::Lastfm).waiting, 1);
        s.shutdown();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_last_fm_sign_in_can_be_given_up_and_a_build_without_the_key_has_none() {
        let fake = Fake::start(|r| match signed(r)["method"].as_str() {
            "auth.getToken" => (200, r#"{"token":"tok"}"#.into()),
            _ => (403, r#"{"error":14,"message":"Unauthorized Token"}"#.into()),
        });
        let s = Scrobbling::open(None, api(&fake));
        s.sign_in(Service::Lastfm).unwrap();
        until("it asks whether the user approved", || fake.asked().len() >= 3);
        s.cancel_sign_in();
        assert!(!of(&s, Service::Lastfm).connecting);
        std::thread::sleep(Duration::from_millis(100));
        let asked = fake.asked().len();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(fake.asked().len(), asked, "the asking stops");
        assert_eq!(of(&s, Service::Lastfm).user, None);
        s.shutdown();

        let s = Scrobbling::open(None, Api { lastfm_app: None, ..api(&fake) });
        assert!(!of(&s, Service::Lastfm).available);
        assert!(of(&s, Service::Listenbrainz).available);
        assert!(s.sign_in(Service::Lastfm).is_err());
        assert_eq!(fake.asked().len(), asked);
        s.shutdown();
    }
}
