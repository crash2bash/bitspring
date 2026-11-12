//! A pretend UPnP renderer for tests: answers the description and SOAP calls the way a real
//! one does, fetches the stream it is told to play, and keeps a playing clock in real time.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Shared {
    pub uri: String,
    pub didl: String,
    pub state: &'static str,
    /// Clock: position at `at`, running while playing.
    pos: f64,
    at: Option<Instant>,
    /// Everything fetched from the current URI.
    pub received: Vec<u8>,
    pub requests: usize,
    pub actions: Vec<String>,
    /// Seconds of audio in what was received (set by the test's decoding).
    pub length: Option<f64>,
    /// The current fetch (a new URI starts a new one) and whether it has read to the end.
    fetch: u64,
    fetched: bool,
    started_fetch: bool,
}

impl Shared {
    fn now(&self) -> f64 {
        self.pos + self.at.map_or(0.0, |t| t.elapsed().as_secs_f64())
    }
}

pub struct FakeRenderer {
    pub port: u16,
    pub shared: Arc<Mutex<Shared>>,
    /// Bytes per second of audio, to know when the stream has been played out.
    pub bytes_per_sec: Arc<Mutex<f64>>,
}

impl FakeRenderer {
    pub fn start(sink: &str) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let shared = Arc::new(Mutex::new(Shared { state: "NO_MEDIA_PRESENT", ..Default::default() }));
        let bps = Arc::new(Mutex::new(0.0));
        let sink = sink.to_string();
        let (s, b) = (shared.clone(), bps.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let (s, b, sink) = (s.clone(), b.clone(), sink.clone());
                std::thread::spawn(move || handle(conn, port, &s, &b, &sink));
            }
        });
        // The clock stops when the stream runs out, like a renderer reaching the end.
        let (s, b) = (shared.clone(), bps.clone());
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(20));
                let mut g = s.lock().unwrap();
                let bps = *b.lock().unwrap();
                if g.state == "PLAYING" && bps > 0.0 {
                    let have = g.received.len() as f64 / bps;
                    if let Some(len) = g.length.or(Some(have)).filter(|_| g.fetched)
                        && g.now() >= len
                    {
                        g.pos = len;
                        g.at = None;
                        g.state = "STOPPED";
                    }
                }
            }
        });
        Self { port, shared, bytes_per_sec: bps }
    }

    pub fn location(&self) -> String {
        format!("http://127.0.0.1:{}/desc.xml", self.port)
    }
}

fn handle(mut conn: TcpStream, port: u16, s: &Arc<Mutex<Shared>>, _bps: &Arc<Mutex<f64>>, sink: &str) {
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
    let out = match action.as_str() {
        "GetProtocolInfo" => format!("<Source></Source><Sink>{sink}</Sink>"),
        "SetAVTransportURI" => {
            g.uri = arg("CurrentURI").unwrap_or_default();
            g.didl = arg("CurrentURIMetaData").unwrap_or_default();
            g.state = "STOPPED";
            g.received.clear();
            g.pos = 0.0;
            g.at = None;
            g.length = None;
            g.fetch += 1;
            g.fetched = false;
            g.started_fetch = false;
            String::new()
        }
        "Play" => {
            if g.state != "PLAYING" {
                g.state = "PLAYING";
                g.at = Some(Instant::now());
                if !g.started_fetch {
                    g.started_fetch = true;
                    let (uri, s2, id) = (g.uri.clone(), s.clone(), g.fetch);
                    std::thread::spawn(move || fetch(uri, s2, id));
                }
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
            format!("<Track>1</Track><RelTime>{}:{:02}:{:06.3}</RelTime>", (t / 3600.0) as u32, ((t / 60.0) as u32) % 60, t % 60.0)
        }
        "GetVolume" => "<CurrentVolume>30</CurrentVolume>".into(),
        "SetVolume" => String::new(),
        _ => return reply(&mut conn, "500 Internal Server Error", "<errorCode>401</errorCode>".into()),
    };
    drop(g);
    let xml = format!(
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:{action}Response xmlns:u="x">{out}</u:{action}Response></s:Body></s:Envelope>"#
    );
    reply(&mut conn, "200 OK", xml);
}

/// Fetch the stream like a renderer: first a short probe, then for real, to the end.
fn fetch(uri: String, s: Arc<Mutex<Shared>>, id: u64) {
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
        if probe {
            let mut b = [0u8; 100];
            let _ = r.read_exact(&mut b);
            s.lock().unwrap().requests += 1;
            return;
        }
        s.lock().unwrap().requests += 1;
        let mut buf = [0u8; 16384];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut g = s.lock().unwrap();
                    if g.fetch != id {
                        return;
                    }
                    g.received.extend_from_slice(&buf[..n]);
                }
            }
        }
    };
    get(true);
    get(false);
    let mut g = s.lock().unwrap();
    if g.fetch == id {
        g.fetched = true;
    }
}
