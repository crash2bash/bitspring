//! A pretend UPnP renderer for tests: answers the description and SOAP calls the way a real
//! one does, fetches what it is told to play (and the next URI ahead, if it takes one), and
//! keeps a playing clock in real time.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Shared {
    /// What plays now, and what follows it (SetNextAVTransportURI).
    pub uri: String,
    pub next: Option<String>,
    /// DIDL metadata by URI.
    pub didl: HashMap<String, String>,
    pub state: &'static str,
    /// Clock: position at `at`, running while playing.
    pos: f64,
    at: Option<Instant>,
    /// Everything fetched, by URI, and the URIs read to the end.
    pub received: HashMap<String, Vec<u8>>,
    fetched: Vec<String>,
    fetching: Vec<String>,
    /// The URIs played, in order.
    pub played: Vec<String>,
    pub requests: usize,
    pub actions: Vec<String>,
}

impl Shared {
    fn now(&self) -> f64 {
        self.pos + self.at.map_or(0.0, |t| t.elapsed().as_secs_f64())
    }
}

pub struct FakeRenderer {
    pub port: u16,
    pub shared: Arc<Mutex<Shared>>,
}

impl FakeRenderer {
    /// `bytes_per_sec`: of the audio, to know when a stream has been played out.
    /// `gapless`: takes SetNextAVTransportURI (like WiiM, Bluesound), or refuses it (Sonos).
    pub fn start(sink: &str, bytes_per_sec: f64, gapless: bool) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let shared = Arc::new(Mutex::new(Shared { state: "NO_MEDIA_PRESENT", ..Default::default() }));
        let sink = sink.to_string();
        let s = shared.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let (s, sink) = (s.clone(), sink.clone());
                std::thread::spawn(move || handle(conn, port, &s, &sink, gapless));
            }
        });
        // The clock: at the end of a stream, on to the next one or stop.
        let s = shared.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(10));
                let mut g = s.lock().unwrap();
                if g.state != "PLAYING" || !g.fetched.contains(&g.uri) {
                    continue;
                }
                let len = g.received.get(&g.uri).map_or(0, |b| b.len()) as f64 / bytes_per_sec;
                if g.now() < len {
                    continue;
                }
                match g.next.take() {
                    Some(next) => {
                        g.uri = next.clone();
                        g.played.push(next);
                        g.pos = 0.0;
                        g.at = Some(Instant::now());
                    }
                    None => {
                        g.pos = len;
                        g.at = None;
                        g.state = "STOPPED";
                    }
                }
            }
        });
        Self { port, shared }
    }

    pub fn location(&self) -> String {
        format!("http://127.0.0.1:{}/desc.xml", self.port)
    }
}

fn handle(mut conn: TcpStream, port: u16, s: &Arc<Mutex<Shared>>, sink: &str, gapless: bool) {
    let mut r = BufReader::new(conn.try_clone().unwrap());
    let mut line = String::new();
    if r.read_line(&mut line).is_err() {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
    let mut headers = HashMap::new();
    loop {
        let mut l = String::new();
        if r.read_line(&mut l).unwrap_or(0) == 0 || l.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let mut body = vec![0; headers.get("content-length").and_then(|n| n.parse().ok()).unwrap_or(0)];
    let _ = r.read_exact(&mut body);
    let body = String::from_utf8_lossy(&body).into_owned();
    let reply = |conn: &mut TcpStream, status: &str, xml: String| {
        let _ = write!(conn, "HTTP/1.1 {status}\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{xml}", xml.len());
    };
    if path == "/desc.xml" {
        let xml = format!(
            r#"<?xml version="1.0"?><root xmlns="urn:schemas-upnp-org:device-1-0"><specVersion><major>1</major><minor>0</minor></specVersion>
            <device><deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType><friendlyName>Test Streamer</friendlyName>
            <manufacturer>Acme</manufacturer><modelName>Streamer 1</modelName><UDN>uuid:fake-{port}</UDN><serviceList>
            <service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType><controlURL>/avt</controlURL></service>
            <service><serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType><controlURL>/rc</controlURL></service>
            <service><serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType><controlURL>/cm</controlURL></service>
            </serviceList></device></root>"#
        );
        return reply(&mut conn, "200 OK", xml);
    }
    let action = headers.get("soapaction").and_then(|a| a.trim_matches('"').split('#').nth(1)).unwrap_or("").to_string();
    let arg = |name: &str| {
        let doc = roxmltree::Document::parse(&body).ok()?;
        doc.descendants().find(|n| n.tag_name().name() == name).map(|n| n.text().unwrap_or("").to_string())
    };
    let mut g = s.lock().unwrap();
    g.actions.push(action.clone());
    let fetch_once = |g: &mut Shared, uri: String| {
        if !g.fetching.contains(&uri) {
            g.fetching.push(uri.clone());
            let s2 = s.clone();
            std::thread::spawn(move || fetch(uri, s2));
        }
    };
    let out = match action.as_str() {
        "GetProtocolInfo" => format!("<Source></Source><Sink>{sink}</Sink>"),
        "SetAVTransportURI" => {
            let uri = arg("CurrentURI").unwrap_or_default();
            g.didl.insert(uri.clone(), arg("CurrentURIMetaData").unwrap_or_default());
            g.uri = uri;
            g.next = None;
            g.state = "STOPPED";
            g.pos = 0.0;
            g.at = None;
            String::new()
        }
        "SetNextAVTransportURI" if gapless => {
            let uri = arg("NextURI").unwrap_or_default();
            g.didl.insert(uri.clone(), arg("NextURIMetaData").unwrap_or_default());
            g.next = Some(uri.clone());
            // Prepared ahead: fetched now, so it joins without a gap.
            fetch_once(&mut g, uri);
            String::new()
        }
        "Play" => {
            if g.state != "PLAYING" {
                if g.state == "STOPPED" {
                    let uri = g.uri.clone();
                    g.played.push(uri.clone());
                    fetch_once(&mut g, uri);
                }
                g.state = "PLAYING";
                g.at = Some(Instant::now());
            }
            String::new()
        }
        "Pause" => {
            g.pos = g.now();
            g.at = None;
            g.state = "PAUSED_PLAYBACK";
            String::new()
        }
        "Stop" => {
            g.pos = 0.0;
            g.at = None;
            g.state = "STOPPED";
            String::new()
        }
        "GetTransportInfo" => format!("<CurrentTransportState>{}</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus>", g.state),
        "GetPositionInfo" => {
            let t = g.now();
            format!(
                "<Track>1</Track><TrackURI>{}</TrackURI><RelTime>{}:{:02}:{:06.3}</RelTime>",
                super::upnp::escape(&g.uri),
                (t / 3600.0) as u32,
                ((t / 60.0) as u32) % 60,
                t % 60.0
            )
        }
        "GetVolume" => "<CurrentVolume>30</CurrentVolume>".into(),
        "SetVolume" => String::new(),
        _ => return reply(&mut conn, "500 Internal Server Error", "<errorCode>401</errorCode><errorDescription>Invalid Action</errorDescription>".into()),
    };
    drop(g);
    let xml = format!(
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:{action}Response xmlns:u="x">{out}</u:{action}Response></s:Body></s:Envelope>"#
    );
    reply(&mut conn, "200 OK", xml);
}

/// Fetch a stream like a renderer: first a short probe, then for real, to the end.
fn fetch(uri: String, s: Arc<Mutex<Shared>>) {
    let u = super::http::Url::parse(&uri).unwrap();
    let get = |probe: bool| {
        let mut c = TcpStream::connect((u.host.as_str(), u.port)).unwrap();
        write!(c, "GET {} HTTP/1.1\r\nHost: x\r\n\r\n", u.path).unwrap();
        let mut r = BufReader::new(c);
        loop {
            let mut l = String::new();
            if r.read_line(&mut l).unwrap_or(0) == 0 || l.trim().is_empty() {
                break;
            }
        }
        s.lock().unwrap().requests += 1;
        if probe {
            let mut b = [0u8; 100];
            let _ = r.read_exact(&mut b);
            return;
        }
        s.lock().unwrap().received.insert(uri.clone(), Vec::new());
        let mut buf = [0u8; 16384];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => s.lock().unwrap().received.entry(uri.clone()).or_default().extend_from_slice(&buf[..n]),
            }
        }
    };
    get(true);
    get(false);
    s.lock().unwrap().fetched.push(uri);
}
