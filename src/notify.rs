//! macOS notifications via osascript.

use std::process::{Command, Stdio};

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

/// Show a notification; failures are ignored (best effort).
pub fn notify(title: &str, message: &str, sound: &str) {
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
