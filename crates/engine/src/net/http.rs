//! A small HTTP/1.1 server for the streams network players fetch, and a client for talking
//! to them (device descriptions, SOAP). Plain HTTP on the local network only: renderers
//! don't do anything else.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

// ---------------------------------------------------------------------------------------------
// Live streams
// ---------------------------------------------------------------------------------------------

/// How much of a stream stays in memory for a player that reconnects or opens it twice
/// (many probe a stream, close it and open it again). The pump keeps a player only a few
/// seconds ahead, so this is plenty.
const WINDOW: usize = 32 << 20;

/// One live stream: a header, then blocks as the pump encodes them. Any number of readers;
/// each follows on from where it asked to start.
pub struct Stream {
    pub mime: String,
    /// The stream has a declared length (WAV): answer with it, as players expect.
    content_length: Option<u64>,
    state: Mutex<Buf>,
    cv: Condvar,
}

struct Buf {
    header: Vec<u8>,
    /// Stream offset (after the header) of `data[0]`.
    base: u64,
    data: VecDeque<u8>,
    /// Offsets (after the header) where blocks begin, within `data`: where a reader that
    /// fell out of the window picks up.
    blocks: VecDeque<u64>,
    ended: bool,
    /// Withdrawn: readers stop now.
    closed: bool,
    readers: usize,
    /// Requests served so far.
    requests: u64,
    /// When a reader last asked for or received data.
    last_read: Option<Instant>,
}

impl Stream {
    pub fn new(mime: String, header: Vec<u8>, content_length: Option<u64>) -> Arc<Self> {
        Arc::new(Self {
            mime,
            content_length,
            state: Mutex::new(Buf {
                header,
                base: 0,
                data: VecDeque::new(),
                blocks: VecDeque::new(),
                ended: false,
                closed: false,
                readers: 0,
                requests: 0,
                last_read: None,
            }),
            cv: Condvar::new(),
        })
    }

    /// Append one encoded block.
    pub fn push(&self, block: &[u8]) {
        let mut b = self.state.lock().unwrap();
        let at = b.base + b.data.len() as u64;
        b.blocks.push_back(at);
        b.data.extend(block);
        while b.data.len() > WINDOW && b.blocks.len() > 1 {
            let next = b.blocks[1];
            let drop = (next - b.base) as usize;
            b.data.drain(..drop);
            b.base = next;
            b.blocks.pop_front();
        }
        drop(b);
        self.cv.notify_all();
    }

    /// Nothing more will come: readers finish what is there and close.
    pub fn end(&self) {
        self.state.lock().unwrap().ended = true;
        self.cv.notify_all();
    }

    /// Withdraw the stream: every reader stops at once.
    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.cv.notify_all();
    }

    /// Bytes encoded so far (after the header).
    pub fn produced(&self) -> u64 {
        let b = self.state.lock().unwrap();
        b.base + b.data.len() as u64
    }

    /// Readers connected right now, and requests served so far.
    pub fn readers(&self) -> (usize, u64) {
        let b = self.state.lock().unwrap();
        (b.readers, b.requests)
    }

    pub fn last_read(&self) -> Option<Instant> {
        self.state.lock().unwrap().last_read
    }

    /// Serve the stream to one connection, starting at byte `from` of the whole stream.
    fn serve(&self, out: &mut TcpStream, from: u64) -> std::io::Result<()> {
        let mut cursor = from;
        {
            let mut b = self.state.lock().unwrap();
            b.readers += 1;
            b.requests += 1;
            b.last_read = Some(Instant::now());
        }
        let result = (|| {
            let mut chunk = Vec::with_capacity(64 << 10);
            loop {
                chunk.clear();
                {
                    let mut b = self.state.lock().unwrap();
                    loop {
                        if b.closed {
                            return Ok(());
                        }
                        let h = b.header.len() as u64;
                        if cursor < h {
                            chunk.extend_from_slice(&b.header[cursor as usize..]);
                            // Straight on with the oldest block still held.
                            cursor = h + b.blocks.front().copied().unwrap_or(b.base);
                            break;
                        }
                        let rel = cursor - h;
                        let end = b.base + b.data.len() as u64;
                        if rel < b.base {
                            // Fell out of the window: pick up at the oldest whole block.
                            cursor = h + b.blocks.front().copied().unwrap_or(b.base);
                            continue;
                        }
                        if rel < end {
                            let skip = (rel - b.base) as usize;
                            let n = (end - rel).min(64 << 10) as usize;
                            chunk.extend(b.data.range(skip..skip + n));
                            cursor += n as u64;
                            break;
                        }
                        if b.ended {
                            return Ok(());
                        }
                        b = self.cv.wait_timeout(b, Duration::from_secs(1)).unwrap().0;
                    }
                    b.last_read = Some(Instant::now());
                }
                out.write_all(&chunk)?;
            }
        })();
        self.state.lock().unwrap().readers -= 1;
        result
    }
}

// ---------------------------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------------------------

enum Resource {
    Stream(Arc<Stream>),
    File { mime: String, bytes: Arc<Vec<u8>> },
}

const MAX_CONNECTIONS: u64 = 32;

pub struct Server {
    port: u16,
    resources: Mutex<HashMap<String, Resource>>,
    next: AtomicU64,
}

/// The process's one server, started on first use on a free port.
pub fn server() -> Result<&'static Server> {
    static SERVER: OnceLock<std::result::Result<Server, String>> = OnceLock::new();
    SERVER
        .get_or_init(|| {
            let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(|e| e.to_string())?;
            let port = listener.local_addr().map_err(|e| e.to_string())?.port();
            std::thread::Builder::new()
                .name("hifi-http".into())
                .spawn(move || {
                    // A player opens one or two connections at a time; anything far beyond
                    // that isn't a player.
                    static OPEN: AtomicU64 = AtomicU64::new(0);
                    for conn in listener.incoming().flatten() {
                        if OPEN.load(Ordering::Acquire) >= MAX_CONNECTIONS {
                            continue; // dropped: closes it
                        }
                        OPEN.fetch_add(1, Ordering::AcqRel);
                        let spawned = std::thread::Builder::new().name("hifi-http-conn".into()).spawn(move || {
                            if let Ok(server) = server() {
                                let _ = server.handle(conn);
                            }
                            OPEN.fetch_sub(1, Ordering::AcqRel);
                        });
                        if spawned.is_err() {
                            OPEN.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
            Ok(Server { port, resources: Mutex::new(HashMap::new()), next: AtomicU64::new(1) })
        })
        .as_ref()
        .map_err(|e| anyhow!("couldn't start the stream server: {e}"))
}

impl Server {
    pub fn port(&self) -> u16 {
        self.port
    }

    fn name(&self, ext: &str) -> String {
        // Whoever has the address can listen in: 128 random bits (from the OS's random keys
        // for hash maps), so nobody else on the network can guess it.
        use std::hash::{BuildHasher, Hasher};
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let random = || {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(n);
            h.finish()
        };
        format!("/{:016x}{:016x}.{ext}", random(), random())
    }

    /// Serve `stream` under a new path (returned, e.g. "/7-1a2b3c4d.flac").
    pub fn publish(&self, stream: Arc<Stream>, ext: &str) -> String {
        let path = self.name(ext);
        self.resources.lock().unwrap().insert(path.clone(), Resource::Stream(stream));
        path
    }

    /// Serve a small file (cover art) under a new path.
    pub fn publish_file(&self, bytes: Arc<Vec<u8>>, mime: &str, ext: &str) -> String {
        let path = self.name(ext);
        self.resources.lock().unwrap().insert(path.clone(), Resource::File { mime: mime.into(), bytes });
        path
    }

    pub fn withdraw(&self, path: &str) {
        if let Some(Resource::Stream(s)) = self.resources.lock().unwrap().remove(path) {
            s.close();
        }
    }

    fn handle(&self, mut conn: TcpStream) -> Result<()> {
        conn.set_read_timeout(Some(Duration::from_secs(10)))?;
        conn.set_write_timeout(Some(Duration::from_secs(30)))?;
        let _ = conn.set_nodelay(true);
        let mut reader = BufReader::new(conn.try_clone()?);
        let (method, target, headers) = read_head(&mut reader)?;
        let path = target.split('?').next().unwrap_or("").to_string();
        let resource = match self.resources.lock().unwrap().get(&path) {
            Some(Resource::Stream(s)) => Some(Ok(s.clone())),
            Some(Resource::File { mime, bytes }) => Some(Err((mime.clone(), bytes.clone()))),
            None => None,
        };
        let head_only = method == "HEAD";
        match resource {
            None => {
                conn.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
            }
            Some(Err((mime, bytes))) => {
                write!(conn, "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len())?;
                if !head_only {
                    conn.write_all(&bytes)?;
                }
            }
            Some(Ok(stream)) => {
                let from = headers.get("range").and_then(|r| parse_range(r)).unwrap_or(0);
                let mut head = String::new();
                match (stream.content_length, from) {
                    (Some(len), 0) => head += &format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\n"),
                    (Some(len), f) => head += &format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {f}-{}/{len}\r\n",
                        len.saturating_sub(f),
                        len.saturating_sub(1)
                    ),
                    // No length: the stream ends when the connection does.
                    (None, _) => head += "HTTP/1.1 200 OK\r\n",
                }
                let dlna_flags = "DLNA.ORG_OP=00;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";
                head += &format!(
                    "Content-Type: {}\r\nAccept-Ranges: none\r\ntransferMode.dlna.org: Streaming\r\ncontentFeatures.dlna.org: {dlna_flags}\r\nCache-Control: no-cache\r\nConnection: close\r\nServer: BitSpring UPnP/1.0\r\n\r\n",
                    stream.mime
                );
                conn.write_all(head.as_bytes())?;
                if !head_only {
                    let from = if stream.content_length.is_some() { from } else { 0 };
                    stream.serve(&mut conn, from)?;
                }
            }
        }
        let _ = conn.flush();
        let _ = conn.shutdown(std::net::Shutdown::Both);
        Ok(())
    }
}

fn parse_range(r: &str) -> Option<u64> {
    r.trim().strip_prefix("bytes=")?.split('-').next()?.trim().parse().ok()
}

/// Request or status line, then headers (names lower-cased).
/// Lines longer than this, or more header lines, and the message is refused.
const MAX_LINE: u64 = 8 << 10;
const MAX_HEADERS: usize = 100;

fn read_line(r: &mut impl BufRead, line: &mut String) -> Result<usize> {
    line.clear();
    let n = r.take(MAX_LINE).read_line(line)?;
    if n as u64 == MAX_LINE && !line.ends_with('\n') {
        bail!("HTTP line too long");
    }
    Ok(n)
}

fn read_head(r: &mut impl BufRead) -> Result<(String, String, HashMap<String, String>)> {
    let mut line = String::new();
    read_line(r, &mut line)?;
    let mut parts = line.split_whitespace();
    let a = parts.next().unwrap_or("").to_string();
    let b = parts.next().unwrap_or("").to_string();
    let mut headers = HashMap::new();
    for _ in 0..=MAX_HEADERS {
        if read_line(r, &mut line)? == 0 {
            break;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            bail!("too many HTTP headers");
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    if a.is_empty() {
        bail!("empty HTTP message");
    }
    Ok((a, b, headers))
}

// ---------------------------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------------------------

pub struct Url {
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    pub fn parse(url: &str) -> Result<Self> {
        let rest = url.trim().strip_prefix("http://").ok_or_else(|| anyhow!("not an http URL: {url}"))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !h.contains(']') || h.ends_with(']') => (h, p.parse().context("bad port")?),
            _ => (authority, 80),
        };
        Ok(Self { host: host.trim_matches(['[', ']']).to_string(), port, path: path.to_string() })
    }

    /// `relative` (an absolute URL, an absolute path or a path) resolved against this one.
    pub fn join(&self, relative: &str) -> String {
        if relative.starts_with("http://") || relative.starts_with("https://") {
            relative.to_string()
        } else if relative.starts_with('/') {
            format!("http://{}:{}{relative}", self.host, self.port)
        } else {
            let dir = &self.path[..self.path.rfind('/').map_or(0, |i| i + 1)];
            format!("http://{}:{}{dir}{relative}", self.host, self.port)
        }
    }

    fn addr(&self) -> Result<SocketAddr> {
        (self.host.as_str(), self.port).to_socket_addrs()?.next().ok_or_else(|| anyhow!("can't resolve {}", self.host))
    }
}

pub struct Response {
    pub status: u16,
    pub body: String,
}

/// One HTTP request. `body` with `headers` for SOAP; GET when there is no body.
pub fn request(url: &str, headers: &[(&str, &str)], body: Option<&str>, timeout: Duration) -> Result<Response> {
    let u = Url::parse(url)?;
    // Addresses come from devices on the network: nothing that could smuggle a second line
    // into the request.
    if u.path.bytes().chain(u.host.bytes()).any(|b| b.is_ascii_control() || b == b' ') {
        bail!("bad URL from a network device: {url:?}");
    }
    let mut conn = TcpStream::connect_timeout(&u.addr()?, timeout).with_context(|| format!("can't reach {}", u.host))?;
    conn.set_read_timeout(Some(timeout))?;
    conn.set_write_timeout(Some(timeout))?;
    let mut req = format!(
        "{} {} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: BitSpring UPnP/1.0\r\nConnection: close\r\n",
        if body.is_some() { "POST" } else { "GET" },
        u.path,
        u.host,
        u.port
    );
    for (k, v) in headers {
        req += &format!("{k}: {v}\r\n");
    }
    if let Some(b) = body {
        req += &format!("Content-Length: {}\r\n", b.len());
    }
    req += "\r\n";
    if let Some(b) = body {
        req += b;
    }
    conn.write_all(req.as_bytes())?;
    let mut r = BufReader::new(conn);
    let (_version, status, headers) = read_head(&mut r)?;
    let status: u16 = status.parse().context("bad HTTP status")?;
    // Device descriptions and SOAP answers are a few kilobytes: anything past this is refused,
    // whatever the headers say, and so is an answer that dribbles in for too long.
    const MAX_BODY: usize = 1 << 20;
    let deadline = Instant::now() + timeout * 3;
    let mut bytes = Vec::new();
    if headers.get("transfer-encoding").is_some_and(|t| t.eq_ignore_ascii_case("chunked")) {
        let mut size = String::new();
        loop {
            if Instant::now() > deadline {
                bail!("answer took too long");
            }
            read_line(&mut r, &mut size)?;
            let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16).context("bad chunk size")?;
            if n == 0 {
                break;
            }
            if n > MAX_BODY - bytes.len() {
                bail!("answer too large");
            }
            let at = bytes.len();
            bytes.resize(at + n, 0);
            r.read_exact(&mut bytes[at..])?;
            read_line(&mut r, &mut size)?;
        }
    } else if let Some(n) = headers.get("content-length").and_then(|n| n.parse::<usize>().ok()) {
        if n > MAX_BODY {
            bail!("answer too large");
        }
        bytes.resize(n, 0);
        r.read_exact(&mut bytes)?;
    } else {
        (&mut r).take(MAX_BODY as u64).read_to_end(&mut bytes)?;
    }
    Ok(Response { status, body: String::from_utf8_lossy(&bytes).into_owned() })
}

/// Our address as seen from `peer`: the interface the system would use to reach it.
pub fn local_ip_for(peer: IpAddr) -> Result<IpAddr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    s.connect((peer, 9))?;
    Ok(s.local_addr()?.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_resolve_like_a_browser() {
        let u = Url::parse("http://192.168.1.5:1400/xml/device_description.xml").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("192.168.1.5", 1400));
        assert_eq!(u.join("/MediaRenderer/AVTransport/Control"), "http://192.168.1.5:1400/MediaRenderer/AVTransport/Control");
        assert_eq!(u.join("ctl/avt"), "http://192.168.1.5:1400/xml/ctl/avt");
        assert_eq!(Url::parse("http://host/").unwrap().port, 80);
    }

    #[test]
    fn a_reader_gets_the_header_then_follows_the_stream() {
        let server = server().unwrap();
        let stream = Stream::new("audio/flac".into(), b"HEAD".to_vec(), None);
        let path = server.publish(stream.clone(), "flac");
        stream.push(b"one");
        let url = format!("http://127.0.0.1:{}{path}", server.port());
        let s2 = stream.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            s2.push(b"two");
            s2.end();
        });
        let r = request(&url, &[], None, Duration::from_secs(5)).unwrap();
        t.join().unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "HEADonetwo");
        // A second request (a player probing, then opening for real) starts over.
        let r = request(&url, &[], None, Duration::from_secs(5)).unwrap();
        assert_eq!(r.body, "HEADonetwo");
        assert_eq!(stream.readers(), (0, 2));
        server.withdraw(&path);
        assert_eq!(request(&url, &[], None, Duration::from_secs(5)).unwrap().status, 404);
    }
}
