fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    // Lock-screen / Now Playing player and remote commands (src/apple_media.rs).
    if os == "ios" || os == "macos" {
        println!("cargo:rustc-link-lib=framework=MediaPlayer");
    }
    // `system_media`: the OS has a system player (lock screen, Control Center, media keys)
    // that `media::` drives — every platform we ship on.
    println!("cargo::rustc-check-cfg=cfg(system_media)");
    if matches!(os.as_str(), "ios" | "android" | "macos" | "windows" | "linux") {
        println!("cargo:rustc-cfg=system_media");
    }
    // Android 15+ devices with 16 KB memory pages load only libraries aligned to them, and
    // Google Play rejects apps targeting Android 15+ without it. NDK 27 still links at 4 KB.
    if os == "android" {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-z,max-page-size=16384");
    }
    // The app's Last.fm API key and secret (src/scrobble.rs) come from the environment of the
    // build, never from the repository; without them the build has no Last.fm.
    println!("cargo:rerun-if-env-changed=BITSPRING_LASTFM_KEY");
    println!("cargo:rerun-if-env-changed=BITSPRING_LASTFM_SECRET");
    tauri_build::build()
}
