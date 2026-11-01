//! Link settings for macOS.
//!
//! * CoreAudio is linked weakly. cpal 0.18 references
//!   `AudioHardwareCreateProcessTap` / `AudioHardwareDestroyProcessTap`
//!   (macOS 14.2+) for its loopback-capture feature, which aw-timer never
//!   uses. A strong reference would make dyld refuse to launch the binary on
//!   older macOS; a weak one resolves to null there and is never called.
//! * `Info.plist` is embedded in the binary (`__TEXT,__info_plist`). A bare
//!   executable has no bundle, and macOS only shows the Calendar permission
//!   prompt when the requesting binary declares a usage description.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-weak_framework,CoreAudio");
        let plist = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default()).join("Info.plist");
        println!("cargo:rerun-if-changed={}", plist.display());
        println!("cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}", plist.display());
    }
}
