//! Android system integration, over JNI to `PlaybackService.kt`:
//! - a foreground "media playback" service, so Android keeps us running with the screen off;
//! - its media notification / lock-screen player (MediaSession), whose buttons and headphone
//!   keys come back here as [`Java_dev_local_hifi_PlaybackService_nativeCommand`];
//! - the music folders and the moment storage permission is granted (library rescan).
//!
//! Same shape as `ios_media.rs`, so `lib.rs` drives both the same way.

use std::sync::Mutex;
use std::time::Instant;

use jni::objects::{JClass, JObject, JValue};
use jni::sys::{jdouble, jint};
use jni::{JNIEnv, JavaVM};
use tauri::{AppHandle, Manager};

use crate::AppState;

const SERVICE: &str = "dev.local.hifi.PlaybackService";

pub struct TrackInfo {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: f64,
    pub cover: Option<Vec<u8>>,
}

fn with_env<R>(f: impl FnOnce(&mut JNIEnv, &JObject) -> jni::errors::Result<R>) -> Option<R> {
    let ctx = ndk_context::android_context();
    // SAFETY: ndk-context holds the process's JavaVM and a global ref to the app Context.
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let mut env = vm.attach_current_thread_permanently().ok()?;
    let context = unsafe { JObject::from_raw(ctx.context().cast()) };
    let r = env.with_local_frame(32, |env| f(env, &context));
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    r.map_err(|e| log::warn!("JNI: {e}")).ok()
}

/// Our own classes must come from the app's class loader: a native thread only sees the
/// system's.
fn service_class<'a>(env: &mut JNIEnv<'a>, context: &JObject) -> jni::errors::Result<JClass<'a>> {
    let loader = env.call_method(context, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?.l()?;
    let name = env.new_string(SERVICE)?;
    let class = env.call_method(&loader, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", &[JValue::Object(&name)])?.l()?;
    Ok(JClass::from(class))
}

/// The app's languages, best first, as BCP 47 tags ("ru-RU", "zh-Hant-TW"): a language
/// picked for this app in Android's settings comes first, else the system's list.
pub fn locales() -> Vec<String> {
    with_env(|env, ctx| {
        let res = env.call_method(ctx, "getResources", "()Landroid/content/res/Resources;", &[])?.l()?;
        let conf = env.call_method(&res, "getConfiguration", "()Landroid/content/res/Configuration;", &[])?.l()?;
        let list = env.call_method(&conf, "getLocales", "()Landroid/os/LocaleList;", &[])?.l()?;
        let tags = env.call_method(&list, "toLanguageTags", "()Ljava/lang/String;", &[])?.l()?;
        let tags: String = env.get_string(&tags.into())?.into();
        Ok(tags.split(',').map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect())
    })
    .unwrap_or_default()
}

/// The app's cache folder (downloaded updates go there).
#[cfg(feature = "updater")]
pub fn cache_dir() -> Option<std::path::PathBuf> {
    with_env(|env, ctx| {
        let dir = env.call_method(ctx, "getCacheDir", "()Ljava/io/File;", &[])?.l()?;
        let p = env.call_method(&dir, "getAbsolutePath", "()Ljava/lang/String;", &[])?.l()?;
        let p: String = env.get_string(&p.into())?.into();
        Ok(std::path::PathBuf::from(p))
    })
}

/// Open Android's installer for the APK at `path`. False: the app may not install apps yet,
/// and Android's settings page for that was opened instead.
#[cfg(feature = "updater")]
pub fn install_apk(path: &str) -> bool {
    with_env(|env, ctx| {
        let loader = env.call_method(ctx, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?.l()?;
        let name = env.new_string("dev.local.hifi.Updater")?;
        let class = env.call_method(&loader, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", &[JValue::Object(&name)])?.l()?;
        let path = env.new_string(path)?;
        env.call_static_method(
            JClass::from(class),
            "install",
            "(Landroid/content/Context;Ljava/lang/String;)Z",
            &[JValue::Object(ctx), JValue::Object(&path)],
        )?
        .z()
    })
    .unwrap_or(false)
}

/// The folders the library scans: the shared Music folder (needs the audio permission) and the
/// app's own one under Android/data (always readable, reachable over USB).
pub fn music_dirs() -> Vec<String> {
    with_env(|env, ctx| {
        let mut out = Vec::new();
        let music = env.get_static_field("android/os/Environment", "DIRECTORY_MUSIC", "Ljava/lang/String;")?.l()?;
        let public = env
            .call_static_method(
                "android/os/Environment",
                "getExternalStoragePublicDirectory",
                "(Ljava/lang/String;)Ljava/io/File;",
                &[JValue::Object(&music)],
            )?
            .l()?;
        let own = env.call_method(ctx, "getExternalFilesDir", "(Ljava/lang/String;)Ljava/io/File;", &[JValue::Object(&music)])?.l()?;
        for f in [public, own] {
            if f.is_null() {
                continue;
            }
            let p = env.call_method(&f, "getAbsolutePath", "()Ljava/lang/String;", &[])?.l()?;
            out.push(env.get_string(&p.into())?.into());
        }
        Ok(out)
    })
    .unwrap_or_default()
}

struct Published {
    rev: u64,
    paused: bool,
    position: f64,
    at: Instant,
}

static PUBLISHED: Mutex<Option<Published>> = Mutex::new(None);

/// A new track started: show it in the media notification (starting the service if needed).
pub fn set_track(_app: &AppHandle, rev: u64, t: TrackInfo) {
    with_env(|env, ctx| {
        let class = service_class(env, ctx)?;
        let title = env.new_string(&t.title)?;
        let artist: JObject = match &t.artist {
            Some(a) => env.new_string(a)?.into(),
            None => JObject::null(),
        };
        let album: JObject = match &t.album {
            Some(a) => env.new_string(a)?.into(),
            None => JObject::null(),
        };
        let cover: JObject = match &t.cover {
            Some(b) => env.byte_array_from_slice(b)?.into(),
            None => JObject::null(),
        };
        env.call_static_method(
            &class,
            "update",
            "(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;J[B)V",
            &[
                JValue::Object(ctx),
                JValue::Object(&title),
                JValue::Object(&artist),
                JValue::Object(&album),
                JValue::Long((t.duration * 1000.0) as i64),
                JValue::Object(&cover),
            ],
        )?;
        Ok(())
    });
    *PUBLISHED.lock().unwrap() = Some(Published { rev, paused: false, position: 0.0, at: Instant::now() });
    progress(0.0, true);
}

fn progress(position: f64, playing: bool) {
    with_env(|env, ctx| {
        let class = service_class(env, ctx)?;
        env.call_static_method(
            &class,
            "progress",
            "(Landroid/content/Context;JZ)V",
            &[JValue::Object(ctx), JValue::Long((position * 1000.0) as i64), JValue::Bool(playing.into())],
        )?;
        Ok(())
    });
}

/// Nothing is loaded any more: drop the notification and the foreground service.
pub fn clear(_app: &AppHandle) {
    *PUBLISHED.lock().unwrap() = None;
    with_env(|env, ctx| {
        let class = service_class(env, ctx)?;
        env.call_static_method(&class, "clear", "(Landroid/content/Context;)V", &[JValue::Object(ctx)])?;
        Ok(())
    });
}

/// Called from the queue thread every tick: resend position and state when the user paused,
/// resumed or seeked (Android runs the clock on from the last update by itself).
pub fn sync(_app: &AppHandle, rev: u64, position: f64, paused: bool) {
    let push = {
        let mut g = PUBLISHED.lock().unwrap();
        match g.as_mut() {
            Some(p) if p.rev == rev => {
                let expected = if p.paused { p.position } else { p.position + p.at.elapsed().as_secs_f64() };
                if p.paused != paused || (position - expected).abs() > 1.0 {
                    p.paused = paused;
                    p.position = position;
                    p.at = Instant::now();
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    };
    if push {
        progress(position, !paused);
    }
}

pub fn install(_app: &AppHandle) {}

/// Buttons in the media notification, on the lock screen, on headphones.
/// 0 toggle, 1 play, 2 pause, 3 next, 4 previous, 5 seek to `arg` seconds.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_local_hifi_PlaybackService_nativeCommand(_env: JNIEnv, _class: JClass, cmd: jint, arg: jdouble) {
    let Some(app) = crate::APP.get() else { return };
    let app = app.clone();
    // Track changes open files: never on the UI thread that called us.
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        match cmd {
            0..=2 => {
                if let Some(p) = state.player.lock().unwrap().as_ref() {
                    p.set_paused(match cmd {
                        1 => false,
                        2 => true,
                        _ => !p.is_paused(),
                    });
                }
            }
            3 => crate::jump(&state, 1),
            4 => crate::jump(&state, -1),
            5 => {
                if let Some(p) = state.player.lock().unwrap().as_ref() {
                    p.seek(arg);
                }
            }
            _ => {}
        }
    });
}

/// The user granted access to the shared Music folder: scan it now.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_local_hifi_MainActivity_nativeStorageGranted(_env: JNIEnv, _class: JClass) {
    if let Some(app) = crate::APP.get() {
        app.state::<crate::library::Library>().rescan_now(app.clone());
    }
}
