//! Which Claude Code session was active when: an index over the local
//! transcripts in `~/.claude/projects/*/*.jsonl`.
//!
//! ActivityWatch only sees the window title "Claude", so chat and the
//! different Code sessions look identical. Each transcript line, however,
//! carries a `timestamp`, the session's `cwd` and its `sessionId`, and a
//! `custom-title` line names the session. Only those fields are read;
//! message content is never parsed into memory or stored.
//!
//! Files are read incrementally (byte offset per file), so a refresh every
//! poll only parses newly appended lines.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// One timestamped transcript line.
#[derive(Debug, Clone)]
pub struct CodeEvent {
    pub t: DateTime<Utc>,
    pub session: String,
    pub cwd: String,
}

/// Only the fields we need; serde skips everything else (content included).
#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
}

#[derive(Default)]
pub struct CodeIndex {
    offsets: HashMap<PathBuf, u64>,
    events: Vec<CodeEvent>,
    titles: HashMap<String, String>,
    since: Option<DateTime<Utc>>,
}

fn root() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude/projects")
}

impl CodeIndex {
    /// Pick up lines appended since the last call, keeping events at or
    /// after `since`. Changing `since` (a new day) rebuilds the index.
    pub fn refresh(&mut self, since: DateTime<Utc>) {
        if self.since != Some(since) {
            *self = Self { since: Some(since), ..Self::default() };
        }
        let cutoff = SystemTime::from(since);
        let Ok(dirs) = std::fs::read_dir(root()) else { return };
        let mut added = false;
        for dir in dirs.flatten() {
            let Ok(files) = std::fs::read_dir(dir.path()) else { continue };
            for f in files.flatten() {
                let path = f.path();
                if path.extension().is_none_or(|e| e != "jsonl") {
                    continue;
                }
                // Only transcripts touched since `since` can hold events.
                let Ok(meta) = f.metadata() else { continue };
                if meta.modified().is_ok_and(|m| m < cutoff) {
                    continue;
                }
                added |= self.read_new(&path, meta.len(), since);
            }
        }
        if added {
            self.events.sort_by_key(|e| e.t);
        }
    }

    /// Parse complete lines after the stored offset. Returns whether any
    /// event was added.
    fn read_new(&mut self, path: &PathBuf, len: u64, since: DateTime<Utc>) -> bool {
        let offset = self.offsets.get(path).copied().unwrap_or(0);
        if len <= offset {
            return false;
        }
        let Ok(mut file) = File::open(path) else { return false };
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return false;
        }
        // Stream line by line: transcripts reach hundreds of MB, so only
        // one line is held in memory at a time.
        let mut reader = BufReader::with_capacity(64 * 1024, file);
        let (mut raw, mut consumed, mut added) = (Vec::new(), offset, false);
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            // A half-written last line is retried on the next refresh.
            if raw.last() != Some(&b'\n') {
                break;
            }
            consumed += raw.len() as u64;
            added |= self.parse_line(&raw[..raw.len() - 1], since);
        }
        self.offsets.insert(path.clone(), consumed);
        added
    }

    /// Record one transcript line. Returns whether an event was added.
    fn parse_line(&mut self, raw: &[u8], since: DateTime<Utc>) -> bool {
        // Cheap prefilter before JSON parsing.
        let interesting = memchr_contains(raw, b"\"timestamp\"") || memchr_contains(raw, b"customTitle");
        if !interesting {
            return false;
        }
        let Ok(line) = serde_json::from_slice::<Line>(raw) else { return false };
        if line.kind.as_deref() == Some("custom-title") {
            if let (Some(id), Some(title)) = (line.session_id, line.custom_title) {
                self.titles.insert(id, title);
            }
            return false;
        }
        let (Some(ts), Some(cwd), Some(session)) = (line.timestamp, line.cwd, line.session_id) else {
            return false;
        };
        let Ok(t) = DateTime::parse_from_rfc3339(&ts) else { return false };
        let t = t.with_timezone(&Utc);
        if t < since {
            return false;
        }
        self.events.push(CodeEvent { t, session, cwd });
        true
    }

    /// Events with `start <= t <= end` (binary search; O(log n + k)).
    pub fn between(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> &[CodeEvent] {
        let lo = self.events.partition_point(|e| e.t < start);
        let hi = self.events.partition_point(|e| e.t <= end);
        &self.events[lo..hi.max(lo)]
    }

    /// Human name of a session: its title, else the cwd's last component.
    pub fn name(&self, ev: &CodeEvent) -> String {
        self.titles.get(&ev.session).cloned().unwrap_or_else(|| {
            ev.cwd.rsplit('/').next().unwrap_or(&ev.cwd).to_owned()
        })
    }

    #[cfg(test)]
    pub fn from_events(mut events: Vec<CodeEvent>, titles: HashMap<String, String>) -> Self {
        events.sort_by_key(|e| e.t);
        Self { events, titles, ..Self::default() }
    }
}

fn memchr_contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}
