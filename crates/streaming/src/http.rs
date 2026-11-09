//! HTTP for the services: JSON calls, and byte sources the decoder reads a track from.
//!
//! A stream is read front to back once — the engine decodes it into RAM as it arrives and
//! seeks there — so the sources here are forward-only. A dropped connection is picked up again
//! where it broke off (a Range request), so a flaky Wi-Fi costs a pause, not the track.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use hifi_engine::MediaSource;
use serde_json::Value;
use ureq::Agent;

const USER_AGENT: &str = concat!("Hifi/", env!("CARGO_PKG_VERSION"));

/// For API calls: everything answers within seconds, or not at all.
pub fn api() -> &'static Agent {
    static A: OnceLock<Agent> = OnceLock::new();
    A.get_or_init(|| {
        Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(25)))
            .http_status_as_error(false)
            .user_agent(USER_AGENT)
            .build()
            .into()
    })
}

/// For audio: connecting and the first byte are bounded; the body takes as long as the track.
fn media() -> &'static Agent {
    static A: OnceLock<Agent> = OnceLock::new();
    A.get_or_init(|| {
        Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_recv_response(Some(Duration::from_secs(20)))
            .http_status_as_error(false)
            .user_agent(USER_AGENT)
            .build()
            .into()
    })
}

/// Read a JSON answer; an HTTP error becomes the service's own message when it sent one.
pub fn json(resp: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<Value> {
    let mut resp = resp.map_err(|e| anyhow!("no connection: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().context("reading the answer")?;
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status >= 400 {
        let msg = ["message", "userMessage", "error_description", "error", "detail"]
            .iter()
            .find_map(|k| v.get(k).and_then(Value::as_str))
            .or_else(|| v.pointer("/errors/0/detail").and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(|| text.chars().take(200).collect());
        bail!(HttpError { status, message: msg });
    }
    if v.is_null() && !text.trim().is_empty() {
        bail!("the service answered something other than JSON");
    }
    Ok(v)
}

/// An HTTP error status with the service's message.
#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (HTTP {})", self.message, self.status)
    }
}

impl std::error::Error for HttpError {}

pub fn status_of(e: &anyhow::Error) -> Option<u16> {
    e.downcast_ref::<HttpError>().map(|h| h.status)
}

/// Small binary download (cover art), at most `limit` bytes.
pub fn bytes(url: &str, limit: u64) -> Result<Vec<u8>> {
    let mut resp = api().get(url).call()?;
    if resp.status().as_u16() >= 400 {
        bail!("HTTP {}", resp.status().as_u16());
    }
    Ok(resp.body_mut().with_config().limit(limit).read_to_vec()?)
}

/// Bytes of a `data:…;base64,` URL (TIDAL hands manifests over that way).
pub fn data_url(url: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let rest = url.strip_prefix("data:")?;
    let (head, data) = rest.split_once(',')?;
    if head.ends_with(";base64") {
        base64::engine::general_purpose::STANDARD.decode(data.trim()).ok()
    } else {
        Some(data.as_bytes().to_vec())
    }
}

type Reader = Box<dyn Read + Send>;

/// One URL, read front to back, resuming after a dropped connection.
struct Resumable {
    url: String,
    reader: Option<Reader>,
    pos: u64,
    len: Option<u64>,
    failures: u32,
}

impl Resumable {
    fn new(url: String) -> Self {
        Self { url, reader: None, pos: 0, len: None, failures: 0 }
    }

    fn connect(&mut self) -> io::Result<()> {
        let mut req = media().get(&self.url);
        if self.pos > 0 {
            req = req.header("Range", format!("bytes={}-", self.pos));
        }
        let resp = req.call().map_err(io::Error::other)?;
        let status = resp.status().as_u16();
        if status >= 400 {
            return Err(io::Error::other(format!("the server answered HTTP {status}")));
        }
        if self.len.is_none() && status == 200 {
            self.len = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
        }
        let mut reader: Reader = Box::new(resp.into_body().into_with_config().limit(u64::MAX).reader());
        // A server that ignores Range starts from the top again: skip what we already have.
        if self.pos > 0 && status == 200 {
            io::copy(&mut (&mut reader).take(self.pos), &mut io::sink())?;
        }
        self.reader = Some(reader);
        Ok(())
    }

    fn done(&self) -> bool {
        self.len.is_some_and(|l| self.pos >= l)
    }
}

impl Read for Resumable {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.done() {
                return Ok(0);
            }
            if self.reader.is_none() {
                if let Err(e) = self.connect() {
                    self.failures += 1;
                    if self.failures > 6 {
                        return Err(e);
                    }
                    std::thread::sleep(Duration::from_millis(400 * u64::from(self.failures)));
                    continue;
                }
            }
            match self.reader.as_mut().unwrap().read(buf) {
                Ok(0) if self.len.is_some_and(|l| self.pos < l) => self.reader = None, // cut short
                Ok(n) => {
                    self.pos += n as u64;
                    if n > 0 {
                        self.failures = 0;
                    }
                    return Ok(n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    log::warn!("stream read failed at byte {}: {e}; reconnecting", self.pos);
                    self.reader = None;
                    self.failures += 1;
                    if self.failures > 6 {
                        return Err(e);
                    }
                }
            }
        }
    }
}

/// What the decoder reads: one file, or a DASH track's segments one after another (an init
/// segment and its fragments make one valid fragmented MP4).
pub struct Stream {
    parts: Mutex<(Vec<String>, Option<Resumable>)>,
    inline: Mutex<io::Cursor<Vec<u8>>>,
    len: Option<u64>,
}

impl Stream {
    /// A single file over HTTP.
    pub fn file(url: &str) -> Result<Self> {
        let mut r = Resumable::new(url.to_string());
        r.connect().context("opening the stream")?;
        let len = r.len;
        Ok(Self { parts: Mutex::new((Vec::new(), Some(r))), inline: Mutex::new(io::Cursor::new(Vec::new())), len })
    }

    /// Segments in order; `inline` is data that comes before them (an init segment carried
    /// in the manifest itself).
    pub fn segments(urls: Vec<String>, inline: Vec<u8>) -> Self {
        let mut urls = urls;
        urls.reverse(); // popped from the back
        Self { parts: Mutex::new((urls, None)), inline: Mutex::new(io::Cursor::new(inline)), len: None }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inline.get_mut().unwrap().read(buf)?;
        if n > 0 {
            return Ok(n);
        }
        let (urls, current) = self.parts.get_mut().unwrap();
        loop {
            if let Some(r) = current.as_mut() {
                let n = r.read(buf)?;
                if n > 0 {
                    return Ok(n);
                }
            }
            match urls.pop() {
                Some(u) => *current = Some(Resumable::new(u)),
                None => return Ok(0),
            }
        }
    }
}

impl Seek for Stream {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "a stream is read front to back"))
    }
}

impl MediaSource for Stream {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}

/// `%XX`-encode a query value.
pub fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A query parameter of a URL, decoded.
pub fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1.split('#').next()?;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| {
            let v = v.replace('+', " ");
            let b = v.as_bytes();
            let mut out = Vec::new();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'%'
                    && let Some(x) = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    out.push(x);
                    i += 3;
                    continue;
                }
                out.push(b[i]);
                i += 1;
            }
            String::from_utf8_lossy(&out).into_owned()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    /// A tiny HTTP server that serves `data`, honours Range, and drops the first connection
    /// halfway through.
    fn serve(data: Vec<u8>, drop_first_at: Option<usize>) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut first = true;
            for s in l.incoming() {
                let mut s = s.unwrap();
                let mut req = [0u8; 4096];
                let n = s.read(&mut req).unwrap_or(0);
                let req = String::from_utf8_lossy(&req[..n]).to_lowercase();
                let from = req.lines().find_map(|l| l.strip_prefix("range: bytes=")).and_then(|r| r.trim_end_matches('-').trim_end_matches("-\r").split('-').next()?.trim().parse::<usize>().ok()).unwrap_or(0);
                let body = &data[from..];
                let head = if from > 0 {
                    format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {from}-{}/{}\r\nConnection: close\r\n\r\n", body.len(), data.len() - 1, data.len())
                } else {
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
                };
                let _ = s.write_all(head.as_bytes());
                let cut = if first { drop_first_at } else { None };
                first = false;
                let _ = s.write_all(&body[..cut.unwrap_or(body.len()).min(body.len())]);
            }
        });
        format!("http://{addr}/track.flac")
    }

    pub fn serve_pub(data: Vec<u8>) -> String {
        serve(data, None)
    }

    #[test]
    fn resumes_after_a_dropped_connection() {
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let url = serve(data.clone(), Some(100_000));
        let mut s = Stream::file(&url).unwrap();
        assert_eq!(s.byte_len(), Some(300_000));
        let mut got = Vec::new();
        s.read_to_end(&mut got).unwrap();
        assert!(got == data, "got {} bytes", got.len());
    }

    #[test]
    fn segments_concatenate() {
        let a = serve(b"hello ".to_vec(), None);
        let b = serve(b"world".to_vec(), None);
        let mut s = Stream::segments(vec![a, b], b">> ".to_vec());
        let mut got = String::new();
        s.read_to_string(&mut got).unwrap();
        assert_eq!(got, ">> hello world");
    }

    #[test]
    fn decodes_flac_over_http() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/t16_44.flac");
        let data = std::fs::read(&path).unwrap();
        let url = serve(data, Some(5000));
        let full = hifi_engine::load_track(&path).unwrap();
        let mut hint = hifi_engine::Hint::new();
        hint.with_extension("flac");
        let t = hifi_engine::open_stream(Box::new(Stream::file(&url).unwrap()), hint, None, Default::default()).unwrap();
        assert!(t.wait_for(usize::MAX, Duration::from_secs(20)));
        assert_eq!(t.frames(), full.frames());
        assert!(t.to_vec() == full.to_vec(), "samples differ after the reconnect");
    }

    #[test]
    fn query_params() {
        assert_eq!(query_param("http://localhost:1/?code_autorisation=ab%2Bc&x=1", "code_autorisation").as_deref(), Some("ab+c"));
        assert_eq!(query_param("https://tidal.com/android/login/auth?code=XYZ&state=1", "code").as_deref(), Some("XYZ"));
        assert_eq!(data_url("data:application/dash+xml;base64,PE1QRC8+").unwrap(), b"<MPD/>");
    }
}

#[cfg(test)]
mod fmp4_probe {
    #[test]
    fn fragmented_mp4_flac_streams() {
        let Ok(p) = std::env::var("HIFI_FMP4") else { return };
        let data = std::fs::read(&p).unwrap();
        // Split into three "segments" at arbitrary points: the decoder must not care.
        let (a, rest) = data.split_at(data.len() / 3);
        let (b, c) = rest.split_at(rest.len() / 2);
        let urls = [a, b, c].iter().map(|d| super::tests::serve_pub(d.to_vec())).collect();
        let mut hint = hifi_engine::Hint::new();
        hint.with_extension("mp4");
        let t = hifi_engine::open_stream(Box::new(super::Stream::segments(urls, Vec::new())), hint, None, Default::default()).unwrap();
        assert!(t.wait_for(usize::MAX, std::time::Duration::from_secs(20)));
        let full = hifi_engine::load_track(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/t16_44.flac").as_path()).unwrap();
        eprintln!("fmp4 frames {} vs flac {}, {} Hz {:?} bits", t.frames(), full.frames(), t.sample_rate, t.bits_per_sample);
        assert_eq!(t.frames(), full.frames());
        assert!(t.to_vec() == full.to_vec());
    }
}
