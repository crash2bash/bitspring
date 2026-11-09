fn main() {
    // Lock-screen player and remote commands (src/ios_media.rs).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {
        println!("cargo:rustc-link-lib=framework=MediaPlayer");
    }
    tauri_build::build()
}
