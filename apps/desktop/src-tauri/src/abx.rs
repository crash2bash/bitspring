//! The blind test (ABX) for the app: see hifi_engine::abx for what is compared and how the
//! two are prepared. Here: one session at a time, which holds the two prepared tracks, keeps
//! what X is to itself until the last answer, and plays A, B or X on the chosen output.
//!
//! While a test runs it has the player: the queue stands still (as during the channel
//! check), and nothing is shown as playing. Closing the test stops its sound; the queue is
//! as it was, and the window starts what the test interrupted again from where it was.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use hifi_engine::abx::{self, Prepared};
use hifi_engine::dsp::upsample::FilterSpec;
use hifi_engine::output::PlayOptions;
use hifi_engine::player::Player;
use hifi_engine::{Track, UserError};
use serde::{Deserialize, Serialize};
use tauri::{Manager, State};

use crate::{AppState, CmdResult};

#[derive(Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Which {
    A,
    B,
    X,
}

/// One answered trial, told only when the test is over.
#[derive(Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Trial {
    x_is_a: bool,
    said_a: bool,
}

struct Session {
    a: Arc<Track>,
    b: Arc<Track>,
    device_uid: String,
    exclusive: bool,
    /// What X is in the trial being listened to.
    x_is_a: bool,
    listening: Which,
    total: u32,
    answers: Vec<Trial>,
}

impl Session {
    fn track(&self, which: Which) -> &Arc<Track> {
        match which {
            Which::A => &self.a,
            Which::B => &self.b,
            Which::X if self.x_is_a => &self.a,
            Which::X => &self.b,
        }
    }

    fn result(&self) -> Option<AbxResult> {
        (self.answers.len() as u32 >= self.total).then(|| {
            let correct = self.answers.iter().filter(|t| t.x_is_a == t.said_a).count() as u32;
            AbxResult { correct, total: self.total, p: abx::p_value(correct, self.total), trials: self.answers.clone() }
        })
    }
}

#[derive(Default)]
pub struct Abx {
    session: Mutex<Option<Session>>,
    /// Counts the starts and the stops: a test closed while it was still being prepared
    /// finds another number here and leaves what is playing alone.
    asked: AtomicU64,
}

/// A fair coin, from the OS's random keys for hash maps.
fn coin() -> bool {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new().build_hasher().finish() & 1 == 1
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AbxInfo {
    /// What was done to make the two comparable.
    prepared: Prepared,
    trials: u32,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AbxResult {
    correct: u32,
    total: u32,
    /// The chance of doing at least this well by guessing.
    p: f64,
    trials: Vec<Trial>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AbxProgress {
    done: u32,
    total: u32,
    /// Set with the last answer.
    result: Option<AbxResult>,
}

const TRIALS: std::ops::RangeInclusive<u32> = 5..=40;

/// Play what the session is listening to. Within the player that already plays one of the
/// two: a swap at the same position. Otherwise (the first time, after the music ran out, or
/// after something else took the player) from `at` seconds.
fn play(state: &AppState, s: &Session, at: f64) -> CmdResult<()> {
    let track = s.track(s.listening).clone();
    let _one = state.play_lock.lock().unwrap();
    {
        let player = state.player.lock().unwrap();
        if let Some(p) = player.as_ref() {
            let heard = p.heard();
            let ours = Arc::ptr_eq(&heard, &s.a) || Arc::ptr_eq(&heard, &s.b);
            if ours && !p.is_finished() && p.swap(track.clone()) {
                return Ok(());
            }
        }
    }
    let device = crate::find_device(&s.device_uid)?;
    // Like the channel check: the queue has nothing to follow while the test has the player.
    *state.current.lock().unwrap() = None;
    let opts = PlayOptions { exclusive: s.exclusive, start_at: at, fades: true, ..Default::default() };
    let mut slot = state.player.lock().unwrap();
    if let Some(p) = slot.take() {
        p.fade_out();
    }
    *slot = Some(Player::start(&device, track, &opts).map_err(crate::err)?);
    Ok(())
}

/// Prepare a test and start playing A. `kind`: "files" (`a` against `b`, two files of the
/// same music), "processing" (`a` as it is against `a` through the processing that is
/// switched on for this output) or "filters" (`a`, the track playing, upsampled to the rate
/// the DAC runs at with the standard filter and with the long one playing it). Takes a
/// while: both are decoded whole (and upsampled), off the IPC thread.
#[tauri::command]
pub async fn abx_start(
    app: tauri::AppHandle,
    kind: String,
    a: String,
    b: Option<String>,
    device_uid: String,
    exclusive: bool,
    trials: Option<u32>,
) -> CmdResult<AbxInfo> {
    let state = app.state::<AppState>();
    let abx_state = app.state::<Abx>();
    let mine = abx_state.asked.fetch_add(1, Ordering::AcqRel) + 1;
    let device = crate::find_device(&device_uid)?;
    if device.is_network() {
        return Err(UserError::AbxNetwork);
    }
    if crate::stream::is_uri(&a) || b.as_deref().is_some_and(crate::stream::is_uri) {
        return Err(UserError::AbxNotFile);
    }
    // (Rendered here, not played through the device: no DSD output.)
    let dsp = crate::dsp_offline(&state, device.uid());
    let filters = if kind == "filters" { Some(playing_filter(&state, &a, &device_uid)?) } else { None };
    let pair = tauri::async_runtime::spawn_blocking(move || match (kind.as_str(), b, filters) {
        ("files", Some(b), _) => abx::of_files(&PathBuf::from(a), &PathBuf::from(b)),
        // A: the standard filter; B: the long one, as it plays now.
        ("filters", _, Some((rate, long))) => abx::of_filters(&PathBuf::from(a), rate, None, Some(long)),
        _ => abx::of_processing(&PathBuf::from(a), &dsp),
    })
    .await
    .map_err(crate::err)?
    .map_err(crate::err)?;
    let total = trials.unwrap_or(16).clamp(*TRIALS.start(), *TRIALS.end());
    let session = Session { a: pair.a, b: pair.b, device_uid, exclusive, x_is_a: coin(), listening: Which::A, total, answers: Vec::new() };
    // From here to the session being kept, a stop waits: it finds either nothing or all of it.
    let mut slot = abx_state.session.lock().unwrap();
    // The window was closed while the two were being prepared (nobody reads this answer):
    // what is playing goes on.
    if abx_state.asked.load(Ordering::Acquire) != mine {
        return Ok(AbxInfo { prepared: pair.prepared, trials: total });
    }
    // What was playing ends here: nothing is shown as playing while the test runs.
    crate::stop(app.state::<AppState>());
    state.now_rev.fetch_add(1, Ordering::AcqRel);
    // (Two identical tracks: there is nothing to listen for, and the window says so.)
    if !pair.prepared.identical {
        play(&state, &session, 0.0)?;
    }
    *slot = Some(session);
    Ok(AbxInfo { prepared: pair.prepared, trials: total })
}

/// The rate the DAC runs at and the long filter, for filter against filter: taken from what
/// plays now (not from the settings, which may have changed since, or been stepped down), so
/// the test is about what was just heard, and the DAC stays at its rate. Only for `path`
/// playing on `device_uid` behind a long filter.
fn playing_filter(state: &AppState, path: &str, device_uid: &str) -> CmdResult<(u32, FilterSpec)> {
    let ours = state.current.lock().unwrap().as_ref().is_some_and(|c| c.path.as_os_str() == path && c.device_uid == device_uid);
    let playing = state.player.lock().unwrap().as_ref().filter(|p| !p.is_finished()).and_then(|p| Some((p.path.device_rate.round() as u32, p.path.upsample_filter?)));
    match playing {
        Some(found) if ours => Ok(found),
        _ => Err(UserError::AbxNoFilter),
    }
}

/// Listen to A, B or X — from where the music is, or from its start once it has run out.
#[tauri::command(async)]
pub fn abx_listen(state: State<'_, AppState>, abx: State<'_, Abx>, which: Which) -> CmdResult<()> {
    let mut session = abx.session.lock().unwrap();
    let Some(s) = session.as_mut() else { return Ok(()) };
    s.listening = which;
    play(&state, s, 0.0)
}

/// How many trials the test has: only before the first answer.
#[tauri::command]
pub fn abx_set_trials(abx: State<'_, Abx>, trials: u32) -> u32 {
    let mut session = abx.session.lock().unwrap();
    match session.as_mut() {
        Some(s) => {
            if s.answers.is_empty() {
                s.total = trials.clamp(*TRIALS.start(), *TRIALS.end());
            }
            s.total
        }
        None => trials,
    }
}

/// "X is A" (or B). The next trial gets an X of its own; whether the answer was right is
/// told only with the last one.
#[tauri::command(async)]
pub fn abx_answer(state: State<'_, AppState>, abx: State<'_, Abx>, is_a: bool) -> CmdResult<AbxProgress> {
    let mut session = abx.session.lock().unwrap();
    let Some(s) = session.as_mut() else { return Ok(AbxProgress { done: 0, total: 0, result: None }) };
    if (s.answers.len() as u32) < s.total {
        s.answers.push(Trial { x_is_a: s.x_is_a, said_a: is_a });
        s.x_is_a = coin();
        // Listening to X: on with the new X. (The switch sounds the same whether or not it
        // is another track: see `Playback::swap`.)
        if s.listening == Which::X && (s.answers.len() as u32) < s.total {
            play(&state, s, 0.0)?;
        }
    }
    Ok(AbxProgress { done: s.answers.len() as u32, total: s.total, result: s.result() })
}

/// The same two again, from the first trial.
#[tauri::command(async)]
pub fn abx_again(state: State<'_, AppState>, abx: State<'_, Abx>) -> CmdResult<()> {
    let mut session = abx.session.lock().unwrap();
    let Some(s) = session.as_mut() else { return Ok(()) };
    s.answers.clear();
    s.x_is_a = coin();
    s.listening = Which::A;
    play(&state, s, 0.0)
}

/// End the test: the sound stops, the prepared tracks are let go.
#[tauri::command(async)]
pub fn abx_stop(state: State<'_, AppState>, abx: State<'_, Abx>) {
    abx.asked.fetch_add(1, Ordering::AcqRel);
    let Some(s) = abx.session.lock().unwrap().take() else { return };
    let _one = state.play_lock.lock().unwrap();
    let mut player = state.player.lock().unwrap();
    // (Unless something else has started playing meanwhile.)
    if player.as_ref().is_some_and(|p| {
        let heard = p.heard();
        Arc::ptr_eq(&heard, &s.a) || Arc::ptr_eq(&heard, &s.b)
    }) && let Some(p) = player.take()
    {
        p.fade_out();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hifi_engine::TrackMeta;

    fn session(total: u32) -> Session {
        let track = || Arc::new(Track::from_samples(44_100, 2, Some(16), &[0.0; 64], TrackMeta::default()));
        Session { a: track(), b: track(), device_uid: String::new(), exclusive: true, x_is_a: true, listening: Which::A, total, answers: Vec::new() }
    }

    #[test]
    fn x_is_one_of_the_two_and_the_result_waits_for_the_last_answer() {
        let mut s = session(3);
        assert!(Arc::ptr_eq(s.track(Which::X), &s.a) && Arc::ptr_eq(s.track(Which::A), &s.a) && Arc::ptr_eq(s.track(Which::B), &s.b));
        s.x_is_a = false;
        assert!(Arc::ptr_eq(s.track(Which::X), &s.b));
        s.answers.push(Trial { x_is_a: true, said_a: true });
        s.answers.push(Trial { x_is_a: false, said_a: true });
        assert!(s.result().is_none(), "nothing is told before the last answer");
        s.answers.push(Trial { x_is_a: false, said_a: false });
        let r = s.result().unwrap();
        assert_eq!((r.correct, r.total), (2, 3));
        assert!((r.p - 0.5).abs() < 1e-12);
    }

    /// Filter against filter takes its rate and filter from what plays: with nothing playing
    /// there is nothing to take them from.
    #[test]
    fn filters_need_a_long_filter_playing() {
        let state = AppState::default();
        assert_eq!(playing_filter(&state, "/music/a.flac", "dac").unwrap_err(), UserError::AbxNoFilter);
    }

    #[test]
    fn the_coin_lands_both_ways() {
        let heads = (0..400).filter(|_| coin()).count();
        assert!((120..=280).contains(&heads), "{heads} of 400");
    }
}
