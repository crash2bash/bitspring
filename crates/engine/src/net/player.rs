//! Playing on a network player (UPnP renderer or Cast device).
//!
//! A pump thread pulls audio from an ordinary [`Playback`] (the same processing, gapless
//! joins and seeking as on a local DAC), encodes it and appends it to a live stream the
//! renderer fetches over HTTP. The pump keeps only a few seconds ahead of what the renderer
//! says it is playing, so pausing, seeking and the position shown stay close to what is
//! heard. Everything the app sees — the track, the position, the meters — follows the
//! renderer's clock, not the pump's.
//!
//! A control thread does all the talking to the renderer (SOAP calls take a moment and must
//! not hold up the app or the pump) and polls its position and state.

use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::encode::{BLOCK, Codec, Encoder, Format};
use super::http::{self, Stream};
use super::{Meta, Remote, State, Target};
use crate::decode::Track;
use crate::dsp::eq::EqPreset;
use crate::output::{self, OutBuf, OutFormat, PlayOptions, Playback, SignalPath, StreamFormat, Transport};
use crate::rate::RateRange;
use crate::UserError;

/// How far ahead of the renderer's position the stream runs once it plays.
const LEAD: f64 = 3.0;
/// What a renderer gets before it has started: enough for one that wants a buffer first.
const INITIAL: f64 = 5.0;
/// A renderer that hasn't fetched the stream by then isn't going to.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
const POLL: Duration = Duration::from_millis(500);

/// The rates a renderer is assumed to take, up to its maximum.
const RATES: [u32; 8] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000];

pub fn rates(t: &Target) -> Vec<RateRange> {
    RATES.iter().filter(|&&hz| hz <= t.max_rate()).map(|&hz| RateRange { min: hz as f64, max: hz as f64 }).collect()
}

enum Cmd {
    /// Play this URL (a new stream: the first one, or after a seek).
    Start { url: String, mime: String, meta: Meta },
    Pause,
    Play,
}

/// The renderer's clock: where in the current stream it is, in seconds.
struct Clock {
    pos: f64,
    at: Instant,
    /// Advancing (the renderer plays and we haven't paused it).
    running: bool,
    /// The renderer has been seen playing this stream.
    started: bool,
    /// Its reported position moves: trust it. Until then, count from when it started.
    reports: bool,
    /// When the stream was handed to the renderer.
    handed: Instant,
}

impl Clock {
    fn new() -> Self {
        Self { pos: 0.0, at: Instant::now(), running: false, started: false, reports: false, handed: Instant::now() }
    }

    fn now(&self) -> f64 {
        self.pos + if self.running { self.at.elapsed().as_secs_f64() } else { 0.0 }
    }

    fn set_running(&mut self, running: bool) {
        if running != self.running {
            self.pos = self.now();
            self.at = Instant::now();
            self.running = running;
        }
    }

    /// A poll's answer. Positions often come in whole seconds: only a clear disagreement
    /// moves the clock, so it doesn't jitter.
    fn report(&mut self, reported: Option<f64>, playing: bool) {
        if playing {
            self.started = true;
        }
        if let Some(r) = reported {
            if r > 0.5 {
                self.reports = true;
            }
            if self.reports {
                let diff = self.now() - r;
                if !(-0.25..=1.25).contains(&diff) {
                    self.pos = r;
                    self.at = Instant::now();
                }
            }
        }
        self.set_running(playing);
    }
}

/// What the stream carries, in order: from `at` seconds into the stream, `track` from
/// `offset` seconds into it.
struct Entry {
    at: f64,
    track: Arc<Track>,
    offset: f64,
}

struct Inner {
    renderer: Target,
    format: Format,
    playback: Playback,
    /// Held by the pump for each block, and by a seek: they never interleave.
    turn: Mutex<()>,
    /// Bumped to make the pump start a new stream (a seek).
    generation: AtomicU64,
    clock: Mutex<Clock>,
    timeline: Mutex<Vec<Entry>>,
    /// Peak per channel of each block, by stream time, for meters that match what's heard.
    meters: Mutex<VecDeque<(f64, [f32; 2])>>,
    /// Seconds of audio in the current stream so far.
    sent: Mutex<f64>,
    stream: Mutex<Option<Arc<Stream>>>,
    paused: AtomicBool,
    /// The pump has written the end of the last track.
    ended: AtomicBool,
    /// …and the renderer has played it out.
    done: AtomicBool,
    quit: AtomicBool,
    error: Mutex<Option<UserError>>,
    control: Mutex<Sender<Cmd>>,
}

impl Inner {
    fn fail(&self, e: UserError) {
        self.error.lock().unwrap().get_or_insert(e);
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.control.lock().unwrap().send(cmd);
    }

    /// The entry heard now, and how many entries came before it.
    fn heard_entry(&self) -> (Arc<Track>, f64, usize) {
        let now = self.clock.lock().unwrap().now().min(*self.sent.lock().unwrap());
        let t = self.timeline.lock().unwrap();
        let i = t.iter().rposition(|e| e.at <= now + 1e-6).unwrap_or(0);
        let e = &t[i];
        let pos = e.offset + (now - e.at).max(0.0);
        (e.track.clone(), pos.min(e.track.duration_secs()), i)
    }
}

pub struct NetPlayer {
    inner: Arc<Inner>,
    pump: Option<JoinHandle<()>>,
    control: Option<JoinHandle<()>>,
    /// Entries before the heard one that have been passed (for `switches`).
    base_switches: AtomicU64,
    pub path: SignalPath,
}

impl NetPlayer {
    pub fn start(renderer: Target, track: Arc<Track>, opts: &PlayOptions) -> Result<Self> {
        let rates = rates(&renderer);
        let plan = output::plan(&track, opts, &rates, renderer.name(), false)?;
        let rate = plan.target.round() as u32;
        let source_bits = track.bits_per_sample.unwrap_or(24);
        let bits: u16 = if renderer.max_bits() <= 16 || (source_bits <= 16 && !plan.processing) { 16 } else { 24 };
        let format = Format { codec: renderer.codec(), rate, channels: track.channels.min(8) as u16, bits };
        // The renderer must reach us: check before telling it where.
        http::server()?;
        // Pausing is the pump's (and the renderer's) business: the playback itself never
        // pauses, or it would hand the pump silence.
        let playback = Playback::new(track.clone(), &PlayOptions { start_paused: false, ..opts.clone() }, &plan, rate as f64)?;
        let physical = StreamFormat { sample_rate: rate as f64, bits: bits as u32, float: false, channels: format.channels as u32, non_mixable: true };
        let out = if bits == 16 { OutFormat::I16 } else { OutFormat::I24Packed };
        let path = output::signal_path(&track, opts, &plan, rate as f64, Some(physical), out, true, Transport::Network);
        let (tx, rx) = mpsc::channel();
        let inner = Arc::new(Inner {
            renderer,
            format,
            playback,
            turn: Mutex::new(()),
            generation: AtomicU64::new(1),
            clock: Mutex::new(Clock::new()),
            timeline: Mutex::new(vec![Entry { at: 0.0, offset: plan.start_frame as f64 / track.sample_rate as f64, track }]),
            meters: Mutex::new(VecDeque::new()),
            sent: Mutex::new(0.0),
            stream: Mutex::new(None),
            paused: AtomicBool::new(opts.start_paused),
            ended: AtomicBool::new(false),
            done: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            error: Mutex::new(None),
            control: Mutex::new(tx),
        });
        let control = {
            let inner = inner.clone();
            std::thread::Builder::new().name("hifi-upnp".into()).spawn(move || control_loop(inner, rx))?
        };
        let pump = {
            let inner = inner.clone();
            std::thread::Builder::new().name("hifi-net-pump".into()).spawn(move || pump_loop(inner))?
        };
        Ok(Self { inner, pump: Some(pump), control: Some(control), base_switches: AtomicU64::new(0), path })
    }

    pub fn renderer(&self) -> &Target {
        &self.inner.renderer
    }

    pub fn heard(&self) -> Arc<Track> {
        self.inner.heard_entry().0
    }

    pub fn upcoming(&self) -> Option<Arc<Track>> {
        let (_, _, i) = self.inner.heard_entry();
        if let Some(e) = self.inner.timeline.lock().unwrap().get(i + 1) {
            return Some(e.track.clone());
        }
        self.inner.playback.upcoming()
    }

    pub fn switches(&self) -> u64 {
        self.base_switches.load(Ordering::Relaxed) + self.inner.heard_entry().2 as u64
    }

    /// Line up `track` to follow without a break: as on a DAC, plus it must fit the stream's
    /// bit depth.
    pub fn queue_next(&self, track: Arc<Track>) -> bool {
        if track.bits_per_sample.unwrap_or(24) > self.inner.format.bits as u32 {
            return false;
        }
        self.inner.playback.queue_next(track)
    }

    pub fn clear_next(&self) {
        self.inner.playback.clear_next()
    }

    pub fn position_secs(&self) -> f64 {
        self.inner.heard_entry().1
    }

    pub fn duration_secs(&self) -> f64 {
        self.heard().duration_secs()
    }

    pub fn is_finished(&self) -> bool {
        let i = &self.inner;
        i.ended.load(Ordering::Acquire)
            && (i.done.load(Ordering::Acquire) || i.clock.lock().unwrap().now() >= *i.sent.lock().unwrap() - 0.2)
    }

    pub fn is_paused(&self) -> bool {
        self.inner.paused.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, paused: bool) {
        let i = &self.inner;
        if i.paused.swap(paused, Ordering::Relaxed) == paused {
            return;
        }
        i.clock.lock().unwrap().set_running(false);
        if paused {
            return i.send(Cmd::Pause);
        }
        // Some renderers drop the connection during a long pause (or were stopped on their
        // own remote): resume with a fresh stream from where it stopped.
        let dropped = !i.ended.load(Ordering::Acquire)
            && i.clock.lock().unwrap().started
            && i.stream.lock().unwrap().as_ref().is_some_and(|s| s.readers().0 == 0);
        if !(dropped && self.seek(self.position_secs())) {
            i.send(Cmd::Play);
        }
    }

    /// Jump within the track being heard: a new stream from that point. False when the
    /// pump has already moved on to the next track (then start anew, as on a DAC).
    pub fn seek(&self, secs: f64) -> bool {
        let i = &self.inner;
        let _turn = i.turn.lock().unwrap();
        let heard = i.heard_entry().0;
        if !Arc::ptr_eq(&heard, &i.playback.heard()) || !i.playback.seek(secs) {
            return false;
        }
        let passed = i.heard_entry().2 as u64;
        self.base_switches.fetch_add(passed, Ordering::Relaxed);
        *i.timeline.lock().unwrap() = vec![Entry { at: 0.0, track: heard.clone(), offset: secs.clamp(0.0, heard.duration_secs()) }];
        i.meters.lock().unwrap().clear();
        // The new stream's clock starts at its beginning.
        *i.clock.lock().unwrap() = Clock::new();
        *i.sent.lock().unwrap() = 0.0;
        i.ended.store(false, Ordering::Release);
        i.done.store(false, Ordering::Release);
        i.generation.fetch_add(1, Ordering::AcqRel);
        true
    }

    pub fn output_peak(&self) -> Option<f32> {
        self.inner.playback.output_peak()
    }

    /// Peak per channel since the last call, of what the renderer is playing now.
    pub fn take_meter(&self) -> [f32; 2] {
        let now = self.inner.clock.lock().unwrap().now();
        let mut m = self.inner.meters.lock().unwrap();
        let mut peak = [0f32; 2];
        while m.front().is_some_and(|(at, _)| *at <= now) {
            let (_, p) = m.pop_front().expect("checked");
            peak = [peak[0].max(p[0]), peak[1].max(p[1])];
        }
        peak
    }

    pub fn underruns(&self) -> u64 {
        self.inner.playback.underruns()
    }

    pub fn is_buffering(&self) -> bool {
        self.inner.playback.is_buffering() || (!self.is_paused() && !self.inner.clock.lock().unwrap().started)
    }

    pub fn buffered_secs(&self) -> f64 {
        self.inner.playback.buffered_secs()
    }

    pub fn source_error(&self) -> Option<String> {
        self.inner.playback.source_error()
    }

    pub fn output_error(&self) -> Option<UserError> {
        self.inner.error.lock().unwrap().clone()
    }

    pub fn update_eq(&self, eq: &EqPreset, bypass: bool) -> bool {
        self.inner.playback.update_eq(eq, bypass)
    }
}

impl Drop for NetPlayer {
    fn drop(&mut self) {
        self.inner.quit.store(true, Ordering::Release);
        if let Some(p) = self.pump.take() {
            let _ = p.join();
        }
        // Closing the channel ends the control thread, which stops the renderer: before the
        // next player on it starts.
        *self.inner.control.lock().unwrap() = mpsc::channel().0;
        if let Some(c) = self.control.take() {
            let _ = c.join();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The pump
// ---------------------------------------------------------------------------------------------

fn pump_loop(inner: Arc<Inner>) {
    let f = inner.format;
    let ch = f.channels as usize;
    let mut bytes = vec![0u8; BLOCK * ch * 4];
    // Rendered samples waiting to fill a whole block (FLAC blocks are all one size).
    let mut pending: Vec<i32> = Vec::with_capacity(2 * BLOCK * ch);
    let mut encoded = Vec::with_capacity(BLOCK * ch * 4 + 64);
    let mut generation = 0;
    let mut encoder = Encoder::new(f);
    let mut path: Option<String> = None;
    let server = match http::server() {
        Ok(s) => s,
        Err(e) => return inner.fail(UserError::other(e)),
    };
    let local = match http::local_ip_for(inner.renderer.ip()) {
        Ok(ip) => ip,
        Err(_) => return inner.fail(UserError::RendererUnreachable { device: inner.renderer.name().to_string() }),
    };
    let mime = inner.renderer.mime().unwrap_or_else(|| f.mime());
    let length = (f.codec == Codec::Wav).then_some(u32::MAX as u64 - 1);

    while !inner.quit.load(Ordering::Acquire) {
        let g = inner.generation.load(Ordering::Acquire);
        if g != generation {
            generation = g;
            if let Some(p) = path.take() {
                server.withdraw(&p);
            }
            encoder = Encoder::new(f);
            pending.clear();
            let stream = Stream::new(mime.clone(), encoder.header(), length);
            let p = server.publish(stream.clone(), f.extension());
            let url = format!("http://{}{p}", host(local, server.port()));
            *inner.stream.lock().unwrap() = Some(stream);
            *inner.sent.lock().unwrap() = 0.0;
            inner.ended.store(false, Ordering::Release);
            inner.done.store(false, Ordering::Release);
            *inner.clock.lock().unwrap() = Clock::new();
            let meta = meta_for(&inner, server, &url);
            inner.send(Cmd::Start { url, mime: mime.clone(), meta });
            path = Some(p);
        }
        let sent = *inner.sent.lock().unwrap();
        let allowed = {
            let c = inner.clock.lock().unwrap();
            if c.started { c.now() + LEAD } else { INITIAL + c.handed.elapsed().as_secs_f64() }
        };
        if inner.ended.load(Ordering::Acquire) || inner.paused.load(Ordering::Relaxed) || sent >= allowed {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }

        let turn = inner.turn.lock().unwrap();
        if inner.generation.load(Ordering::Acquire) != generation {
            continue; // a seek came in: start over
        }
        let switches = inner.playback.switches();
        let got = {
            let mut buf = [OutBuf { data: &mut bytes, channels: ch, format: OutFormat::I32 { bits: f.bits as u32 } }];
            inner.playback.shared().render(&mut buf)
        };
        let finished = inner.playback.is_finished();
        if got == 0 && !finished {
            // Still decoding (or refilling after a seek): wait rather than send silence.
            drop(turn);
            std::thread::sleep(Duration::from_millis(3));
            continue;
        }
        let peaks = inner.playback.take_meter();
        if inner.playback.switches() != switches {
            let at = sent + (pending.len() / ch) as f64 / f.rate as f64;
            inner.timeline.lock().unwrap().push(Entry { at, track: inner.playback.heard(), offset: 0.0 });
        }
        pending.extend(bytes[..got * ch * 4].as_chunks::<4>().0.iter().map(|b| i32::from_le_bytes(*b)));
        let mut new_sent = sent;
        let stream = inner.stream.lock().unwrap().clone();
        while pending.len() >= BLOCK * ch || (finished && !pending.is_empty()) {
            let n = pending.len().min(BLOCK * ch);
            encoded.clear();
            encoder.encode(&pending[..n], &mut encoded);
            pending.drain(..n);
            if let Some(s) = &stream {
                s.push(&encoded);
            }
            inner.meters.lock().unwrap().push_back((new_sent, peaks));
            new_sent += (n / ch) as f64 / f.rate as f64;
        }
        *inner.sent.lock().unwrap() = new_sent;
        if finished {
            if let Some(s) = &stream {
                s.end();
            }
            inner.ended.store(true, Ordering::Release);
        }
        drop(turn);
    }
    if let Some(p) = path {
        server.withdraw(&p);
    }
}

fn host(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(v4) => format!("{v4}:{port}"),
        IpAddr::V6(v6) => format!("[{v6}]:{port}"),
    }
}

/// Metadata for the renderer's display: the track the stream starts with.
fn meta_for(inner: &Inner, server: &http::Server, url: &str) -> Meta {
    let track = inner.timeline.lock().unwrap()[0].track.clone();
    let m = &track.meta;
    let title = m.title.clone().unwrap_or_else(|| "BitSpring".into());
    let cover = m.cover.as_ref().map(|(mime, bytes)| {
        let ext = if mime.contains("png") { "png" } else { "jpg" };
        let p = server.publish_file(Arc::new(bytes.clone()), mime, ext);
        let base = url.split('/').take(3).collect::<Vec<_>>().join("/");
        format!("{base}{p}")
    });
    Meta { title, artist: m.artist.clone(), album: m.album.clone(), cover }
}

// ---------------------------------------------------------------------------------------------
// The control thread
// ---------------------------------------------------------------------------------------------

fn control_loop(inner: Arc<Inner>, rx: Receiver<Cmd>) {
    let name = inner.renderer.name().to_string();
    let mut remote: Box<dyn Remote> = inner.renderer.remote();
    let refused = |detail: String| UserError::RendererRefused { device: name.clone(), detail };
    let mut failures = 0;
    // When the current stream was handed over, until the renderer has fetched it.
    let mut awaiting: Option<Instant> = None;
    // The renderer's state lags a command for a moment: don't read anything into it then.
    let mut last_command = Instant::now();
    loop {
        match rx.recv_timeout(POLL) {
            Ok(Cmd::Start { url, mime, meta }) => {
                last_command = Instant::now();
                match remote.load(&url, &mime, &meta, !inner.paused.load(Ordering::Relaxed)) {
                    Ok(()) => {
                        inner.clock.lock().unwrap().handed = Instant::now();
                        awaiting = Some(Instant::now());
                    }
                    Err(e) => inner.fail(if is_unreachable(&e) { UserError::RendererUnreachable { device: name.clone() } } else { refused(format!("{e:#}")) }),
                }
            }
            Ok(Cmd::Pause) => {
                last_command = Instant::now();
                let _ = remote.pause();
            }
            Ok(Cmd::Play) => {
                last_command = Instant::now();
                if let Err(e) = remote.play() {
                    inner.fail(refused(format!("{e:#}")));
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        match remote.status() {
            Ok(st) => {
                failures = 0;
                let s = st.state;
                let playing = s == State::Playing && !inner.paused.load(Ordering::Relaxed);
                inner.clock.lock().unwrap().report(st.position, playing);
                let connected = inner.stream.lock().unwrap().as_ref().is_some_and(|st| st.readers().1 > 0);
                if connected {
                    awaiting = None;
                }
                if let Some(t) = awaiting
                    && t.elapsed() > CONNECT_TIMEOUT
                    && !inner.paused.load(Ordering::Relaxed)
                {
                    inner.fail(UserError::RendererSilent { device: name.clone() });
                    awaiting = None;
                }
                let started = inner.clock.lock().unwrap().started;
                if inner.ended.load(Ordering::Acquire) && started && matches!(s, State::Stopped | State::NoMedia) {
                    inner.done.store(true, Ordering::Release);
                }
                // Paused or stopped on the renderer itself (its app, its remote): show it.
                if started
                    && last_command.elapsed() > Duration::from_secs(3)
                    && !inner.ended.load(Ordering::Acquire)
                    && !inner.paused.load(Ordering::Relaxed)
                    && matches!(s, State::Paused | State::Stopped | State::NoMedia)
                {
                    inner.paused.store(true, Ordering::Relaxed);
                }
            }
            Err(_) => {
                failures += 1;
                if failures >= 6 {
                    inner.fail(UserError::RendererUnreachable { device: name.clone() });
                }
            }
        }
    }
    let _ = remote.stop();
}

fn is_unreachable(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::TrackMeta;
    use crate::net::fake::FakeRenderer;
    use crate::net::upnp;

    fn track(seed: u32, frames: usize, title: &str) -> (Arc<Track>, Vec<i32>) {
        let mut x = seed;
        let ints: Vec<i32> = (0..frames * 2)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x as i32) >> 16
            })
            .collect();
        let f: Vec<f32> = ints.iter().map(|&i| i as f32 / 32768.0).collect();
        let meta = TrackMeta { title: Some(title.into()), artist: Some("Tester".into()), ..Default::default() };
        (Arc::new(Track::from_samples(44_100, 2, Some(16), &f, meta)), ints)
    }

    #[test]
    fn two_tracks_reach_the_renderer_bit_perfect_and_without_a_gap() {
        let fake = FakeRenderer::start("http-get:*:audio/mpeg:*,http-get:*:audio/flac:*");
        *fake.bytes_per_sec.lock().unwrap() = 44_100.0 * 4.0;
        let renderer = upnp::describe_at(&fake.location()).unwrap();
        assert_eq!((renderer.name.as_str(), renderer.codec), ("Test Streamer", Codec::Flac));
        let renderer = Target::Upnp(renderer);

        let (t1, s1) = track(1, 44_100, "One");
        let (t2, s2) = track(2, 30_000, "Two");
        let p = NetPlayer::start(renderer, t1.clone(), &PlayOptions::default()).unwrap();
        assert!(p.path.bit_perfect, "{:?}", p.path);
        assert!(p.queue_next(t2.clone()));

        let start = Instant::now();
        let mut switched_at = None;
        while !p.is_finished() {
            assert!(start.elapsed() < Duration::from_secs(10), "never finished");
            assert_eq!(p.output_error(), None);
            if switched_at.is_none() && Arc::ptr_eq(&p.heard(), &t2) {
                switched_at = Some((start.elapsed().as_secs_f64(), p.position_secs()));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // The second track is shown when the renderer reaches it, not when it was sent.
        let (at, pos) = switched_at.expect("moved on to track two");
        assert!((0.9..1.6).contains(&at), "switched at {at:.2} s");
        assert!(pos < 0.3, "{pos}");

        let g = fake.shared.lock().unwrap();
        assert!(g.didl.contains("<dc:title>One</dc:title>"), "{}", g.didl);
        assert!(g.requests >= 2, "probe and the real fetch");
        let (rate, got) = crate::net::encode::tests::decode(g.received.clone(), "flac", 16);
        assert_eq!(rate, 44_100);
        let want: Vec<i32> = s1.iter().chain(&s2).copied().collect();
        assert!(got.len() >= want.len(), "{} < {}", got.len(), want.len());
        assert!(got[..want.len()] == want[..], "the samples differ");
        assert!(got[want.len()..].iter().all(|&s| s == 0), "only silence after the end");
    }

    #[test]
    fn pause_and_seek_follow_the_renderer() {
        let fake = FakeRenderer::start("http-get:*:audio/flac:*");
        *fake.bytes_per_sec.lock().unwrap() = 44_100.0 * 4.0;
        let renderer = Target::Upnp(upnp::describe_at(&fake.location()).unwrap());
        let (t1, s1) = track(3, 44_100 * 4, "Long");
        let p = NetPlayer::start(renderer, t1, &PlayOptions::default()).unwrap();
        std::thread::sleep(Duration::from_millis(1200));
        let before = p.position_secs();
        assert!((0.6..1.5).contains(&before), "{before}");
        p.set_paused(true);
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(fake.shared.lock().unwrap().state, "PAUSED_PLAYBACK");
        let held = p.position_secs();
        std::thread::sleep(Duration::from_millis(500));
        assert!((p.position_secs() - held).abs() < 0.05, "position moves while paused");
        p.set_paused(false);
        // Seek: a new stream from 3 s.
        assert!(p.seek(3.0));
        assert!((p.position_secs() - 3.0).abs() < 0.3, "{}", p.position_secs());
        let start = Instant::now();
        while !p.is_finished() {
            assert!(start.elapsed() < Duration::from_secs(6), "never finished");
            std::thread::sleep(Duration::from_millis(20));
        }
        let g = fake.shared.lock().unwrap();
        assert!(g.actions.iter().filter(|a| *a == "SetAVTransportURI").count() == 2, "{:?}", g.actions);
        let (_, got) = crate::net::encode::tests::decode(g.received.clone(), "flac", 16);
        let from = 3 * 44_100 * 2;
        assert!(got[..s1.len() - from] == s1[from..], "the stream after the seek starts at 3 s exactly");
    }
}
