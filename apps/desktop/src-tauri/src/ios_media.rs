//! iPhone system integration: the lock screen / Control Center player ("Now Playing") and the
//! remote commands that come from it, from headphone buttons and from CarPlay or a watch.
//!
//! The commands act on the same state as the UI's buttons, so the UI just sees the change on
//! its next status poll.

use std::ptr::NonNull;
use std::sync::Mutex;
use std::time::Instant;

use block2::RcBlock;
use objc2::encode::{Encode, Encoding, RefEncode};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{AnyThread, msg_send};
use objc2_foundation::{NSData, NSMutableDictionary, NSNumber, NSString};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPMediaItemArtwork, MPMediaItemPropertyAlbumTitle,
    MPMediaItemPropertyArtist, MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration,
    MPMediaItemPropertyTitle, MPNowPlayingInfoCenter, MPNowPlayingInfoPropertyElapsedPlaybackTime,
    MPNowPlayingInfoPropertyPlaybackRate, MPRemoteCommandCenter, MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
};
use tauri::{AppHandle, Manager};

use crate::AppState;

/// What the lock screen shows for the current track.
pub struct TrackInfo {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: f64,
    pub cover: Option<Vec<u8>>,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}
// SAFETY: matches CoreGraphics' CGSize on 64-bit Apple platforms.
unsafe impl Encode for CGSize {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}
unsafe impl RefEncode for CGSize {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

/// The last track info we published, kept so play/pause/seek updates can resend it whole.
struct Published {
    info: Retained<NSMutableDictionary<NSString, AnyObject>>,
    rev: u64,
    paused: bool,
    /// Position we last told iOS, and when: iOS runs the clock on from there by itself.
    position: f64,
    at: Instant,
}

// SAFETY: only touched on the main thread (see `on_main`).
unsafe impl Send for Published {}

static PUBLISHED: Mutex<Option<Published>> = Mutex::new(None);

fn on_main(app: &AppHandle, f: impl FnOnce() + Send + 'static) {
    let _ = app.run_on_main_thread(f);
}

fn number(v: f64) -> Retained<AnyObject> {
    // NSNumber → NSValue → NSObject → AnyObject
    Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::new_f64(v))))
}

fn string(s: &str) -> Retained<AnyObject> {
    Retained::into_super(Retained::into_super(NSString::from_str(s)))
}

fn artwork(bytes: &[u8]) -> Option<Retained<AnyObject>> {
    let data = NSData::with_bytes(bytes);
    let class = AnyClass::get(c"UIImage")?;
    // SAFETY: +[UIImage imageWithData:] returns an autoreleased image or nil.
    let image: Option<Retained<AnyObject>> = unsafe { msg_send![class, imageWithData: &*data] };
    let image = image?;
    // SAFETY: plain property read on a live UIImage.
    let size: CGSize = unsafe { msg_send![&*image, size] };
    let handler = RcBlock::new(move |_: CGSize| -> NonNull<AnyObject> { NonNull::from(&*image) });
    // SAFETY: -[MPMediaItemArtwork initWithBoundsSize:requestHandler:] (iOS 10+); the block
    // keeps the image alive and returns it for any requested size.
    let art: Retained<MPMediaItemArtwork> =
        unsafe { msg_send![MPMediaItemArtwork::alloc(), initWithBoundsSize: size, requestHandler: &*handler] };
    Some(Retained::into_super(Retained::into_super(art)))
}

/// A new track started.
pub fn set_track(app: &AppHandle, rev: u64, t: TrackInfo) {
    on_main(app, move || {
        let info = NSMutableDictionary::<NSString, AnyObject>::new();
        // SAFETY: the keys are MediaPlayer's constant strings.
        unsafe {
            info.insert(MPMediaItemPropertyTitle, &*string(&t.title));
            if let Some(a) = &t.artist {
                info.insert(MPMediaItemPropertyArtist, &*string(a));
            }
            if let Some(a) = &t.album {
                info.insert(MPMediaItemPropertyAlbumTitle, &*string(a));
            }
            info.insert(MPMediaItemPropertyPlaybackDuration, &*number(t.duration));
            if let Some(art) = t.cover.as_deref().and_then(artwork) {
                info.insert(MPMediaItemPropertyArtwork, &*art);
            }
        }
        let mut g = PUBLISHED.lock().unwrap();
        *g = Some(Published { info, rev, paused: false, position: 0.0, at: Instant::now() });
        publish(g.as_ref().unwrap());
    });
}

/// Nothing is loaded any more.
pub fn clear(app: &AppHandle) {
    on_main(app, || {
        *PUBLISHED.lock().unwrap() = None;
        // SAFETY: plain property write on the shared center.
        unsafe { MPNowPlayingInfoCenter::defaultCenter().setNowPlayingInfo(None) };
    });
}

fn publish(p: &Published) {
    // SAFETY: the keys are MediaPlayer's constant strings; the center is a singleton.
    unsafe {
        p.info.insert(MPNowPlayingInfoPropertyElapsedPlaybackTime, &*number(p.position));
        p.info.insert(MPNowPlayingInfoPropertyPlaybackRate, &*number(if p.paused { 0.0 } else { 1.0 }));
        MPNowPlayingInfoCenter::defaultCenter().setNowPlayingInfo(Some(&p.info));
    }
}

/// Called from the queue thread every tick: resend position and rate when the user paused,
/// resumed or seeked (iOS extrapolates the clock in between on its own).
pub fn sync(app: &AppHandle, rev: u64, position: f64, paused: bool) {
    let stale = {
        let g = PUBLISHED.lock().unwrap();
        g.as_ref().is_some_and(|p| {
            let expected = if p.paused { p.position } else { p.position + p.at.elapsed().as_secs_f64() };
            p.rev == rev && (p.paused != paused || (position - expected).abs() > 1.0)
        })
    };
    if stale {
        on_main(app, move || {
            let mut g = PUBLISHED.lock().unwrap();
            if let Some(p) = g.as_mut().filter(|p| p.rev == rev) {
                p.paused = paused;
                p.position = position;
                p.at = Instant::now();
                publish(p);
            }
        });
    }
}

type Handler = RcBlock<dyn Fn(NonNull<MPRemoteCommandEvent>) -> MPRemoteCommandHandlerStatus>;

fn handler(app: &AppHandle, f: impl Fn(&AppHandle, &MPRemoteCommandEvent) + 'static) -> Handler {
    let app = app.clone();
    RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
        // SAFETY: MediaPlayer passes a live event for the duration of the call.
        f(&app, unsafe { event.as_ref() });
        MPRemoteCommandHandlerStatus::Success
    })
}

/// Hook up the remote commands. Call once, on the main thread.
pub fn install(app: &AppHandle) {
    let paused = |app: &AppHandle, value: Option<bool>| {
        let state = app.state::<AppState>();
        if let Some(p) = state.player.lock().unwrap().as_ref() {
            p.set_paused(value.unwrap_or(!p.is_paused()));
        }
    };
    // Track changes open files: off the main thread.
    let jump = |app: &AppHandle, delta: i64| {
        let app = app.clone();
        std::thread::spawn(move || crate::jump(&app.state::<AppState>(), delta));
    };
    // SAFETY: the shared command center is a singleton; handlers are retained by it.
    unsafe {
        let c = MPRemoteCommandCenter::sharedCommandCenter();
        c.playCommand().addTargetWithHandler(&handler(app, move |a, _| paused(a, Some(false))));
        c.pauseCommand().addTargetWithHandler(&handler(app, move |a, _| paused(a, Some(true))));
        c.togglePlayPauseCommand().addTargetWithHandler(&handler(app, move |a, _| paused(a, None)));
        c.nextTrackCommand().addTargetWithHandler(&handler(app, move |a, _| jump(a, 1)));
        c.previousTrackCommand().addTargetWithHandler(&handler(app, move |a, _| jump(a, -1)));
        c.changePlaybackPositionCommand().addTargetWithHandler(&handler(app, |a, e| {
            let e: &MPChangePlaybackPositionCommandEvent = &*(e as *const _ as *const MPChangePlaybackPositionCommandEvent);
            if let Some(p) = a.state::<AppState>().player.lock().unwrap().as_ref() {
                p.seek(e.positionTime());
            }
        }));
    }
}
