//! Link CoreAudio weakly.
//!
//! cpal 0.18 references `AudioHardwareCreateProcessTap` /
//! `AudioHardwareDestroyProcessTap` (macOS 14.2+) for its loopback-capture
//! feature, which aw-timer never uses. A strong reference would make dyld
//! refuse to launch the binary on older macOS; a weak one resolves to null
//! there and is simply never called.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-weak_framework,CoreAudio");
    }
}
