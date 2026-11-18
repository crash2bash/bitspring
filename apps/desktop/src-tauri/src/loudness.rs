//! Volume levelling for the app (see hifi_engine::dsp::loudness): what is known about each
//! file's loudness, kept in loudness.json; the analysis of the library in the background; and
//! the gain a track gets when it is opened.
//!
//! A file's own ReplayGain tags come first. Without them the track is measured: at once when
//! it is opened and fits in memory whole (most do), else by the background analysis. A result
//! counts as long as the file has the same size and modification time.
//!
//! What we measured can be written into the files' own tags (ReplayGain), when the user asks:
//! other players then level them the same way, and the figures travel with the files.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use hifi_engine::Track;
use hifi_engine::dsp::DspSettings;
use hifi_engine::dsp::loudness::{self, Known, LevelMode, Measured};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::library::Library;

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    /// The rules the result came from (results from older rules are measured again).
    rules: u32,
    size: u64,
    mtime: u64,
    #[serde(flatten)]
    measured: Measured,
    /// The file's tags hold loudness values: ours, written from this entry, or the ones
    /// another scanner left (those stay). Nothing more to write.
    #[serde(default)]
    tagged: bool,
}

/// Writing what was measured into the files' tags, and how far it has got.
#[derive(Default)]
struct Tagging {
    running: AtomicBool,
    stop: AtomicBool,
    done: AtomicUsize,
    total: AtomicUsize,
    /// The last run of this session: files written, and files that couldn't be.
    written: AtomicUsize,
    failed: AtomicUsize,
    finished: AtomicBool,
    /// Files that couldn't be written this session (read-only, on a disc that went away):
    /// not offered again until the next start.
    refused: Mutex<HashSet<String>>,
}

/// The files waiting to be analysed (in order, and as a set to tell quickly what is queued).
#[derive(Default)]
struct Queue {
    order: VecDeque<String>,
    queued: HashSet<String>,
}

/// The background analysis, and how far it has got.
#[derive(Default)]
struct Work {
    queue: Mutex<Queue>,
    wake: Condvar,
    /// Files handed to the workers since they were last idle, and how many are done.
    total: AtomicUsize,
    done: AtomicUsize,
    busy: AtomicUsize,
    started: AtomicBool,
    /// Files that couldn't be decoded this session: not tried again, and left out of their
    /// album's figure (they can't be played either).
    failed: Mutex<HashSet<String>>,
    tagging: Tagging,
}

#[derive(Default, Clone)]
pub struct Loudness {
    cache: Arc<Mutex<HashMap<String, Entry>>>,
    dirty: Arc<AtomicBool>,
    file: Option<PathBuf>,
    work: Arc<Work>,
}

/// Size and modification time of the file a library path reads (a CUE track: its image).
/// None for what isn't a file (a streaming track).
fn stamp(path: &str) -> Option<(u64, u64)> {
    let m = std::fs::metadata(hifi_engine::source_file(path)).ok()?;
    Some((m.len(), m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs()))
}

impl Loudness {
    pub fn load(app: &AppHandle) -> Self {
        let file = app.path().app_data_dir().ok().map(|d| d.join("loudness.json"));
        let cache = file.as_deref().and_then(hifi_engine::store::load_json).unwrap_or_default();
        let l = Self { cache: Arc::new(Mutex::new(cache)), dirty: Arc::default(), file, work: Arc::default() };
        // Results arrive one track at a time: write them out at most every few seconds.
        let saver = l.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(3));
                if saver.dirty.swap(false, Ordering::AcqRel)
                    && let Some(f) = &saver.file
                {
                    let snapshot = saver.cache.lock().unwrap().clone();
                    // Best effort: a lost write only means measuring those files again.
                    let _ = hifi_engine::store::save_json(f, &snapshot);
                }
            }
        });
        l
    }

    fn cached(&self, path: &str) -> Option<Measured> {
        let s = stamp(path)?;
        self.cache.lock().unwrap().get(path).filter(|e| (e.rules, e.size, e.mtime) == (loudness::RULES_VERSION, s.0, s.1)).map(|e| e.measured.clone())
    }

    fn store(&self, path: &str, measured: Measured) {
        let Some((size, mtime)) = stamp(path) else { return };
        self.cache.lock().unwrap().insert(path.to_string(), Entry { rules: loudness::RULES_VERSION, size, mtime, measured, tagged: false });
        self.dirty.store(true, Ordering::Release);
    }

    /// The file's tags now hold loudness values. (Writing them changed the file's size and
    /// time: what we measured still stands, under the new ones.)
    fn mark_tagged(&self, path: &str, measured: Measured) {
        let Some((size, mtime)) = stamp(path) else { return };
        self.cache.lock().unwrap().insert(path.to_string(), Entry { rules: loudness::RULES_VERSION, size, mtime, measured, tagged: true });
        self.dirty.store(true, Ordering::Release);
    }

    /// Among `paths`: the files whose tags could take what we measured — measured by us
    /// with something to level, of a kind we write such tags into, and not holding them yet.
    fn untagged<'a>(&self, paths: impl Iterator<Item = &'a str>) -> Vec<String> {
        let cache = self.cache.lock().unwrap();
        let refused = self.work.tagging.refused.lock().unwrap();
        paths
            .filter(|p| cache.get(*p).is_some_and(|e| e.rules == loudness::RULES_VERSION && !e.tagged && e.measured.lufs.is_some()))
            .filter(|p| hifi_engine::tags::can_write_gain(p) && !refused.contains(*p))
            .map(str::to_string)
            .collect()
    }

    /// The album `path` belongs to as one programme, measuring here whichever of its tracks
    /// nobody has yet. None: all of it silent.
    fn album_measured(&self, lib: &Library, path: &str, own: &Measured) -> Option<loudness::Loudness> {
        let mut all = Vec::new();
        for p in lib.album_of(path) {
            if p == path {
                all.push(own.clone());
            } else if let Some(m) = self.cached(&p) {
                all.push(m);
            } else if !self.failed(&p) {
                // (A track that can't be decoded is left out, as it is when playing.)
                match loudness::analyze(Path::new(&p)) {
                    Ok(m) => {
                        self.store(&p, m.clone());
                        all.push(m);
                    }
                    Err(_) => {
                        self.work.failed.lock().unwrap().insert(p);
                    }
                }
            }
        }
        loudness::album(all.iter())
    }

    /// Write what we measured into the tags of the library files that lack it (see
    /// [`Self::untagged`]), one file after another, until done or told to stop. Each write is
    /// the tag editor's: into a copy that replaces the file only once its audio is found
    /// identical and the values read back.
    fn write_tags(&self, lib: &Library) {
        let t = &self.work.tagging;
        let paths = self.untagged(lib.tracks().iter().map(|t| t.path.as_str()));
        t.total.store(paths.len(), Ordering::Release);
        t.done.store(0, Ordering::Release);
        t.written.store(0, Ordering::Release);
        t.failed.store(0, Ordering::Release);
        for path in paths {
            if t.stop.load(Ordering::Acquire) {
                break;
            }
            self.write_tags_of(lib, &path);
            t.done.fetch_add(1, Ordering::AcqRel);
        }
        t.finished.store(true, Ordering::Release);
    }

    fn write_tags_of(&self, lib: &Library, path: &str) {
        let t = &self.work.tagging;
        // Changed since it was measured: it will be measured again, and offered then.
        let Some(own) = self.cached(path) else { return };
        let Some(track) = own.loudness() else { return };
        let refuse = || {
            t.failed.fetch_add(1, Ordering::AcqRel);
            t.refused.lock().unwrap().insert(path.to_string());
        };
        match hifi_engine::tags::read_gain(Path::new(path)) {
            // Another scanner's values: they stay, and they are what the track plays by.
            Ok(tags) if !tags.is_empty() => return self.mark_tagged(path, own),
            Ok(_) => {}
            Err(_) => return refuse(),
        }
        let album = self.album_measured(lib, path, &own).unwrap_or(track);
        let gain = hifi_engine::tags::GainTags {
            track_db: track.gain_db,
            track_peak: own.peak,
            album_db: album.gain_db,
            album_peak: album.peak.unwrap_or(own.peak),
        };
        match hifi_engine::tags::write_gain(Path::new(path), &gain) {
            Ok(()) => {
                self.mark_tagged(path, own);
                t.written.fetch_add(1, Ordering::AcqRel);
            }
            Err(_) => refuse(),
        }
    }

    fn failed(&self, path: &str) -> bool {
        self.work.failed.lock().unwrap().contains(path)
    }

    /// What is known about `path` now: its tags first, then what we measured (`own`). The
    /// album's figure, worked out only when `album` asks for it, needs every track of the
    /// album that can be decoded measured.
    fn known(&self, lib: &Library, path: &str, track: &Track, own: Option<&Measured>, album: bool) -> Known {
        let tags = track.meta.gain.known();
        let measured_peak = own.map(|m| m.peak);
        let measured_album = || {
            if !album {
                return None;
            }
            let paths = lib.album_of(path);
            let all: Option<Vec<Measured>> =
                paths.iter().filter(|p| *p == path || !self.failed(p)).map(|p| if p == path { own.cloned() } else { self.cached(p) }).collect();
            loudness::album(all?.iter())
        };
        // A tag that gives a gain without a peak: ours, if we have it.
        let with_peak = |mut l: loudness::Loudness| {
            l.peak = l.peak.or(measured_peak);
            l
        };
        // Measured and found silent (or shorter than one gating block): known, with nothing
        // to level.
        let measured = |m: &Measured| m.loudness().unwrap_or(loudness::Loudness { gain_db: 0.0, peak: Some(m.peak) });
        Known { track: tags.track.map(with_peak).or_else(|| own.map(measured)), album: tags.album.map(with_peak).or_else(measured_album) }
    }

    /// Set the gain `track` (the file at `path`) plays at with these settings — call before it
    /// is started or lined up. A track we know nothing about is measured here if it will be
    /// in memory whole within `wait` (a second or so for an ordinary file being opened); else
    /// it plays as it is this time and is analysed in the background for the next.
    pub fn level(&self, lib: &Library, dsp: &DspSettings, path: &str, track: &Arc<Track>, wait: Duration) {
        // Nothing would use a gain: levelling is off, and headroom only matters under
        // processing that rebuilds the waveform. (The usual case — no waiting, no measuring.)
        let wanted = dsp.level.mode != LevelMode::Off || (dsp.level.headroom && dsp.reshapes());
        if !wanted || track.is_dop() {
            return track.set_level(None);
        }
        // The album's figure: for album mode, and as the peak headroom goes by.
        let album = dsp.level.mode != LevelMode::Track;
        let is_file = stamp(path).is_some();
        let mut own = self.cached(path);
        let tags = track.meta.gain.known().track;
        // The track's own figure is needed in every mode: as the gain, the fallback, or (for
        // headroom) the peak.
        if own.is_none() && !tags.is_some_and(|t| t.peak.is_some()) {
            // Worth waiting for only if it can be in memory whole: not a stream still
            // arriving, a file opened in the middle, or one too long for its window.
            if is_file && track.loads_whole() {
                track.wait_for(usize::MAX, wait);
            }
            match loudness::measure_loaded(track) {
                Some(m) => {
                    self.store(path, m.clone());
                    // (Kept for this once where there is no file to remember it by.)
                    own = Some(m);
                }
                None if is_file => self.want(vec![path.to_string()]),
                None => {}
            }
        }
        let known = self.known(lib, path, track, own.as_ref(), album);
        // What goes by the album's figure and doesn't have it yet: its gain in album mode, its
        // peak when only headroom is on (so the tracks of an album are lowered alike, and
        // keep their relative levels).
        let album_missing = match dsp.level.mode {
            LevelMode::Album => known.album.is_none(),
            LevelMode::Off => known.album.and_then(|a| a.peak).is_none(),
            LevelMode::Track => false,
        };
        if album_missing {
            // The rest of the album, so its tracks agree from the next one on.
            self.want(lib.album_of(path));
        }
        track.set_level(loudness::resolve(&dsp.level, dsp.reshapes(), &known));
    }

    /// Analyse `paths` in the background: those not known yet, not queued already, and not
    /// found undecodable this session.
    fn want(&self, paths: Vec<String>) {
        let w = &self.work;
        let fresh: Vec<String> = {
            let cache = self.cache.lock().unwrap();
            let failed = w.failed.lock().unwrap();
            paths.into_iter().filter(|p| !cache.get(p).is_some_and(|e| e.rules == loudness::RULES_VERSION) && !failed.contains(p)).collect()
        };
        let added = {
            let mut q = w.queue.lock().unwrap();
            let mut added = 0;
            for p in fresh {
                if q.queued.insert(p.clone()) {
                    q.order.push_back(p);
                    added += 1;
                }
            }
            added
        };
        if added == 0 {
            return;
        }
        w.total.fetch_add(added, Ordering::AcqRel);
        if !w.started.swap(true, Ordering::AcqRel) {
            // Each analysis keeps two threads busy (the decoder and the meter): a quarter of
            // the cores in workers leaves half the machine to playback and the UI.
            let workers = std::thread::available_parallelism().map_or(1, |n| n.get() / 4).clamp(1, 3);
            for i in 0..workers {
                let me = self.clone();
                let _ = std::thread::Builder::new().name(format!("hifi-loudness-{i}")).spawn(move || me.work_loop());
            }
        }
        w.wake.notify_all();
    }

    fn work_loop(&self) {
        let w = &self.work;
        loop {
            let path = {
                let mut q = w.queue.lock().unwrap();
                loop {
                    if let Some(p) = q.order.pop_front() {
                        q.queued.remove(&p);
                        w.busy.fetch_add(1, Ordering::AcqRel);
                        break p;
                    }
                    q = w.wake.wait(q).unwrap();
                }
            };
            if self.cached(&path).is_none() {
                match loudness::analyze(Path::new(&path)) {
                    Ok(m) => self.store(&path, m),
                    // It can't be decoded: it stays unknown, and isn't tried again.
                    Err(_) => {
                        w.failed.lock().unwrap().insert(path);
                    }
                }
            }
            w.done.fetch_add(1, Ordering::AcqRel);
            // Everyone idle with nothing queued: the next batch counts from zero.
            let q = w.queue.lock().unwrap();
            if w.busy.fetch_sub(1, Ordering::AcqRel) == 1 && q.order.is_empty() {
                w.total.store(0, Ordering::Release);
                w.done.store(0, Ordering::Release);
            }
        }
    }

    fn stop(&self) {
        let w = &self.work;
        let mut q = w.queue.lock().unwrap();
        let dropped = q.order.len();
        q.order.clear();
        q.queued.clear();
        w.total.fetch_sub(dropped.min(w.total.load(Ordering::Acquire)), Ordering::AcqRel);
        if w.busy.load(Ordering::Acquire) == 0 {
            w.total.store(0, Ordering::Release);
            w.done.store(0, Ordering::Release);
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoudnessStatus {
    /// Library tracks dealt with so far (measured, or found undecodable), of how many.
    analysed: usize,
    tracks: usize,
    /// (done, total) while the background analysis runs.
    running: Option<(usize, usize)>,
    /// Measured files whose tags could take the result and don't hold it yet.
    untagged: usize,
    /// (done, total) while the tags are being written.
    tagging: Option<(usize, usize)>,
    /// (written, failed) of the last run of this session, once it has finished.
    tagged: Option<(usize, usize)>,
}

#[tauri::command(async)]
pub fn loudness_status(l: State<'_, Loudness>, lib: State<'_, Library>) -> LoudnessStatus {
    let tracks = lib.tracks();
    // (A file that can't be decoded counts as dealt with: there is nothing more to do for it.)
    let analysed = {
        let cache = l.cache.lock().unwrap();
        let failed = l.work.failed.lock().unwrap();
        tracks.iter().filter(|t| cache.get(&t.path).is_some_and(|e| e.rules == loudness::RULES_VERSION) || failed.contains(&t.path)).count()
    };
    let (done, total) = (l.work.done.load(Ordering::Acquire), l.work.total.load(Ordering::Acquire));
    let t = &l.work.tagging;
    let load = |n: &AtomicUsize| n.load(Ordering::Acquire);
    LoudnessStatus {
        analysed,
        tracks: tracks.len(),
        running: (total > 0).then_some((done.min(total), total)),
        untagged: l.untagged(tracks.iter().map(|t| t.path.as_str())).len(),
        tagging: t.running.load(Ordering::Acquire).then(|| (load(&t.done), load(&t.total))),
        tagged: (t.finished.load(Ordering::Acquire) && !t.running.load(Ordering::Acquire)).then(|| (load(&t.written), load(&t.failed))),
    }
}

/// Write what was measured into the tags of the library's files, in the background — or
/// stop doing so (the file being written is finished first).
#[tauri::command(async)]
pub fn loudness_write_tags(app: AppHandle, l: State<'_, Loudness>, start: bool) {
    let t = &l.work.tagging;
    if !start {
        return t.stop.store(true, Ordering::Release);
    }
    if t.running.swap(true, Ordering::AcqRel) {
        return; // one run at a time
    }
    t.stop.store(false, Ordering::Release);
    t.finished.store(false, Ordering::Release);
    let me = l.inner().clone();
    std::thread::spawn(move || {
        let lib = app.state::<Library>();
        me.write_tags(&lib);
        me.work.tagging.running.store(false, Ordering::Release);
        // The files changed on disc: the library takes their new size and time.
        if me.work.tagging.written.load(Ordering::Acquire) > 0 {
            lib.rescan_now(app.clone());
        }
    });
}

/// Measure every library track not measured yet, in the background — or stop doing so.
#[tauri::command(async)]
pub fn loudness_analyze(l: State<'_, Loudness>, lib: State<'_, Library>, start: bool) {
    if start {
        l.want(lib.tracks().iter().map(|t| t.path.clone()).collect());
    } else {
        l.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hifi_engine::dsd::DsdMode;
    use hifi_engine::dsp::loudness::{Basis, LevelSettings};
    use hifi_engine::net::encode::{Codec, Encoder, Format};

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bitspring-loudness-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A real FLAC file: 6 s of a stereo 1 kHz tone at `amp` of full scale. (Such a tone
    /// reads its own level in LUFS: 0.5 → −6.02.)
    fn tone(dir: &Path, name: &str, amp: f64) -> String {
        let mut enc = Encoder::new(Format { codec: Codec::Flac, rate: 44_100, channels: 2, bits: 16 });
        let mut bytes = enc.header();
        let samples: Vec<i32> = (0..44_100 * 6)
            .flat_map(|i| {
                let v = (amp * 32_767.0 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 44_100.0).sin()).round() as i32;
                [v, v]
            })
            .collect();
        for block in samples.chunks(4096 * 2) {
            enc.encode(block, &mut bytes);
        }
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn open(path: &str) -> Arc<Track> {
        hifi_engine::open_file(Path::new(path), DsdMode::Pcm).unwrap()
    }

    fn settings(mode: LevelMode) -> DspSettings {
        DspSettings { level: LevelSettings { mode, headroom: false }, ..Default::default() }
    }

    const WAIT: Duration = Duration::from_secs(10);

    #[test]
    fn a_track_nobody_measured_is_measured_when_opened_and_remembered() {
        let d = tmpdir("track");
        let path = tone(&d, "tone.flac", 0.5);
        let (l, lib) = (Loudness::default(), Library::default());
        assert!(l.cached(&path).is_none());
        let t = open(&path);
        l.level(&lib, &settings(LevelMode::Track), &path, &t, WAIT);
        let level = t.level().expect("levelled");
        assert_eq!(level.basis, Basis::Track);
        // −6.02 LUFS brought to −18.
        assert!((level.gain_db() + 11.98).abs() < 0.15, "{:.2} dB", level.gain_db());
        assert!(l.cached(&path).is_some());
        // Next time there is nothing to wait for.
        let again = open(&path);
        l.level(&lib, &settings(LevelMode::Track), &path, &again, Duration::ZERO);
        assert_eq!(again.level(), t.level());
        // The file changed: what we knew about it no longer counts.
        tone(&d, "tone.flac", 0.25);
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(std::time::SystemTime::now() + Duration::from_secs(5)).unwrap();
        assert!(l.cached(&path).is_none());
        // Levelling off: the track plays as it is.
        l.level(&lib, &DspSettings::default(), &path, &t, WAIT);
        assert_eq!(t.level(), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The usual case: nothing would use a gain, so nothing is waited for or measured — not
    /// with everything off, and not with only "headroom for peaks" on (its default) while no
    /// processing runs.
    #[test]
    fn nothing_is_measured_when_nothing_would_use_it() {
        let d = tmpdir("unused");
        let path = tone(&d, "tone.flac", 0.5);
        let (l, lib) = (Loudness::default(), Library::default());
        let headroom_only = DspSettings { level: LevelSettings { mode: LevelMode::Off, headroom: true }, ..Default::default() };
        for dsp in [DspSettings::default(), headroom_only.clone()] {
            let t = open(&path);
            let started = std::time::Instant::now();
            l.level(&lib, &dsp, &path, &t, Duration::from_secs(30));
            assert_eq!(t.level(), None);
            assert!(l.cached(&path).is_none(), "not measured");
            assert_eq!(l.work.total.load(Ordering::Acquire), 0, "nor queued");
            assert!(started.elapsed() < Duration::from_secs(5));
        }
        // With processing on, headroom needs the track's peak: now it is measured. (A tone
        // at half scale has room to spare, so there is still nothing to lower.)
        let upsampling = DspSettings { upsample: hifi_engine::dsp::Upsample::UpTo(192_000.0), ..headroom_only };
        let t = open(&path);
        l.level(&lib, &upsampling, &path, &t, WAIT);
        assert!(l.cached(&path).is_some());
        assert_eq!(t.level(), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// What can't be in memory whole is not waited for: a file opened in the middle goes to
    /// the background analysis at once.
    #[test]
    fn a_track_that_cannot_be_measured_in_memory_is_not_waited_for() {
        let d = tmpdir("nowait");
        let path = tone(&d, "tone.flac", 0.5);
        let (l, lib) = (Loudness::default(), Library::default());
        let t = hifi_engine::open_file_at(Path::new(&path), DsdMode::Pcm, 3.0).unwrap();
        assert!(!t.loads_whole());
        let started = std::time::Instant::now();
        l.level(&lib, &settings(LevelMode::Track), &path, &t, Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(5), "no waiting");
        assert_eq!(t.level().unwrap().basis, Basis::Unknown, "plays as it is this time");
        // …and the background measures it for the next.
        let deadline = std::time::Instant::now() + WAIT;
        while l.cached(&path).is_none() {
            assert!(std::time::Instant::now() < deadline, "the background analysis never got to it");
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A file that can't be decoded is tried once, then left alone — and doesn't keep the
    /// rest of its album from getting the album's gain. Silence counts as measured.
    #[test]
    fn a_broken_file_and_a_silent_one_do_not_hold_anything_up() {
        let d = tmpdir("broken");
        let (loud, quiet, silent) = (tone(&d, "01 loud.flac", 0.5), tone(&d, "02 quiet.flac", 0.125), tone(&d, "03 silent.flac", 0.0));
        let broken = d.join("04 broken.flac").to_string_lossy().into_owned();
        std::fs::write(&broken, b"fLaC and then nothing a decoder could use").unwrap();
        let mut tracks = [&loud, &quiet, &silent].map(|p| hifi_engine::library::scan_file(Path::new(p)).unwrap().0).to_vec();
        tracks.push(hifi_engine::library::TrackInfo { path: broken.clone(), ..tracks[0].clone() });
        let (l, lib) = (Loudness::default(), Library::of(tracks));
        assert_eq!(lib.album_of(&loud).len(), 4);
        let album = settings(LevelMode::Album);
        l.level(&lib, &album, &loud, &open(&loud), WAIT);
        let idle = |l: &Loudness| {
            let deadline = std::time::Instant::now() + WAIT;
            while l.work.total.load(Ordering::Acquire) != 0 {
                assert!(std::time::Instant::now() < deadline, "the background analysis never finished");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        idle(&l);
        assert!(l.failed(&broken) && l.cached(&broken).is_none());
        // The album's gain, from the tracks that play (the silent one adds nothing to it).
        let t = open(&quiet);
        l.level(&lib, &album, &quiet, &t, WAIT);
        let level = t.level().unwrap();
        assert_eq!(level.basis, Basis::Album);
        assert!((level.gain_db() + 9.23).abs() < 0.2, "{:.2} dB", level.gain_db());
        // Nothing was queued again for the broken file.
        assert_eq!(l.work.total.load(Ordering::Acquire), 0);
        // The silent track is known — nothing to level — not "not measured yet".
        let s = open(&silent);
        l.level(&lib, &settings(LevelMode::Track), &silent, &s, WAIT);
        let level = s.level().unwrap();
        assert_eq!((level.basis, level.gain), (Basis::Track, 1.0));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Headroom alone (levelling off, processing on) goes by the album's highest peak: the
    /// quiet track of an album with a loud one is lowered by as much as the loud one, once
    /// the album is measured — which opening any of its tracks sets off.
    #[test]
    fn headroom_alone_lowers_an_album_by_its_highest_peak() {
        let d = tmpdir("headroom");
        let (loud, quiet) = (tone(&d, "01 loud.flac", 1.0), tone(&d, "02 quiet.flac", 0.25));
        let tracks = [&loud, &quiet].map(|p| hifi_engine::library::scan_file(Path::new(p)).unwrap().0).to_vec();
        let (l, lib) = (Loudness::default(), Library::of(tracks));
        let dsp = DspSettings {
            level: LevelSettings { mode: LevelMode::Off, headroom: true },
            upsample: hifi_engine::dsp::Upsample::UpTo(192_000.0),
            ..Default::default()
        };
        // The quiet track first: on its own it has room to spare, and nothing is known of
        // the rest of the album yet.
        let first = open(&quiet);
        l.level(&lib, &dsp, &quiet, &first, WAIT);
        assert_eq!(first.level(), None);
        let deadline = std::time::Instant::now() + WAIT;
        while l.cached(&loud).is_none() {
            assert!(std::time::Instant::now() < deadline, "the rest of the album was never measured");
            std::thread::sleep(Duration::from_millis(20));
        }
        // From then on both are lowered by the loud one's overshoot.
        let (a, b) = (open(&loud), open(&quiet));
        l.level(&lib, &dsp, &loud, &a, WAIT);
        l.level(&lib, &dsp, &quiet, &b, WAIT);
        let (la, lb) = (a.level().expect("the loud track is lowered"), b.level().expect("and the quiet one with it"));
        assert_eq!((la.basis, lb.basis), (Basis::Peak, Basis::Peak));
        assert_eq!(la.gain, lb.gain);
        assert!(la.gain < 1.0 && la.gain > 0.9, "{}", la.gain);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// "Write to tags": what we measured goes into the files, with the album's figure (its
    /// other tracks measured on the way), the files that already carry values keep them, and
    /// from then on the tags are what the tracks play by.
    #[test]
    fn measured_loudness_is_written_into_the_tags() {
        use hifi_engine::tags;
        let d = tmpdir("tags");
        let (loud, quiet, theirs) = (tone(&d, "01 loud.flac", 0.5), tone(&d, "02 quiet.flac", 0.125), tone(&d, "03 theirs.flac", 0.25));
        let foreign = tags::GainTags { track_db: -1.0, track_peak: 0.25, album_db: -2.0, album_peak: 0.5 };
        tags::write_gain(Path::new(&theirs), &foreign).unwrap();
        let tracks = [&loud, &quiet, &theirs].map(|p| hifi_engine::library::scan_file(Path::new(p)).unwrap().0).to_vec();
        let (l, lib) = (Loudness::default(), Library::of(tracks));
        let all = [loud.as_str(), quiet.as_str(), theirs.as_str()];
        // Nothing measured: nothing to write.
        assert!(l.untagged(all.into_iter()).is_empty());
        l.write_tags(&lib);
        assert!(tags::read_gain(Path::new(&loud)).unwrap().is_empty());
        // Two of the three measured (the third as the background analysis would).
        l.level(&lib, &settings(LevelMode::Track), &loud, &open(&loud), WAIT);
        l.store(&theirs, loudness::analyze(Path::new(&theirs)).unwrap());
        assert_eq!(l.untagged(all.into_iter()).len(), 2);
        let audio = std::fs::read(&loud).unwrap().len();
        l.write_tags(&lib);
        let t = &l.work.tagging;
        assert_eq!((t.written.load(Ordering::Acquire), t.failed.load(Ordering::Acquire), t.done.load(Ordering::Acquire)), (1, 0, 2));
        // The loud track: its own gain (−6.02 LUFS to −18) and the album's, over all three.
        let got = tags::read_gain(Path::new(&loud)).unwrap();
        assert!((got.track_db.unwrap() + 11.98).abs() < 0.15, "{got:?}");
        assert!((got.track_peak.unwrap() - 0.5).abs() < 0.01, "{got:?}");
        assert!(got.album_db.unwrap() > got.track_db.unwrap() && got.album_db.unwrap() < -6.0, "{got:?}");
        assert!((got.album_peak.unwrap() - 0.5).abs() < 0.01, "{got:?}");
        assert!(std::fs::read(&loud).unwrap().len() > audio, "the tags are in the file");
        // The quiet one was measured for the album's sake, and is offered next.
        assert!(tags::read_gain(Path::new(&quiet)).unwrap().is_empty());
        assert_eq!(l.untagged(all.into_iter()), vec![quiet.clone()]);
        // Another scanner's values are untouched.
        let kept = tags::read_gain(Path::new(&theirs)).unwrap();
        assert_eq!((kept.track_db, kept.album_db), (Some(-1.0), Some(-2.0)));
        // What we measured still counts for the rewritten file, and playing goes by its tags
        // — the same gain as before.
        assert!(l.cached(&loud).is_some());
        let again = open(&loud);
        l.level(&lib, &settings(LevelMode::Track), &loud, &again, Duration::ZERO);
        assert!((again.level().unwrap().gain_db() + 11.98).abs() < 0.15);
        // A second run takes the rest; then there is nothing left to offer.
        l.write_tags(&lib);
        assert_eq!(t.written.load(Ordering::Acquire), 1);
        assert!(!tags::read_gain(Path::new(&quiet)).unwrap().is_empty());
        assert!(l.untagged(all.into_iter()).is_empty());
        // A file that can't be replaced is counted, left alone, and not offered again.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let locked = tmpdir("tags-locked");
            let p = tone(&locked, "01 locked.flac", 0.5);
            let lib = Library::of(vec![hifi_engine::library::scan_file(Path::new(&p)).unwrap().0]);
            let l = Loudness::default();
            l.store(&p, loudness::analyze(Path::new(&p)).unwrap());
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
            l.write_tags(&lib);
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            let t = &l.work.tagging;
            assert_eq!((t.written.load(Ordering::Acquire), t.failed.load(Ordering::Acquire)), (0, 1));
            assert!(tags::read_gain(Path::new(&p)).unwrap().is_empty());
            assert!(l.untagged([p.as_str()].into_iter()).is_empty());
            let _ = std::fs::remove_dir_all(&locked);
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_album_gets_one_gain_once_all_of_it_is_measured() {
        let d = tmpdir("album");
        let (loud, quiet) = (tone(&d, "01 loud.flac", 0.5), tone(&d, "02 quiet.flac", 0.125));
        let tracks = [&loud, &quiet].map(|p| hifi_engine::library::scan_file(Path::new(p)).unwrap().0).to_vec();
        let (l, lib) = (Loudness::default(), Library::of(tracks));
        assert_eq!(lib.album_of(&loud), vec![loud.clone(), quiet.clone()]);
        let album = settings(LevelMode::Album);
        // The first track opened: the rest of the album isn't measured yet, so it goes by
        // its own loudness — and the rest is measured in the background.
        let first = open(&loud);
        l.level(&lib, &album, &loud, &first, WAIT);
        assert_eq!(first.level().unwrap().basis, Basis::Track);
        let deadline = std::time::Instant::now() + WAIT;
        while l.cached(&quiet).is_none() {
            assert!(std::time::Instant::now() < deadline, "the background analysis never got to it");
            std::thread::sleep(Duration::from_millis(20));
        }
        // From then on both tracks get the album's gain, so the quiet one stays quieter.
        // (−6.02 and −18.06 LUFS, equally long: −8.77 LUFS together.)
        let (a, b) = (open(&loud), open(&quiet));
        l.level(&lib, &album, &loud, &a, WAIT);
        l.level(&lib, &album, &quiet, &b, WAIT);
        let (la, lb) = (a.level().unwrap(), b.level().unwrap());
        assert_eq!((la.basis, lb.basis), (Basis::Album, Basis::Album));
        assert!((la.gain_db() + 9.23).abs() < 0.2, "{:.2} dB", la.gain_db());
        assert_eq!(la.gain, lb.gain);
        // By track, each goes to the target on its own. The quiet one would need +0.06 dB.
        let t = open(&quiet);
        l.level(&lib, &settings(LevelMode::Track), &quiet, &t, WAIT);
        assert!((t.level().unwrap().gain_db() - 0.06).abs() < 0.15);
        // The status the panel shows, and the workers back at rest.
        let deadline = std::time::Instant::now() + WAIT;
        while l.work.total.load(Ordering::Acquire) != 0 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(l.work.done.load(Ordering::Acquire), 0);
        let _ = std::fs::remove_dir_all(&d);
    }
}
