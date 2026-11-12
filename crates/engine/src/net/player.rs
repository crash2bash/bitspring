//! Playing on a network player (UPnP renderer or Cast device).
//!
//! A pump thread pulls audio from an ordinary [`Playback`] (the same processing, gapless
//! joins and seeking as on a local DAC), encodes it and appends it to a stream the player
//! fetches over HTTP. Each track gets its own stream, with its own title and cover, cut at
//! the exact sample where the playback moved on. The next track's stream is offered to the
//! player as soon as it begins (UPnP's "next URI", a Cast queue item), so players that
//! prepare the next track ahead play an album without gaps; others start it when the
//! current one ends.
//!
//! The pump keeps only a few seconds ahead of what the player says it is playing, so
//! pausing, seeking and the position shown stay close to what is heard. Everything the app
//! sees — the track, the position, the meters — follows the player's clock, not the pump's.
//!
//! A control thread does all the talking to the player (calls take a moment and must not
//! hold up the app or the pump) and polls its position and state.

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

/// What a player gets before it has started: enough for one that wants a buffer first.
const INITIAL: f64 = 5.0;
/// A player that hasn't fetched the stream by then isn't going to.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
const POLL: Duration = Duration::from_millis(500);

/// The rates a player is assumed to take, up to its maximum.
const RATES: [u32; 8] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000];

pub fn rates(t: &Target) -> Vec<RateRange> {
    RATES.iter().filter(|&&hz| hz <= t.max_rate()).map(|&hz| RateRange { min: hz as f64, max: hz as f64 }).collect()
}

enum Cmd {
    /// Play this stream now (the first one, after a seek, or the next one on a player that
    /// can't take it ahead).
    Load(Offer),
    /// This stream follows the current one: hand it over ahead if the player can.
    Next(Offer),
    Pause,
    Play,
}

#[derive(Clone)]
struct Offer {
    url: String,
    mime: String,
    meta: Meta,
}

/// The player's clock: where it is in what we have sent since the last seek (all streams
/// end to end), in seconds.
struct Clock {
    pos: f64,
    at: Instant,
    /// Advancing (the player plays and we haven't paused it).
    running: bool,
    /// The player has been seen playing.
    started: bool,
    /// Its reported position moves: trust it. Until then, count from when it started.
    reports: bool,
    /// When the first stream was handed to the player.
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

    /// A poll's answer (already turned into overall seconds). Positions often come in whole
    /// seconds: only a clear disagreement moves the clock, so it doesn't jitter.
    fn report(&mut self, reported: Option<f64>, local: f64, playing: bool) {
        if playing {
            self.started = true;
        }
        if let Some(r) = reported {
            if local > 0.5 {
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

/// One stream: `track` from `offset` seconds, starting `at` seconds into what was sent.
struct Entry {
    url: String,
    path: String,
    stream: Arc<Stream>,
    track: Arc<Track>,
    offset: f64,
    at: f64,
    /// Its length once the pump has finished it.
    len: Option<f64>,
}

struct Inner {
    renderer: Target,
    format: Format,
    playback: Playback,
    /// Held by the pump for each block, and by a seek: they never interleave.
    turn: Mutex<()>,
    /// Bumped to make the pump start over with a new stream (a seek).
    generation: AtomicU64,
    clock: Mutex<Clock>,
    /// The streams since the last seek, in order.
    entries: Mutex<Vec<Entry>>,
    /// The first track's start, until the pump has opened its stream.
    first: Mutex<Option<(Arc<Track>, f64)>>,
    /// Peak per channel of each block, by overall time, for meters that match what's heard.
    meters: Mutex<VecDeque<(f64, [f32; 2])>>,
    /// Seconds sent since the last seek.
    sent: Mutex<f64>,
    paused: AtomicBool,
    /// The pump has written the end of the last track.
    ended: AtomicBool,
    /// …and the player has played it out.
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

    /// The track heard now, the position in it, and how many streams came before it.
    fn heard(&self) -> (Arc<Track>, f64, usize) {
        let now = self.clock.lock().unwrap().now().min(*self.sent.lock().unwrap());
        let e = self.entries.lock().unwrap();
        if e.is_empty() {
            let (t, offset) = self.first.lock().unwrap().clone().expect("a first track");
            return (t, offset, 0);
        }
        let i = e.iter().rposition(|x| x.at <= now + 1e-6).unwrap_or(0);
        let x = &e[i];
        let mut pos = now - x.at;
        if let Some(len) = x.len {
            pos = pos.min(len);
        }
        (x.track.clone(), (x.offset + pos.max(0.0)).min(x.track.duration_secs()), i)
    }
}

pub struct NetPlayer {
    inner: Arc<Inner>,
    pump: Option<JoinHandle<()>>,
    control: Option<JoinHandle<()>>,
    /// Streams passed before the last seek (for `switches`).
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
        // The player must reach us: check before telling it where.
        http::server()?;
        // Pausing is the pump's (and the player's) business: the playback itself never
        // pauses, or it would hand the pump silence.
        let playback = Playback::new(track.clone(), &PlayOptions { start_paused: false, ..opts.clone() }, &plan, rate as f64)?;
        let physical = StreamFormat { sample_rate: rate as f64, bits: bits as u32, float: false, channels: format.channels as u32, non_mixable: true };
        let out = if bits == 16 { OutFormat::I16 } else { OutFormat::I24Packed };
        let path = output::signal_path(&track, opts, &plan, rate as f64, Some(physical), out, true, Transport::Network);
        let (tx, rx) = mpsc::channel();
        let offset = plan.start_frame as f64 / track.sample_rate as f64;
        let inner = Arc::new(Inner {
            renderer,
            format,
            playback,
            turn: Mutex::new(()),
            generation: AtomicU64::new(1),
            clock: Mutex::new(Clock::new()),
            entries: Mutex::new(Vec::new()),
            first: Mutex::new(Some((track, offset))),
            meters: Mutex::new(VecDeque::new()),
            sent: Mutex::new(0.0),
            paused: AtomicBool::new(opts.start_paused),
            ended: AtomicBool::new(false),
            done: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            error: Mutex::new(None),
            control: Mutex::new(tx),
        });
        let control = {
            let inner = inner.clone();
            std::thread::Builder::new().name("hifi-net-control".into()).spawn(move || control_loop(inner, rx))?
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
        self.inner.heard().0
    }

    pub fn upcoming(&self) -> Option<Arc<Track>> {
        let (_, _, i) = self.inner.heard();
        if let Some(e) = self.inner.entries.lock().unwrap().get(i + 1) {
            return Some(e.track.clone());
        }
        self.inner.playback.upcoming()
    }

    pub fn switches(&self) -> u64 {
        self.base_switches.load(Ordering::Relaxed) + self.inner.heard().2 as u64
    }

    /// Line up `track` to follow: as on a DAC, plus it must fit the stream's bit depth.
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
        self.inner.heard().1
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
        // Some players drop the connection during a long pause (or were stopped on their
        // own remote): resume with a fresh stream from where it stopped.
        let (_, _, idx) = i.heard();
        let dropped = !i.ended.load(Ordering::Acquire)
            && i.clock.lock().unwrap().started
            && i.entries.lock().unwrap().get(idx).is_some_and(|e| e.stream.readers().0 == 0 && e.len.is_none());
        if !(dropped && self.seek(self.position_secs())) {
            i.send(Cmd::Play);
        }
    }

    /// Jump within the track being heard: a new stream from that point. False when the
    /// pump has already moved on to the next track (then start anew, as on a DAC).
    pub fn seek(&self, secs: f64) -> bool {
        let i = &self.inner;
        let _turn = i.turn.lock().unwrap();
        let (heard, _, passed) = i.heard();
        if !Arc::ptr_eq(&heard, &i.playback.heard()) || !i.playback.seek(secs) {
            return false;
        }
        self.base_switches.fetch_add(passed as u64, Ordering::Relaxed);
        *i.first.lock().unwrap() = Some((heard.clone(), secs.clamp(0.0, heard.duration_secs())));
        // The old streams go now; the pump opens the new one when it sees the new generation.
        let old: Vec<Entry> = i.entries.lock().unwrap().drain(..).collect();
        if let Ok(server) = http::server() {
            for e in old {
                server.withdraw(&e.path);
            }
        }
        i.meters.lock().unwrap().clear();
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

    /// Peak per channel since the last call, of what the player is playing now.
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
        // Closing the channel ends the control thread, which stops the player: before the
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

/// The pump's state for the stream it is writing.
struct Writer {
    encoder: Encoder,
    stream: Arc<Stream>,
    /// Rendered samples waiting to fill a whole block (FLAC blocks are all one size).
    pending: Vec<i32>,
    /// Seconds written to this stream.
    written: f64,
}

struct Pump {
    inner: Arc<Inner>,
    server: &'static http::Server,
    base: String,
    mime: String,
    length: Option<u64>,
    encoded: Vec<u8>,
}

impl Pump {
    /// Open a stream for `track` (from `offset` s), starting `at` s into what was sent.
    fn open(&self, track: Arc<Track>, offset: f64, at: f64) -> (Writer, Offer) {
        let f = self.inner.format;
        let encoder = Encoder::new(f);
        let stream = Stream::new(self.mime.clone(), encoder.header(), self.length);
        let path = self.server.publish(stream.clone(), f.extension());
        let url = format!("{}{path}", self.base);
        let meta = self.meta(&track);
        self.inner.entries.lock().unwrap().push(Entry { url: url.clone(), path, stream: stream.clone(), track, offset, at, len: None });
        let w = Writer { encoder, stream, pending: Vec::with_capacity(2 * BLOCK * f.channels as usize), written: 0.0 };
        (w, Offer { url, mime: self.mime.clone(), meta })
    }

    /// Encode whole blocks of `w.pending` (and the rest too, at the end of the stream).
    /// Returns the seconds written, recording each block's peaks for the meters.
    fn flush(&mut self, w: &mut Writer, last: bool, from: f64, peaks: [f32; 2]) -> f64 {
        let f = self.inner.format;
        let ch = f.channels as usize;
        let mut t = from;
        while w.pending.len() >= BLOCK * ch || (last && !w.pending.is_empty()) {
            let n = w.pending.len().min(BLOCK * ch);
            self.encoded.clear();
            w.encoder.encode(&w.pending[..n], &mut self.encoded);
            w.pending.drain(..n);
            w.stream.push(&self.encoded);
            self.inner.meters.lock().unwrap().push_back((t, peaks));
            let secs = (n / ch) as f64 / f.rate as f64;
            t += secs;
            w.written += secs;
        }
        t - from
    }

    /// Cover art and tags for the player's display.
    fn meta(&self, track: &Track) -> Meta {
        let m = &track.meta;
        let cover = m.cover.as_ref().map(|(mime, bytes)| {
            let ext = if mime.contains("png") { "png" } else { "jpg" };
            format!("{}{}", self.base, self.server.publish_file(Arc::new(bytes.clone()), mime, ext))
        });
        Meta {
            title: m.title.clone().unwrap_or_else(|| "BitSpring".into()),
            artist: m.artist.clone().or_else(|| m.album_artist.clone()),
            album: m.album.clone(),
            cover,
        }
    }

    fn withdraw_all(&self) {
        for e in self.inner.entries.lock().unwrap().drain(..) {
            self.server.withdraw(&e.path);
        }
    }
}

fn pump_loop(inner: Arc<Inner>) {
    let f = inner.format;
    let ch = f.channels as usize;
    let mut bytes = vec![0u8; BLOCK * ch * 4];
    let server = match http::server() {
        Ok(s) => s,
        Err(e) => return inner.fail(UserError::other(e)),
    };
    let local = match http::local_ip_for(inner.renderer.ip()) {
        Ok(ip) => ip,
        Err(_) => return inner.fail(UserError::RendererUnreachable { device: inner.renderer.name().to_string() }),
    };
    let mut pump = Pump {
        server,
        base: format!("http://{}", host(local, server.port())),
        mime: inner.renderer.mime().unwrap_or_else(|| f.mime()),
        length: (f.codec == Codec::Wav).then_some(u32::MAX as u64 - 1),
        encoded: Vec::with_capacity(BLOCK * ch * 4 + 64),
        inner: inner.clone(),
    };
    let lead = inner.renderer.lead();
    let mut generation = 0;
    let mut writer: Option<Writer> = None;

    while !inner.quit.load(Ordering::Acquire) {
        let g = inner.generation.load(Ordering::Acquire);
        if g != generation {
            generation = g;
            pump.withdraw_all();
            let (track, offset) = inner.first.lock().unwrap().clone().expect("a first track");
            let (w, offer) = pump.open(track, offset, 0.0);
            writer = Some(w);
            inner.send(Cmd::Load(offer));
        }
        let Some(w) = writer.as_mut() else { break };
        let sent = *inner.sent.lock().unwrap();
        let allowed = {
            let c = inner.clock.lock().unwrap();
            if c.started { c.now() + lead } else { INITIAL.max(lead) + c.handed.elapsed().as_secs_f64() }
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
        let switched = inner.playback.switches() != switches;
        if got == 0 && !finished && !switched {
            // Still decoding (or refilling after a seek): wait rather than send silence.
            drop(turn);
            std::thread::sleep(Duration::from_millis(3));
            continue;
        }
        let peaks = inner.playback.take_meter();
        let samples = bytes[..got * ch * 4].as_chunks::<4>().0.iter().map(|b| i32::from_le_bytes(*b));
        let mut t = sent;
        if switched {
            // The block holds the end of one track and the start of the next: cut it there.
            let new = inner.playback.frames_since_switch().min(got);
            let mut samples = samples;
            w.pending.extend(samples.by_ref().take((got - new) * ch));
            t += pump.flush(w, true, t, peaks);
            w.stream.end();
            let len = w.written;
            if let Some(e) = inner.entries.lock().unwrap().last_mut() {
                e.len = Some(len);
            }
            let (mut next, offer) = pump.open(inner.playback.heard(), 0.0, t);
            next.pending.extend(samples);
            inner.send(Cmd::Next(offer));
            *w = next;
        } else {
            w.pending.extend(samples);
        }
        t += pump.flush(w, finished, t, peaks);
        *inner.sent.lock().unwrap() = t;
        if finished {
            w.stream.end();
            let len = w.written;
            if let Some(e) = inner.entries.lock().unwrap().last_mut() {
                e.len = Some(len);
            }
            inner.ended.store(true, Ordering::Release);
        }
        drop(turn);
    }
    pump.withdraw_all();
}

fn host(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(v4) => format!("{v4}:{port}"),
        IpAddr::V6(v6) => format!("[{v6}]:{port}"),
    }
}

// ---------------------------------------------------------------------------------------------
// The control thread
// ---------------------------------------------------------------------------------------------

fn control_loop(inner: Arc<Inner>, rx: Receiver<Cmd>) {
    let name = inner.renderer.name().to_string();
    let mut remote: Box<dyn Remote> = inner.renderer.remote();
    let refused = |detail: String| UserError::RendererRefused { device: name.clone(), detail };
    let unreachable = || UserError::RendererUnreachable { device: name.clone() };
    let mut failures = 0;
    // When the first stream was handed over, until the player has fetched it.
    let mut awaiting: Option<Instant> = None;
    // The player's state lags a command for a moment: don't read anything into it then.
    let mut last_command = Instant::now();
    // A next stream the player couldn't take ahead: loaded when the current one ends.
    let mut waiting: Option<Offer> = None;
    // The stream the player plays now (by URL), as far as we know.
    let mut current: Option<String> = None;
    loop {
        match rx.recv_timeout(POLL) {
            Ok(Cmd::Load(offer)) => {
                last_command = Instant::now();
                waiting = None;
                match remote.load(&offer.url, &offer.mime, &offer.meta, !inner.paused.load(Ordering::Relaxed)) {
                    Ok(()) => {
                        inner.clock.lock().unwrap().handed = Instant::now();
                        awaiting = Some(Instant::now());
                        current = Some(offer.url);
                    }
                    Err(e) => inner.fail(if is_unreachable(&e) { unreachable() } else { refused(format!("{e:#}")) }),
                }
            }
            Ok(Cmd::Next(offer)) => match remote.queue(&offer.url, &offer.mime, &offer.meta) {
                Ok(true) => {}
                Ok(false) => waiting = Some(offer),
                Err(e) if is_unreachable(&e) => inner.fail(unreachable()),
                Err(_) => waiting = Some(offer),
            },
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
        let st = match remote.status() {
            Ok(st) => {
                failures = 0;
                st
            }
            Err(_) => {
                failures += 1;
                if failures >= 6 {
                    inner.fail(unreachable());
                }
                continue;
            }
        };
        if let Some(url) = &st.current {
            current = Some(url.clone());
        }
        let paused = inner.paused.load(Ordering::Relaxed);
        // Which of our streams that is, where it began, and whether the pump has finished it.
        let (at, finished_stream, last_one) = {
            let e = inner.entries.lock().unwrap();
            let i = current.as_deref().and_then(|u| e.iter().position(|x| x.url == u)).unwrap_or(0);
            match e.get(i) {
                Some(x) => (Some(x.at), x.len.is_some(), i + 1 == e.len()),
                None => (None, false, true),
            }
        };
        let playing = st.state == State::Playing && !paused;
        let active = matches!(st.state, State::Playing | State::Paused);
        let reported = match (at, st.position) {
            // Between tracks a stopped player says 0: that isn't news.
            (Some(at), Some(p)) if active => Some((at + p, p)),
            _ => None,
        };
        inner.clock.lock().unwrap().report(reported.map(|r| r.0), reported.map_or(0.0, |r| r.1), playing);
        if inner.entries.lock().unwrap().first().is_some_and(|e| e.stream.readers().1 > 0) {
            awaiting = None;
        }
        if let Some(t) = awaiting
            && t.elapsed() > CONNECT_TIMEOUT
            && !paused
        {
            inner.fail(UserError::RendererSilent { device: name.clone() });
            awaiting = None;
        }
        let started = inner.clock.lock().unwrap().started;
        let ended_here = started && finished_stream && matches!(st.state, State::Stopped | State::NoMedia) && last_command.elapsed() > Duration::from_secs(2);
        // The player reached the end of a stream and couldn't take the next ahead: start it.
        if ended_here && let Some(offer) = waiting.take() {
            last_command = Instant::now();
            match remote.load(&offer.url, &offer.mime, &offer.meta, !paused) {
                Ok(()) => current = Some(offer.url),
                Err(e) => inner.fail(refused(format!("{e:#}"))),
            }
            continue;
        }
        if ended_here && last_one && inner.ended.load(Ordering::Acquire) {
            inner.done.store(true, Ordering::Release);
        }
        // Paused or stopped on the player itself (its app, its remote): show it.
        if started
            && last_command.elapsed() > Duration::from_secs(3)
            && !paused
            && !finished_stream
            && matches!(st.state, State::Paused | State::Stopped | State::NoMedia)
        {
            inner.paused.store(true, Ordering::Relaxed);
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
    use crate::net::encode::tests::decode;
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

    const BPS: f64 = 44_100.0 * 4.0;

    /// Play two tracks back to back; returns the renderer and when track two showed up.
    fn two_tracks(gapless: bool) -> (FakeRenderer, Vec<i32>, Vec<i32>, f64) {
        let fake = FakeRenderer::start("http-get:*:audio/mpeg:*,http-get:*:audio/flac:*", BPS, gapless);
        let renderer = upnp::describe_at(&fake.location()).unwrap();
        assert_eq!((renderer.name.as_str(), renderer.codec), ("Test Streamer", Codec::Flac));
        let (t1, s1) = track(1, 44_100, "One");
        let (t2, s2) = track(2, 30_000, "Two");
        let p = NetPlayer::start(Target::Upnp(renderer), t1, &PlayOptions::default()).unwrap();
        assert!(p.path.bit_perfect, "{:?}", p.path);
        assert!(p.queue_next(t2.clone()));
        let start = Instant::now();
        let mut switched_at = None;
        while !p.is_finished() {
            assert!(start.elapsed() < Duration::from_secs(10), "never finished");
            assert_eq!(p.output_error(), None);
            if switched_at.is_none() && Arc::ptr_eq(&p.heard(), &t2) {
                switched_at = Some(start.elapsed().as_secs_f64());
                assert!(p.position_secs() < 0.4, "{}", p.position_secs());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        (fake, s1, s2, switched_at.expect("moved on to track two"))
    }

    fn check_streams(fake: &FakeRenderer, s1: &[i32], s2: &[i32]) {
        let g = fake.shared.lock().unwrap();
        assert_eq!(g.played.len(), 2, "{:?}", g.played);
        let (a, b) = (&g.played[0], &g.played[1]);
        assert!(g.didl[a].contains("<dc:title>One</dc:title>"), "{}", g.didl[a]);
        assert!(g.didl[b].contains("<dc:title>Two</dc:title>"), "{}", g.didl[b]);
        // Each track its own stream, cut at the exact sample.
        let (rate, got1) = decode(g.received[a].clone(), "flac", 16);
        assert_eq!(rate, 44_100);
        assert!(got1 == s1, "track one: {} samples, want {}", got1.len(), s1.len());
        let (_, got2) = decode(g.received[b].clone(), "flac", 16);
        assert!(got2 == s2, "track two: {} samples, want {}", got2.len(), s2.len());
    }

    #[test]
    fn a_gapless_renderer_gets_each_track_ahead_bit_perfect() {
        let (fake, s1, s2, at) = two_tracks(true);
        // Shown when the renderer reaches it, not when it was sent.
        assert!((0.9..1.6).contains(&at), "switched at {at:.2} s");
        check_streams(&fake, &s1, &s2);
        let g = fake.shared.lock().unwrap();
        assert_eq!(g.actions.iter().filter(|a| *a == "SetAVTransportURI").count(), 1, "{:?}", g.actions);
        assert!(g.actions.iter().any(|a| a == "SetNextAVTransportURI"));
    }

    #[test]
    fn a_renderer_without_next_uri_starts_the_next_track_itself() {
        let (fake, s1, s2, at) = two_tracks(false);
        assert!((0.9..2.6).contains(&at), "switched at {at:.2} s");
        check_streams(&fake, &s1, &s2);
        let g = fake.shared.lock().unwrap();
        assert_eq!(g.actions.iter().filter(|a| *a == "SetAVTransportURI").count(), 2, "{:?}", g.actions);
    }

    #[test]
    fn pause_and_seek_follow_the_renderer() {
        let fake = FakeRenderer::start("http-get:*:audio/flac:*", BPS, true);
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
        assert_eq!(g.actions.iter().filter(|a| *a == "SetAVTransportURI").count(), 2, "{:?}", g.actions);
        let (_, got) = decode(g.received[&g.uri].clone(), "flac", 16);
        let from = 3 * 44_100 * 2;
        assert!(got[..] == s1[from..], "the stream after the seek starts at 3 s exactly");
    }
}
