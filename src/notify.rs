//! macOS notifications.
//!
//! When aw-timer runs from its app bundle (`aw-timer.app`, made by
//! `install.sh`), notifications go through UserNotifications, so clicking one
//! opens the report window (see `menubar`). Outside a bundle (`cargo run`, a
//! Homebrew install) UserNotifications is unavailable and they fall back to
//! osascript, whose notifications open Script Editor when clicked.

use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_foundation::{NSBundle, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest, UNUserNotificationCenter,
};

/// Set once the user has allowed aw-timer's notifications.
static AUTHORIZED: AtomicBool = AtomicBool::new(false);

/// Text goes through argv, never into AppleScript source, so window titles
/// containing quotes cannot break or inject into the script.
const SCRIPT: &[&str] = &[
    "on run argv",
    "if item 3 of argv is \"\" then",
    "display notification (item 2 of argv) with title (item 1 of argv)",
    "else",
    "display notification (item 2 of argv) with title (item 1 of argv) sound name (item 3 of argv)",
    "end if",
    "end run",
];

/// Whether this process runs from an `.app` bundle.
pub fn bundled() -> bool {
    static BUNDLED: OnceLock<bool> = OnceLock::new();
    *BUNDLED.get_or_init(|| NSBundle::mainBundle().bundlePath().to_string().ends_with(".app"))
}

/// Ask for permission to notify (shows the system prompt once). Call at
/// startup from the app bundle; elsewhere it does nothing.
pub fn request_authorization() {
    if !bundled() {
        return;
    }
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let done = RcBlock::new(|granted: Bool, _err: *mut NSError| {
        AUTHORIZED.store(granted.as_bool(), Ordering::Relaxed);
    });
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
        &done,
    );
}

/// Show a notification; failures are ignored (best effort).
pub fn notify(title: &str, message: &str, sound: &str) {
    if bundled() && AUTHORIZED.load(Ordering::Relaxed) {
        post(title, message);
        if !sound.is_empty() {
            play(sound);
        }
        return;
    }
    let mut cmd = Command::new("osascript");
    for line in SCRIPT {
        cmd.args(["-e", line]);
    }
    let _ = cmd
        .args([title, message, sound])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn post(title: &str, message: &str) {
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(message));
    // A unique identifier, so notifications don't replace one another.
    let id = format!("aw-timer-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&NSString::from_str(&id), &content, None);
    UNUserNotificationCenter::currentNotificationCenter().addNotificationRequest_withCompletionHandler(&request, None);
}

/// Play a system sound by name (e.g. "Glass") without blocking.
/// UserNotifications only plays sounds shipped inside the app bundle, so the
/// macOS system sounds are played here instead.
fn play(name: &str) {
    // Only plain names: the value comes from the config file.
    if !name.chars().all(|c| c.is_alphanumeric() || c == ' ' || c == '-' || c == '_') {
        return;
    }
    let path = format!("/System/Library/Sounds/{name}.aiff");
    let _ = Command::new("afplay").arg(path).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}
