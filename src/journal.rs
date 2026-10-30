//! Append-only journal of user actions: forced rests and focus goals.
//!
//! Stored as JSON lines in `~/.local/share/aw-timer/journal.jsonl`. The menu
//! bar appends; the tracker and report read it.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;

use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};

use crate::config::minutes;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    /// Start a forced rest; `minutes = None` means until ended manually.
    RestStart {
        t: DateTime<Utc>,
        #[serde(default)]
        minutes: Option<f64>,
    },
    RestEnd { t: DateTime<Utc> },
    /// Set the focus goal; an empty `text` clears it.
    Goal { t: DateTime<Utc>, text: String },
    /// Set the current project manually; an empty `name` clears it.
    Project { t: DateTime<Utc>, name: String },
}

impl Entry {
    pub fn time(&self) -> DateTime<Utc> {
        match self {
            Entry::RestStart { t, .. }
            | Entry::RestEnd { t }
            | Entry::Goal { t, .. }
            | Entry::Project { t, .. } => *t,
        }
    }
}

/// A forced rest interval.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rest {
    pub start: DateTime<Utc>,
    /// Actual end, or `now` while still running.
    pub end: DateTime<Utc>,
    /// End time requested when the rest was started (timed rests only).
    pub planned_end: Option<DateTime<Utc>>,
    /// Still running at the time of evaluation.
    pub open: bool,
}

pub fn path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share/aw-timer/journal.jsonl")
}

/// Append one entry (creating the file and its directory if needed).
///
/// # Errors
/// Propagates I/O errors.
pub fn append(entry: &Entry) -> std::io::Result<()> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
    // One short write with O_APPEND: concurrent readers never see half a line
    // in practice, and malformed lines are skipped anyway.
    OpenOptions::new().create(true).append(true).open(p)?.write_all(format!("{line}\n").as_bytes())
}

/// All entries in time order; a missing file or malformed lines are ignored.
pub fn load() -> Vec<Entry> {
    let text = std::fs::read_to_string(path()).unwrap_or_default();
    let mut entries: Vec<Entry> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    entries.sort_by_key(Entry::time);
    entries
}

/// Rest intervals as of `now`. A new start or an explicit end closes the
/// previous rest; a timed rest also closes itself at its planned end.
///
/// Time O(n) over the (time-sorted) entries.
pub fn rests(entries: &[Entry], now: DateTime<Utc>) -> Vec<Rest> {
    let mut out = Vec::new();
    let mut cur: Option<(DateTime<Utc>, Option<DateTime<Utc>>)> = None;
    let close = |out: &mut Vec<Rest>, (start, planned): (DateTime<Utc>, Option<DateTime<Utc>>), at: DateTime<Utc>| {
        let end = planned.map_or(at, |p| p.min(at));
        out.push(Rest { start, end, planned_end: planned, open: false });
    };
    for e in entries.iter().filter(|e| e.time() <= now) {
        match *e {
            Entry::RestStart { t, minutes: m } => {
                if let Some(c) = cur.take() {
                    close(&mut out, c, t);
                }
                cur = Some((t, m.map(|m| t + minutes(m))));
            }
            Entry::RestEnd { t } => {
                if let Some(c) = cur.take() {
                    close(&mut out, c, t);
                }
            }
            Entry::Goal { .. } | Entry::Project { .. } => {}
        }
    }
    if let Some((start, planned)) = cur {
        match planned {
            Some(p) if p <= now => out.push(Rest { start, end: p, planned_end: planned, open: false }),
            _ => out.push(Rest { start, end: now, planned_end: planned, open: true }),
        }
    }
    out
}

/// Latest value picked by `pick` set earlier on the same local day as `t`;
/// an empty value means "cleared".
fn latest_same_day(entries: &[Entry], t: DateTime<Utc>, pick: impl Fn(&Entry) -> Option<&str>) -> Option<String> {
    let day = t.with_timezone(&Local).date_naive();
    entries
        .iter()
        .rev()
        .filter(|e| e.time() <= t && e.time().with_timezone(&Local).date_naive() == day)
        .find_map(|e| pick(e).map(|v| v.trim().to_owned()))
        .filter(|v| !v.is_empty())
}

/// The goal in effect at `t`.
pub fn goal_at(entries: &[Entry], t: DateTime<Utc>) -> Option<String> {
    latest_same_day(entries, t, |e| match e {
        Entry::Goal { text, .. } => Some(text),
        _ => None,
    })
}

/// The manually selected project in effect at `t`.
pub fn project_at(entries: &[Entry], t: DateTime<Utc>) -> Option<String> {
    latest_same_day(entries, t, |e| match e {
        Entry::Project { name, .. } => Some(name),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::tests::t0;

    fn at(m: f64) -> DateTime<Utc> {
        t0() + minutes(m)
    }

    #[test]
    fn timed_rest_expires_and_manual_end_wins() {
        let e = vec![
            Entry::RestStart { t: at(0.0), minutes: Some(5.0) },
            Entry::RestStart { t: at(10.0), minutes: Some(15.0) },
            Entry::RestEnd { t: at(12.0) },
        ];
        let r = rests(&e, at(30.0));
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].start, r[0].end, r[0].open), (at(0.0), at(5.0), false));
        assert_eq!((r[1].start, r[1].end), (at(10.0), at(12.0)));
    }

    #[test]
    fn open_rest_runs_until_now() {
        let e = vec![Entry::RestStart { t: at(0.0), minutes: None }];
        let r = rests(&e, at(7.0));
        assert_eq!((r[0].end, r[0].open), (at(7.0), true));
        let timed = vec![Entry::RestStart { t: at(0.0), minutes: Some(15.0) }];
        assert!(rests(&timed, at(7.0))[0].open);
    }

    #[test]
    fn goal_latest_and_clearable() {
        let e = vec![
            Entry::Goal { t: at(0.0), text: "寫 HW3".into() },
            Entry::Goal { t: at(30.0), text: "  ".into() },
        ];
        assert_eq!(goal_at(&e, at(10.0)).as_deref(), Some("寫 HW3"));
        assert_eq!(goal_at(&e, at(40.0)), None);
    }

    #[test]
    fn entries_round_trip_as_json_lines() {
        let e = Entry::RestStart { t: at(0.0), minutes: Some(5.0) };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"type\":\"rest_start\""));
        assert_eq!(serde_json::from_str::<Entry>(&s).unwrap(), e);
    }
}
