//! Google Cast (Chromecast): built into Google and Nest speakers, Chromecast dongles, Android
//! TV and Google TV (NVIDIA SHIELD, Sony, TCL…), and many soundbars and receivers.
//!
//! Found over mDNS (`_googlecast._tcp`). Controlled over TLS on port 8009 with Cast's own
//! messages: a length-prefixed protobuf envelope around JSON. We launch Google's Default
//! Media Receiver and tell it to play our stream, the same live FLAC stream UPnP players get.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use serde_json::{Value, json};

use super::{Meta, Remote, State, Status};

/// Google's Default Media Receiver: plays a URL, on every Cast device.
const APP_ID: &str = "CC1AD845";
const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";
const SENDER: &str = "sender-0";
const RECEIVER: &str = "receiver-0";

/// FLAC through the Default Media Receiver: up to 96 kHz / 24-bit, by Google's spec.
pub const MAX_RATE: u32 = 96_000;

#[derive(Debug, Clone)]
pub struct Device {
    /// "cast:" + the device id.
    pub uid: String,
    pub name: String,
    pub model: String,
    pub ip: IpAddr,
    pub port: u16,
}

impl Device {
    /// Volume, 0…100.
    pub fn volume(&self) -> Result<u32> {
        let mut c = Conn::open(self)?;
        let reply = c.request(RECEIVER, NS_RECEIVER, json!({"type": "GET_STATUS"}), Duration::from_secs(3))?;
        let level = reply.payload["status"]["volume"]["level"].as_f64().ok_or_else(|| anyhow!("no volume in the answer"))?;
        Ok((level * 100.0).round() as u32)
    }

    pub fn set_volume(&self, volume: u32) -> Result<()> {
        let mut c = Conn::open(self)?;
        let level = volume.min(100) as f64 / 100.0;
        c.request(RECEIVER, NS_RECEIVER, json!({"type": "SET_VOLUME", "volume": {"level": level}}), Duration::from_secs(3))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// The connection
// ---------------------------------------------------------------------------------------------

/// Cast devices present a self-signed certificate. What we send is a URL on the local
/// network and play/pause, so there is nothing to protect by checking it.
#[derive(Debug)]
struct AnyCert(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AnyCert {
    fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn tls_config() -> Result<Arc<ClientConfig>> {
    static CONFIG: OnceLock<std::result::Result<Arc<ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = ClientConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .map_err(|e| e.to_string())?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AnyCert(provider)))
                .with_no_client_auth();
            Ok(Arc::new(config))
        })
        .clone()
        .map_err(|e| anyhow!(e))
}

struct Msg {
    source: String,
    namespace: String,
    payload: Value,
}

struct Conn {
    tls: StreamOwned<ClientConnection, TcpStream>,
    inbox: Vec<u8>,
    next_id: u64,
    last_ping: Instant,
}

impl Conn {
    fn open(dev: &Device) -> Result<Self> {
        let addr = SocketAddr::new(dev.ip, dev.port);
        let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(4)).with_context(|| format!("can't reach {}", dev.name))?;
        tcp.set_read_timeout(Some(Duration::from_millis(100)))?;
        tcp.set_write_timeout(Some(Duration::from_secs(4)))?;
        let _ = tcp.set_nodelay(true);
        let conn = ClientConnection::new(tls_config()?, ServerName::IpAddress(dev.ip.into()))?;
        let mut c = Self { tls: StreamOwned::new(conn, tcp), inbox: Vec::new(), next_id: 1, last_ping: Instant::now() };
        c.send(RECEIVER, NS_CONNECTION, &json!({"type": "CONNECT"}))?;
        Ok(c)
    }

    fn send(&mut self, dest: &str, namespace: &str, payload: &Value) -> Result<()> {
        let body = encode(SENDER, dest, namespace, &payload.to_string());
        let mut frame = (body.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(&body);
        // A write may meet the read timeout while TLS shuffles records: retry those.
        let mut written = 0;
        let deadline = Instant::now() + Duration::from_secs(4);
        while written < frame.len() {
            match self.tls.write(&frame[written..]) {
                Ok(n) => written += n,
                Err(e) if is_timeout(&e) && Instant::now() < deadline => continue,
                Err(e) => return Err(e.into()),
            }
        }
        self.tls.flush()?;
        Ok(())
    }

    /// The next message, or None if none came within `wait`. Answers the device's PINGs.
    fn recv(&mut self, wait: Duration) -> Result<Option<Msg>> {
        let end = Instant::now() + wait;
        loop {
            if self.last_ping.elapsed() > Duration::from_secs(5) {
                self.last_ping = Instant::now();
                self.send(RECEIVER, NS_HEARTBEAT, &json!({"type": "PING"}))?;
            }
            if self.inbox.len() >= 4 {
                let len = u32::from_be_bytes([self.inbox[0], self.inbox[1], self.inbox[2], self.inbox[3]]) as usize;
                if len > 1 << 20 {
                    bail!("Cast message too large");
                }
                if self.inbox.len() >= 4 + len {
                    let body: Vec<u8> = self.inbox.drain(..4 + len).skip(4).collect();
                    let Some((source, namespace, text)) = decode(&body) else { continue };
                    let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    if namespace == NS_HEARTBEAT && payload["type"] == "PING" {
                        self.send(&source, NS_HEARTBEAT, &json!({"type": "PONG"}))?;
                        continue;
                    }
                    return Ok(Some(Msg { source, namespace, payload }));
                }
            }
            if Instant::now() >= end {
                return Ok(None);
            }
            let mut buf = [0u8; 16 << 10];
            match self.tls.read(&mut buf) {
                Ok(0) => bail!("the Cast device closed the connection"),
                Ok(n) => self.inbox.extend_from_slice(&buf[..n]),
                Err(e) if is_timeout(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Send `payload` with a fresh requestId and wait for the answer that carries it.
    fn request(&mut self, dest: &str, namespace: &str, mut payload: Value, wait: Duration) -> Result<Msg> {
        let id = self.next_id;
        self.next_id += 1;
        payload["requestId"] = json!(id);
        self.send(dest, namespace, &payload)?;
        let end = Instant::now() + wait;
        while Instant::now() < end {
            if let Some(m) = self.recv(end.saturating_duration_since(Instant::now()))?
                && m.payload["requestId"] == json!(id)
            {
                return Ok(m);
            }
        }
        bail!("no answer to {}", payload["type"])
    }
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

// CastMessage, the protobuf envelope: version (1), source (2), destination (3),
// namespace (4), payload type (5, 0 = text), text payload (6).
fn encode(source: &str, dest: &str, namespace: &str, text: &str) -> Vec<u8> {
    fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }
    let mut out = vec![0x08, 0x00];
    for (field, s) in [(2u8, source), (3, dest), (4, namespace)] {
        out.push(field << 3 | 2);
        varint(&mut out, s.len() as u64);
        out.extend_from_slice(s.as_bytes());
    }
    out.extend_from_slice(&[0x28, 0x00, 0x32]);
    varint(&mut out, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
    out
}

/// (source, namespace, text payload) of a CastMessage.
fn decode(b: &[u8]) -> Option<(String, String, String)> {
    let mut at = 0;
    let varint = |at: &mut usize| -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *b.get(*at)?;
            *at += 1;
            v |= ((byte & 0x7F) as u64) << shift;
            if byte & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    };
    let (mut source, mut namespace, mut text) = (String::new(), String::new(), String::new());
    while at < b.len() {
        let key = varint(&mut at)?;
        match key & 7 {
            0 => {
                varint(&mut at)?;
            }
            2 => {
                let len = varint(&mut at)? as usize;
                let s = String::from_utf8_lossy(b.get(at..at + len)?).into_owned();
                at += len;
                match key >> 3 {
                    2 => source = s,
                    4 => namespace = s,
                    6 => text = s,
                    _ => {}
                }
            }
            _ => return None,
        }
    }
    Some((source, namespace, text))
}

// ---------------------------------------------------------------------------------------------
// Playing
// ---------------------------------------------------------------------------------------------

/// One NetPlayer's line to a Cast device.
pub struct CastRemote {
    dev: Device,
    conn: Option<Conn>,
    /// The Default Media Receiver's transport id, once it runs.
    transport: Option<String>,
    media_session: Option<i64>,
    last: Status,
}

impl CastRemote {
    pub fn new(dev: Device) -> Self {
        Self { dev, conn: None, transport: None, media_session: None, last: Status { state: State::NoMedia, position: None } }
    }

    fn conn(&mut self) -> Result<&mut Conn> {
        if self.conn.is_none() {
            self.conn = Some(Conn::open(&self.dev)?);
            self.transport = None;
        }
        Ok(self.conn.as_mut().expect("just opened"))
    }

    /// Launch (or find running) the Default Media Receiver and connect to it.
    fn app(&mut self) -> Result<String> {
        if let Some(t) = &self.transport {
            return Ok(t.clone());
        }
        let c = self.conn()?;
        // Launching takes a few seconds on a TV that has to start the receiver app.
        let mut reply = c.request(RECEIVER, NS_RECEIVER, json!({"type": "LAUNCH", "appId": APP_ID}), Duration::from_secs(20))?;
        if reply.payload["type"] == "LAUNCH_ERROR" {
            bail!("the device didn't start its media player ({})", reply.payload["reason"]);
        }
        // An answer without the app yet (still starting): wait for a status that has it.
        let end = Instant::now() + Duration::from_secs(20);
        let transport = loop {
            if let Some(t) = transport_of(&reply.payload) {
                break t;
            }
            if Instant::now() > end {
                bail!("the device's media player didn't start");
            }
            match c.recv(Duration::from_secs(1))? {
                Some(m) if m.namespace == NS_RECEIVER => reply = m,
                _ => {}
            }
        };
        c.send(&transport, NS_CONNECTION, &json!({"type": "CONNECT"}))?;
        self.transport = Some(transport.clone());
        Ok(transport)
    }

    fn media(&mut self, kind: &str) -> Result<()> {
        let (Some(t), Some(id)) = (self.transport.clone(), self.media_session) else { return Ok(()) };
        let c = self.conn()?;
        c.request(&t, NS_MEDIA, json!({"type": kind, "mediaSessionId": id}), Duration::from_secs(4)).map(drop)
    }

    /// Take in a status message, if it is one.
    fn note(&mut self, m: &Msg) {
        match (m.namespace.as_str(), m.payload["type"].as_str()) {
            (NS_MEDIA, Some("MEDIA_STATUS")) => {
                let s = m.payload["status"].as_array().and_then(|a| a.first());
                self.last = match s {
                    None => Status { state: State::NoMedia, position: None },
                    Some(s) => {
                        if let Some(id) = s["mediaSessionId"].as_i64() {
                            self.media_session = Some(id);
                        }
                        let state = match s["playerState"].as_str() {
                            Some("PLAYING") => State::Playing,
                            Some("PAUSED") => State::Paused,
                            Some("BUFFERING") | Some("LOADING") => State::Transitioning,
                            Some("IDLE") => State::Stopped,
                            _ => State::Other,
                        };
                        Status { state, position: s["currentTime"].as_f64() }
                    }
                };
            }
            (NS_RECEIVER, Some("RECEIVER_STATUS")) if transport_of(&m.payload).is_none() => {
                // Our receiver app was closed (another app took over, or the TV's remote).
                self.transport = None;
                self.media_session = None;
                self.last = Status { state: State::NoMedia, position: None };
            }
            (NS_CONNECTION, Some("CLOSE")) if Some(&m.source) == self.transport.as_ref() => {
                self.transport = None;
                self.media_session = None;
                self.last = Status { state: State::Stopped, position: None };
            }
            _ => {}
        }
    }

    /// Any errors on the line drop it: the next call reconnects.
    fn guard<T>(&mut self, r: Result<T>) -> Result<T> {
        if r.is_err() {
            self.conn = None;
            self.transport = None;
        }
        r
    }
}

fn transport_of(receiver_status: &Value) -> Option<String> {
    receiver_status["status"]["applications"]
        .as_array()?
        .iter()
        .find(|a| a["appId"] == APP_ID)
        .and_then(|a| a["transportId"].as_str().map(str::to_string))
}

impl Remote for CastRemote {
    fn load(&mut self, url: &str, mime: &str, meta: &Meta, play: bool) -> Result<()> {
        let r = (|| {
            let t = self.app()?;
            let mut metadata = json!({"metadataType": 3, "title": meta.title});
            if let Some(a) = &meta.artist {
                metadata["artist"] = json!(a);
            }
            if let Some(a) = &meta.album {
                metadata["albumName"] = json!(a);
            }
            if let Some(c) = &meta.cover {
                metadata["images"] = json!([{"url": c}]);
            }
            let load = json!({
                "type": "LOAD",
                "autoplay": play,
                "currentTime": 0,
                "media": {"contentId": url, "contentUrl": url, "contentType": mime, "streamType": "LIVE", "metadata": metadata},
            });
            let c = self.conn()?;
            let reply = c.request(&t, NS_MEDIA, load, Duration::from_secs(15))?;
            match reply.payload["type"].as_str() {
                Some("MEDIA_STATUS") => {
                    self.note(&reply);
                    Ok(())
                }
                other => bail!("{} ({})", other.unwrap_or("no answer"), reply.payload["reason"].as_str().unwrap_or("")),
            }
        })();
        self.guard(r)
    }

    fn play(&mut self) -> Result<()> {
        let r = self.media("PLAY");
        self.guard(r)
    }

    fn pause(&mut self) -> Result<()> {
        let r = self.media("PAUSE");
        self.guard(r)
    }

    fn stop(&mut self) -> Result<()> {
        let r = self.media("STOP");
        self.media_session = None;
        self.guard(r)
    }

    fn status(&mut self) -> Result<Status> {
        let r = (|| {
            let Some(t) = self.transport.clone() else {
                // Nothing loaded through us: just keep the line alive.
                if let Some(c) = self.conn.as_mut() {
                    while let Some(m) = c.recv(Duration::ZERO)? {
                        let _ = m;
                    }
                }
                return Ok(self.last);
            };
            let mut msg = json!({"type": "GET_STATUS"});
            if let Some(id) = self.media_session {
                msg["mediaSessionId"] = json!(id);
            }
            let c = self.conn()?;
            let reply = c.request(&t, NS_MEDIA, msg, Duration::from_secs(2))?;
            self.note(&reply);
            // And whatever else came in meanwhile (receiver status, closes).
            loop {
                let c = self.conn()?;
                match c.recv(Duration::ZERO)? {
                    Some(m) => self.note(&m),
                    None => break,
                }
            }
            Ok(self.last)
        })();
        self.guard(r)
    }
}

// ---------------------------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Registry {
    known: Mutex<HashMap<String, (Device, Instant)>>,
    wake: Mutex<bool>,
    cv: Condvar,
}

static REG: OnceLock<Arc<Registry>> = OnceLock::new();

fn registry() -> &'static Arc<Registry> {
    REG.get_or_init(|| {
        let reg = Arc::new(Registry::default());
        let r = reg.clone();
        let _ = std::thread::Builder::new().name("hifi-mdns".into()).spawn(move || discovery_loop(r));
        reg
    })
}

fn discovery_loop(reg: Arc<Registry>) {
    let mut round = 0u32;
    loop {
        if let Ok(found) = super::mdns::browse("_googlecast._tcp.local", Duration::from_secs(3)) {
            let now = Instant::now();
            let mut k = reg.known.lock().unwrap();
            for i in found {
                let Some(ip) = i.ip.filter(|ip| super::upnp::is_local(*ip)) else { continue };
                let id = i.txt.get("id").cloned().unwrap_or_else(|| i.name.clone());
                let name = i.txt.get("fn").cloned().unwrap_or_else(|| i.name.split("._googlecast").next().unwrap_or("Chromecast").to_string());
                let model = i.txt.get("md").cloned().unwrap_or_else(|| "Chromecast".into());
                let dev = Device { uid: format!("cast:{id}"), name, model, ip, port: i.port };
                k.insert(dev.uid.clone(), (dev, now));
            }
        }
        round += 1;
        let pause = if round < 4 { Duration::from_secs(10) } else { Duration::from_secs(60) };
        let mut wake = reg.wake.lock().unwrap();
        if !*wake {
            wake = reg.cv.wait_timeout(wake, pause).unwrap().0;
        }
        *wake = false;
    }
}

pub fn start_discovery() {
    registry();
}

pub fn refresh() {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    if last.is_some_and(|t| t.elapsed() < Duration::from_secs(10)) {
        return;
    }
    *last = Some(Instant::now());
    let reg = registry();
    *reg.wake.lock().unwrap() = true;
    reg.cv.notify_all();
}

/// The Cast devices seen lately.
pub fn devices() -> Vec<Device> {
    let Some(reg) = REG.get() else { return Vec::new() };
    let mut v: Vec<Device> =
        reg.known.lock().unwrap().values().filter(|(_, seen)| seen.elapsed() < Duration::from_secs(150)).map(|(d, _)| d.clone()).collect();
    v.sort_by_key(|d| d.name.to_lowercase());
    v
}

/// A device by uid: any seen before, or after a fresh search.
pub fn find(uid: &str) -> Option<Device> {
    if let Some((d, _)) = registry().known.lock().unwrap().get(uid) {
        return Some(d.clone());
    }
    refresh();
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        std::thread::sleep(Duration::from_millis(250));
        if let Some((d, _)) = registry().known.lock().unwrap().get(uid) {
            return Some(d.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let b = encode("sender-0", "receiver-0", NS_RECEIVER, r#"{"type":"GET_STATUS","requestId":1}"#);
        let (source, ns, text) = decode(&b).unwrap();
        assert_eq!((source.as_str(), ns.as_str()), ("sender-0", NS_RECEIVER));
        assert_eq!(text, r#"{"type":"GET_STATUS","requestId":1}"#);
    }

    #[test]
    fn the_receiver_app_is_found_in_a_status() {
        let s = json!({"type": "RECEIVER_STATUS", "status": {"applications": [
            {"appId": "E8C28D3C", "transportId": "web-1"},
            {"appId": APP_ID, "transportId": "web-7", "sessionId": "abc"}
        ]}});
        assert_eq!(transport_of(&s).as_deref(), Some("web-7"));
        assert_eq!(transport_of(&json!({"status": {}})), None);
    }
}
