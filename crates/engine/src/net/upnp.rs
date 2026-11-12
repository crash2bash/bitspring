//! UPnP AV: finding media renderers on the network (SSDP), reading what they can play, and
//! controlling them (AVTransport, RenderingControl).

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

use super::encode::Codec;
use super::http::{self, Url};
use super::{Meta, Remote, State, Status};

const AVT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RC: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const CM: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";
const SOAP_TIMEOUT: Duration = Duration::from_secs(4);
/// Polls and the final Stop: a renderer that has gone away mustn't hold anything up long.
const QUICK: Duration = Duration::from_millis(1500);

/// A media renderer: a streamer, network speaker or AV receiver that plays from a URL.
#[derive(Debug, Clone)]
pub struct Renderer {
    /// "upnp:" + the device's UDN: stable across restarts and address changes.
    pub uid: String,
    pub name: String,
    pub manufacturer: String,
    pub model: String,
    pub ip: IpAddr,
    av_transport: String,
    rendering: Option<String>,
    /// What we send it: the codec, the MIME type exactly as the renderer lists it, and the
    /// highest rate and bit depth it takes.
    pub codec: Codec,
    pub mime: Option<String>,
    pub max_rate: u32,
    pub max_bits: u16,
    pub has_volume: bool,
}


impl Renderer {

    pub fn set_uri(&self, url: &str, didl: &str) -> Result<()> {
        soap(&self.av_transport, AVT, "SetAVTransportURI", &[("InstanceID", "0"), ("CurrentURI", url), ("CurrentURIMetaData", didl)], SOAP_TIMEOUT)
            .map(drop)
    }

    /// Ok(false) if the renderer has no SetNextAVTransportURI (or refuses this one).
    pub fn set_next_uri(&self, url: &str, didl: &str) -> Result<bool> {
        match soap(&self.av_transport, AVT, "SetNextAVTransportURI", &[("InstanceID", "0"), ("NextURI", url), ("NextURIMetaData", didl)], SOAP_TIMEOUT) {
            Ok(_) => Ok(true),
            Err(e) if e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some()) => Err(e),
            Err(_) => Ok(false),
        }
    }

    pub fn play(&self) -> Result<()> {
        soap(&self.av_transport, AVT, "Play", &[("InstanceID", "0"), ("Speed", "1")], SOAP_TIMEOUT).map(drop)
    }

    pub fn pause(&self) -> Result<()> {
        soap(&self.av_transport, AVT, "Pause", &[("InstanceID", "0")], SOAP_TIMEOUT).map(drop)
    }

    pub fn stop(&self) -> Result<()> {
        soap(&self.av_transport, AVT, "Stop", &[("InstanceID", "0")], QUICK).map(drop)
    }

    /// Seconds into the current URI, if the renderer says (many only count whole seconds),
    /// and which URI that is.
    pub fn position(&self) -> Result<(Option<f64>, Option<String>)> {
        let body = soap(&self.av_transport, AVT, "GetPositionInfo", &[("InstanceID", "0")], QUICK)?;
        let uri = value(&body, "TrackURI").map(|u| u.trim().to_string()).filter(|u| !u.is_empty());
        Ok((value(&body, "RelTime").and_then(|t| parse_time(&t)), uri))
    }

    pub fn state(&self) -> Result<State> {
        let body = soap(&self.av_transport, AVT, "GetTransportInfo", &[("InstanceID", "0")], QUICK)?;
        Ok(match value(&body, "CurrentTransportState").as_deref() {
            Some("PLAYING") => State::Playing,
            Some("PAUSED_PLAYBACK") | Some("PAUSED_RECORDING") => State::Paused,
            Some("STOPPED") => State::Stopped,
            Some("TRANSITIONING") => State::Transitioning,
            Some("NO_MEDIA_PRESENT") => State::NoMedia,
            _ => State::Other,
        })
    }

    /// Volume, 0…100.
    pub fn volume(&self) -> Result<u32> {
        let rc = self.rendering.as_deref().ok_or_else(|| anyhow!("no volume control"))?;
        let body = soap(rc, RC, "GetVolume", &[("InstanceID", "0"), ("Channel", "Master")], QUICK)?;
        value(&body, "CurrentVolume").and_then(|v| v.trim().parse().ok()).ok_or_else(|| anyhow!("no volume in the answer"))
    }

    pub fn set_volume(&self, volume: u32) -> Result<()> {
        let rc = self.rendering.as_deref().ok_or_else(|| anyhow!("no volume control"))?;
        let v = volume.min(100).to_string();
        soap(rc, RC, "SetVolume", &[("InstanceID", "0"), ("Channel", "Master"), ("DesiredVolume", &v)], SOAP_TIMEOUT).map(drop)
    }
}

/// One NetPlayer's line to a renderer.
pub struct UpnpRemote {
    r: Renderer,
    /// A stream was handed over before: stop it before the next, as most renderers want.
    handed: bool,
}

impl UpnpRemote {
    pub fn new(r: Renderer) -> Self {
        Self { r, handed: false }
    }
}

impl Remote for UpnpRemote {
    fn load(&mut self, url: &str, mime: &str, meta: &Meta, play: bool) -> Result<()> {
        if self.handed {
            let _ = self.r.stop();
        }
        self.handed = true;
        let d = didl(url, mime, &meta.title, meta.artist.as_deref(), meta.album.as_deref(), meta.cover.as_deref());
        self.r.set_uri(url, &d)?;
        if play { self.r.play() } else { Ok(()) }
    }
    fn queue(&mut self, url: &str, mime: &str, meta: &Meta) -> Result<bool> {
        let d = didl(url, mime, &meta.title, meta.artist.as_deref(), meta.album.as_deref(), meta.cover.as_deref());
        self.r.set_next_uri(url, &d)
    }
    fn play(&mut self) -> Result<()> {
        self.r.play()
    }
    fn pause(&mut self) -> Result<()> {
        self.r.pause()
    }
    fn stop(&mut self) -> Result<()> {
        self.r.stop()
    }
    fn status(&mut self) -> Result<Status> {
        let state = self.r.state()?;
        let (position, current) = self.r.position()?;
        Ok(Status { state, position, current })
    }
}

// ---------------------------------------------------------------------------------------------
// SOAP
// ---------------------------------------------------------------------------------------------

fn soap(control: &str, service: &str, action: &str, args: &[(&str, &str)], timeout: Duration) -> Result<String> {
    let mut inner = String::new();
    for (k, v) in args {
        inner += &format!("<{k}>{}</{k}>", escape(v));
    }
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action} xmlns:u=\"{service}\">{inner}</u:{action}></s:Body></s:Envelope>"
    );
    let soap_action = format!("\"{service}#{action}\"");
    let r = http::request(
        control,
        &[("Content-Type", "text/xml; charset=\"utf-8\""), ("SOAPACTION", &soap_action)],
        Some(&body),
        timeout,
    )?;
    if r.status != 200 {
        let code = value(&r.body, "errorCode").unwrap_or_default();
        let text = value(&r.body, "errorDescription").unwrap_or_default();
        bail!("{action} refused (HTTP {}{}{})", r.status, if code.is_empty() { String::new() } else { format!(", UPnP error {code}") }, if text.is_empty() { String::new() } else { format!(": {text}") });
    }
    Ok(r.body)
}

/// Text of the first element named `name` (any namespace).
fn value(xml: &str, name: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    doc.descendants().find(|n| n.tag_name().name() == name).map(|n| n.text().unwrap_or("").to_string())
}

/// "H:MM:SS" or "H:MM:SS.fff" (or "NOT_IMPLEMENTED").
fn parse_time(t: &str) -> Option<f64> {
    let mut secs = 0.0;
    let parts: Vec<&str> = t.trim().split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    for p in parts {
        secs = secs * 60.0 + p.parse::<f64>().ok()?;
    }
    Some(secs)
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out += "&amp;",
            '<' => out += "&lt;",
            '>' => out += "&gt;",
            '"' => out += "&quot;",
            '\'' => out += "&apos;",
            c => out.push(c),
        }
    }
    out
}

/// DIDL-Lite metadata for one stream.
pub fn didl(url: &str, mime: &str, title: &str, artist: Option<&str>, album: Option<&str>, cover: Option<&str>) -> String {
    let flags = "DLNA.ORG_OP=00;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";
    let mut item = format!("<dc:title>{}</dc:title>", escape(title));
    if let Some(a) = artist {
        item += &format!("<upnp:artist>{0}</upnp:artist><dc:creator>{0}</dc:creator>", escape(a));
    }
    if let Some(a) = album {
        item += &format!("<upnp:album>{}</upnp:album>", escape(a));
    }
    if let Some(c) = cover {
        item += &format!("<upnp:albumArtURI>{}</upnp:albumArtURI>", escape(c));
    }
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
         <item id=\"1\" parentID=\"0\" restricted=\"1\">{item}<upnp:class>object.item.audioItem.musicTrack</upnp:class>\
         <res protocolInfo=\"http-get:*:{}:{flags}\">{}</res></item></DIDL-Lite>",
        escape(mime),
        escape(url)
    )
}

// ---------------------------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------------------------

const SSDP: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(239, 255, 255, 250)), 1900);

/// Send M-SEARCH for media renderers and collect the description URLs that answer.
fn search(wait: Duration) -> Result<HashSet<String>> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    let _ = sock.set_multicast_ttl_v4(2);
    for st in ["urn:schemas-upnp-org:device:MediaRenderer:1", "urn:schemas-upnp-org:service:AVTransport:1"] {
        let msg = format!("M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {st}\r\nUSER-AGENT: BitSpring UPnP/1.0\r\n\r\n");
        // Twice: UDP gets lost.
        for _ in 0..2 {
            let _ = sock.send_to(msg.as_bytes(), SSDP);
        }
    }
    let mut found = HashSet::new();
    let end = Instant::now() + wait;
    let mut buf = [0u8; 4096];
    while Instant::now() < end {
        if let Ok((n, _)) = sock.recv_from(&mut buf) {
            let text = String::from_utf8_lossy(&buf[..n]);
            for line in text.lines() {
                if let Some((k, v)) = line.split_once(':')
                    && k.trim().eq_ignore_ascii_case("location")
                {
                    found.insert(v.trim().to_string());
                }
            }
        }
    }
    Ok(found)
}

/// Read a device description and, for a media renderer in it, what it can play.
fn describe(location: &str) -> Result<Renderer> {
    // Only devices on this network: an answer to our search names a local address, and
    // everything in its description must stay on that device.
    let loc = Url::parse(location)?;
    let device_ip: IpAddr = loc.host.parse().map_err(|_| anyhow!("renderer address {} isn't an IP", loc.host))?;
    if !is_local(device_ip) {
        bail!("{device_ip} isn't on the local network");
    }
    let r = http::request(location, &[], None, SOAP_TIMEOUT)?;
    if r.status != 200 {
        bail!("description: HTTP {}", r.status);
    }
    let doc = roxmltree::Document::parse(&r.body)?;
    let base_url = doc.descendants().find(|n| n.tag_name().name() == "URLBase").and_then(|n| n.text()).map(str::trim).unwrap_or(location);
    let base = Url::parse(base_url).ok().filter(|b| b.host == loc.host).unwrap_or(loc);
    let child_text = |n: roxmltree::Node, name: &str| {
        n.children().find(|c| c.tag_name().name() == name).and_then(|c| c.text()).map(|t| t.trim().to_string())
    };
    let devices: Vec<_> = doc.descendants().filter(|n| n.tag_name().name() == "device").collect();
    let renderer = devices
        .iter()
        .find(|d| child_text(**d, "deviceType").is_some_and(|t| t.contains("MediaRenderer")))
        .or_else(|| devices.first())
        .ok_or_else(|| anyhow!("no device in the description"))?;
    let root = devices.first().copied().unwrap_or(*renderer);
    let mut services = HashMap::new();
    for s in renderer.descendants().filter(|n| n.tag_name().name() == "service") {
        if let (Some(t), Some(c)) = (child_text(s, "serviceType"), child_text(s, "controlURL")) {
            let kind = t.rsplit(':').nth(1).unwrap_or("").to_string();
            let url = base.join(&c);
            if Url::parse(&url).is_ok_and(|u| u.host == base.host) {
                services.entry(kind).or_insert(url);
            }
        }
    }
    let av_transport = services.get("AVTransport").cloned().ok_or_else(|| anyhow!("not a media renderer"))?;
    let udn = child_text(*renderer, "UDN").or_else(|| child_text(root, "UDN")).unwrap_or_else(|| location.to_string());
    let manufacturer = child_text(*renderer, "manufacturer").or_else(|| child_text(root, "manufacturer")).unwrap_or_default();
    let model = child_text(root, "modelName").or_else(|| child_text(*renderer, "modelName")).unwrap_or_default();
    // Sonos names its rooms; the friendly name is an address and a model.
    let name = child_text(root, "roomName")
        .or_else(|| child_text(*renderer, "friendlyName"))
        .or_else(|| child_text(root, "friendlyName"))
        .unwrap_or_else(|| model.clone());
    let ip = device_ip;

    let sink: Vec<String> = services
        .get("ConnectionManager")
        .and_then(|cm| soap(cm, CM, "GetProtocolInfo", &[], SOAP_TIMEOUT).ok())
        .and_then(|b| value(&b, "Sink"))
        .map(|s| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    let (codec, mime, max_rate, max_bits) = capabilities(&sink, &manufacturer, &model);
    Ok(Renderer {
        uid: format!("upnp:{}", udn.trim_start_matches("uuid:")),
        name,
        manufacturer,
        model,
        ip,
        av_transport,
        has_volume: services.contains_key("RenderingControl"),
        rendering: services.get("RenderingControl").cloned(),
        codec,
        mime,
        max_rate,
        max_bits,
    })
}

/// A private, link-local or loopback address: on this network.
pub fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unicast_link_local() || v6.is_unique_local(),
    }
}

/// From the renderer's sink protocols: what to send it.
fn capabilities(sink: &[String], manufacturer: &str, model: &str) -> (Codec, Option<String>, u32, u16) {
    let mimes: Vec<&str> = sink.iter().filter_map(|p| p.split(':').nth(2)).collect();
    let find = |want: &[&str]| mimes.iter().find(|m| want.iter().any(|w| m.eq_ignore_ascii_case(w))).map(|m| m.to_string());
    let l16: Vec<&str> = mimes.iter().copied().filter(|m| m.to_ascii_lowercase().starts_with("audio/l16")).collect();
    let (codec, mime) = if let Some(m) = find(&["audio/flac", "audio/x-flac"]) {
        (Codec::Flac, Some(m))
    } else if let Some(m) = find(&["audio/wav", "audio/x-wav", "audio/wave"]) {
        (Codec::Wav, Some(m))
    } else if !l16.is_empty() {
        (Codec::L16, None)
    } else {
        // Says nothing useful: FLAC is what nearly every renderer plays.
        (Codec::Flac, None)
    };
    let sonos = manufacturer.to_ascii_lowercase().contains("sonos") || model.to_ascii_lowercase().contains("sonos");
    let listed_rates: Vec<u32> = l16
        .iter()
        .filter_map(|m| m.split(';').find_map(|p| p.trim().strip_prefix("rate=")).and_then(|r| r.parse().ok()))
        .collect();
    let max_rate = if sonos {
        48_000
    } else if codec == Codec::L16 {
        listed_rates.iter().copied().max().unwrap_or(48_000)
    } else {
        192_000
    };
    let max_bits = if codec == Codec::L16 { 16 } else { 24 };
    (codec, mime, max_rate, max_bits)
}

// ---------------------------------------------------------------------------------------------
// The renderers on the network, kept current in the background
// ---------------------------------------------------------------------------------------------

struct Known {
    renderer: Renderer,
    location: String,
    seen: Instant,
}

#[derive(Default)]
struct Registry {
    known: Mutex<HashMap<String, Known>>,
    /// Bumped by `refresh()` so the background loop searches right away.
    wake: Mutex<bool>,
    cv: std::sync::Condvar,
}

static REG: OnceLock<Arc<Registry>> = OnceLock::new();

/// The registry, starting the search on first use.
fn registry() -> &'static Arc<Registry> {
    REG.get_or_init(|| {
        let reg = Arc::new(Registry::default());
        let r = reg.clone();
        let _ = std::thread::Builder::new().name("hifi-ssdp".into()).spawn(move || discovery_loop(r));
        reg
    })
}

fn discovery_loop(reg: Arc<Registry>) {
    let mut round = 0u32;
    loop {
        if let Ok(locations) = search(Duration::from_secs(3)) {
            let known: HashMap<String, String> = reg.known.lock().unwrap().iter().map(|(k, v)| (v.location.clone(), k.clone())).collect();
            let handles: Vec<_> = locations
                .into_iter()
                .map(|loc| {
                    let seen_before = known.get(&loc).cloned();
                    std::thread::spawn(move || { let d = describe(&loc).ok(); (loc, seen_before, d) })
                })
                .collect();
            let now = Instant::now();
            for h in handles {
                if let Ok((location, before, described)) = h.join() {
                    let mut k = reg.known.lock().unwrap();
                    match described {
                        Some(renderer) => {
                            k.insert(renderer.uid.clone(), Known { renderer, location, seen: now });
                        }
                        // Answered the search but not the description this time: keep it.
                        None => {
                            if let Some(e) = before.and_then(|uid| k.get_mut(&uid)) {
                                e.seen = now;
                            }
                        }
                    }
                }
            }
            // Gone for a few rounds: no longer listed (but still found by uid, so playback on
            // a player that has stopped answering searches carries on).
        }
        round += 1;
        // Often at first (devices wake up slowly), then once a minute or when asked.
        let pause = if round < 4 { Duration::from_secs(10) } else { Duration::from_secs(60) };
        let mut wake = reg.wake.lock().unwrap();
        if !*wake {
            wake = reg.cv.wait_timeout(wake, pause).unwrap().0;
        }
        *wake = false;
    }
}

/// Start looking for renderers (in the background; returns at once).
pub fn start_discovery() {
    registry();
}

/// Look again now (the user opened the output list) — unless a search ran moments ago.
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

/// Unseen for this long: not listed any more.
const GONE: Duration = Duration::from_secs(150);

/// The renderers found so far, by name.
pub fn renderers() -> Vec<Renderer> {
    // Not searching yet (nobody has asked): none, and no search started just for this.
    let Some(reg) = REG.get() else { return Vec::new() };
    let mut v: Vec<Renderer> =
        reg.known.lock().unwrap().values().filter(|k| k.seen.elapsed() < GONE).map(|k| k.renderer.clone()).collect();
    v.sort_by_key(|r| r.name.to_lowercase());
    v
}

/// One renderer by uid: from what discovery found, or, for a uid used before, after a fresh
/// search (the app may ask before discovery has got round to it).
pub fn find(uid: &str) -> Option<Renderer> {
    if let Some(k) = registry().known.lock().unwrap().get(uid) {
        return Some(k.renderer.clone());
    }
    refresh();
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        std::thread::sleep(Duration::from_millis(250));
        if let Some(r) = renderers().into_iter().find(|r| r.uid == uid) {
            return Some(r);
        }
    }
    None
}

/// Read one renderer straight from its description URL (tests, and a renderer the user
/// types in by address).
pub fn describe_at(location: &str) -> Result<Renderer> {
    let r = describe(location)?;
    let reg = registry();
    reg.known.lock().unwrap().insert(r.uid.clone(), Known { renderer: r.clone(), location: location.to_string(), seen: Instant::now() });
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_parse() {
        assert_eq!(parse_time("0:01:02"), Some(62.0));
        assert_eq!(parse_time("1:00:00.500"), Some(3600.5));
        assert_eq!(parse_time("NOT_IMPLEMENTED"), None);
    }

    #[test]
    fn flac_is_preferred_then_wav_then_l16() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let (c, m, r, b) = capabilities(&s(&["http-get:*:audio/mpeg:*", "http-get:*:audio/x-flac:*", "http-get:*:audio/wav:*"]), "WiiM", "Pro");
        assert_eq!((c, m.as_deref(), r, b), (Codec::Flac, Some("audio/x-flac"), 192_000, 24));
        let (c, _, r, _) = capabilities(&s(&["http-get:*:audio/flac:*"]), "Sonos, Inc.", "Sonos One");
        assert_eq!((c, r), (Codec::Flac, 48_000));
        let (c, m, r, b) = capabilities(&s(&["http-get:*:audio/L16;rate=44100;channels=2:*", "http-get:*:audio/L16;rate=48000;channels=2:*"]), "", "");
        assert_eq!((c, m, r, b), (Codec::L16, None, 48_000, 16));
    }

    #[test]
    fn didl_is_escaped() {
        let d = didl("http://h/1.flac?a=1&b=2", "audio/flac", "Rock & Roll", Some("AC/DC"), None, None);
        assert!(d.contains("Rock &amp; Roll"));
        assert!(d.contains("1.flac?a=1&amp;b=2"));
        assert!(roxmltree::Document::parse(&d).is_ok());
    }
}
