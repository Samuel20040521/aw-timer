//! Interval merging: work blocks, focus sessions and distraction runs.
//!
//! All three reduce to one primitive: take a subset of activities and merge
//! neighbours whose gap is below a threshold.
//!
//! * work block      = all activities,         gap < `break_gap`
//! * focus session   = focus activities only,  gap < `max_interruption`
//!   (gaps cover AFK *and* non-focus activity), kept only if the focused
//!   time reaches `min_session`
//! * distraction run = distraction activities, gap < `distraction_merge`

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};

use crate::categorize::Kind;

/// A categorized, non-AFK window event.
#[derive(Debug, Clone)]
pub struct Activity {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub app: String,
    pub title: String,
    pub category: String,
    pub kind: Kind,
    /// Project this time belongs to (rules, Claude Code cwd, or manual).
    pub project: Option<String>,
    /// Extra context, e.g. the Claude Code session title.
    pub detail: Option<String>,
}

impl Activity {
    /// Duration in seconds.
    pub fn seconds(&self) -> f64 {
        seconds(self.end - self.start)
    }
}

/// Duration to fractional seconds.
pub fn seconds(d: Duration) -> f64 {
    d.num_microseconds().unwrap_or(i64::MAX) as f64 / 1e6
}

const BROWSERS: &[&str] = &[
    "vivaldi", "google chrome", "safari", "arc", "firefox", "microsoft edge",
    "brave browser", "opera", "zen", "orion", "chromium",
];

/// Page title (browser suffix removed) if `act` is a browser window.
pub fn page_title(act: &Activity) -> Option<&str> {
    if act.title.is_empty() || !BROWSERS.contains(&act.app.to_lowercase().as_str()) {
        return None;
    }
    // Browser titles look like "Page title - Vivaldi"; drop the suffix.
    Some(act.title.rsplit_once(" - ").map_or(act.title.as_str(), |(t, _)| t))
}

/// Human label: page title for browsers, the Claude Code session title when
/// known, otherwise the app name.
pub fn short_label(act: &Activity) -> String {
    if let Some(d) = &act.detail {
        return truncate(&format!("{} · {d}", act.app), 40);
    }
    let Some(title) = page_title(act) else {
        return act.app.clone();
    };
    truncate(title, 40)
}

fn truncate(s: &str, max: usize) -> String {
    let title = s;
    if title.chars().count() <= max {
        title.to_owned()
    } else {
        title.chars().take(max - 1).chain(std::iter::once('…')).collect()
    }
}

/// A merged run of activities, with per-category/label totals.
#[derive(Debug, Clone)]
pub struct Span {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub active_seconds: f64,
    by_category: HashMap<String, f64>,
    by_label: HashMap<String, f64>,
    by_project: HashMap<String, f64>,
}

impl Span {
    fn new(act: &Activity) -> Self {
        let mut span = Self {
            start: act.start,
            end: act.end,
            active_seconds: 0.0,
            by_category: HashMap::new(),
            by_label: HashMap::new(),
            by_project: HashMap::new(),
        };
        span.add(act);
        span
    }

    fn add(&mut self, act: &Activity) {
        let s = act.seconds();
        self.end = self.end.max(act.end);
        self.active_seconds += s;
        *self.by_category.entry(act.category.clone()).or_default() += s;
        *self.by_label.entry(short_label(act)).or_default() += s;
        if let Some(p) = &act.project {
            *self.by_project.entry(p.clone()).or_default() += s;
        }
    }

    /// Project with the most time inside this span, if any.
    pub fn top_project(&self) -> Option<&str> {
        Some(top_key(&self.by_project)).filter(|p| !p.is_empty())
    }

    /// Category with the most time inside this span.
    pub fn top_category(&self) -> &str {
        top_key(&self.by_category)
    }

    /// Most-used app (or page title for browsers) inside this span.
    pub fn top_label(&self) -> &str {
        top_key(&self.by_label)
    }
}

/// Key with the largest value; ties broken by key for determinism.
fn top_key(m: &HashMap<String, f64>) -> &str {
    m.iter()
        .max_by(|a, b| a.1.total_cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map_or("", |(k, _)| k.as_str())
}

/// A forced-rest interval that no span may bridge.
pub type Barrier = (DateTime<Utc>, DateTime<Utc>);

/// Whether any barrier overlaps the gap `[from, to]`.
fn blocked(barriers: &[Barrier], from: DateTime<Utc>, to: DateTime<Utc>) -> bool {
    barriers.iter().any(|&(s, e)| s < to && e > from)
}

/// Merge activities whose gap to the previous span is below `max_gap` and
/// not crossed by a barrier.
///
/// Time O(n log n) for the sort plus O(n·b) for the sweep (b barriers,
/// a handful per day); space O(n). Activities are assumed non-overlapping
/// (ActivityWatch floods them).
pub fn merge_spans<'a>(
    acts: impl IntoIterator<Item = &'a Activity>,
    max_gap: Duration,
    barriers: &[Barrier],
) -> Vec<Span> {
    let mut ordered: Vec<&Activity> = acts.into_iter().collect();
    ordered.sort_by_key(|a| a.start);
    let mut spans: Vec<Span> = Vec::new();
    for act in ordered {
        match spans.last_mut() {
            Some(cur) if act.start - cur.end < max_gap && !blocked(barriers, cur.end, act.start) => {
                cur.add(act)
            }
            _ => spans.push(Span::new(act)),
        }
    }
    spans
}

/// Continuous working periods separated by breaks >= `break_gap` or by a
/// forced rest (time inside a forced rest is not work).
pub fn work_blocks(acts: &[Activity], break_gap: Duration, barriers: &[Barrier]) -> Vec<Span> {
    merge_spans(acts.iter().filter(|a| a.kind != Kind::Rest), break_gap, barriers)
}

/// Focus sessions: focused work tolerating interruptions < `max_interruption`,
/// with at least `min_focus` of focused time.
pub fn focus_sessions(
    acts: &[Activity],
    max_interruption: Duration,
    min_focus: Duration,
    barriers: &[Barrier],
) -> Vec<Span> {
    let threshold = seconds(min_focus);
    focus_spans(acts, max_interruption, barriers)
        .into_iter()
        .filter(|s| s.active_seconds >= threshold)
        .collect()
}

/// Focus streaks of any length (focus sessions before the minimum filter).
pub fn focus_spans(acts: &[Activity], max_interruption: Duration, barriers: &[Barrier]) -> Vec<Span> {
    merge_spans(acts.iter().filter(|a| a.kind == Kind::Focus), max_interruption, barriers)
}

/// Runs of distracting activity, bridging brief app switches.
pub fn distraction_runs(acts: &[Activity], merge_gap: Duration, barriers: &[Barrier]) -> Vec<Span> {
    merge_spans(acts.iter().filter(|a| a.kind == Kind::Distraction), merge_gap, barriers)
}

/// Format seconds as `"2h 05m"`, `"42m"` or `"<1m"`.
pub fn fmt_duration(secs: f64) -> String {
    let minutes = (secs / 60.0).floor() as i64;
    if minutes < 1 {
        return "<1m".into();
    }
    let (h, m) = (minutes / 60, minutes % 60);
    if h > 0 { format!("{h}h {m:02}m") } else { format!("{m}m") }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chrono::TimeZone;

    pub fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 30, 9, 0, 0).unwrap()
    }

    /// Activity starting `start_min` minutes after T0.
    pub fn act(start_min: f64, mins: f64, kind: Kind, cat: &str) -> Activity {
        let start = t0() + crate::config::minutes(start_min);
        Activity {
            start,
            end: start + crate::config::minutes(mins),
            app: "Code".into(),
            title: String::new(),
            category: cat.into(),
            kind,
            project: None,
            detail: None,
        }
    }

    pub fn focus(start_min: f64, mins: f64) -> Activity {
        act(start_min, mins, Kind::Focus, "寫程式")
    }

    #[test]
    fn merge_bridges_short_gaps_only() {
        let acts = [focus(0.0, 10.0), focus(12.0, 10.0), focus(40.0, 5.0)];
        let spans = merge_spans(&acts, Duration::minutes(5), &[]);
        assert_eq!(spans.len(), 2);
        assert!((spans[0].active_seconds - 1200.0).abs() < 1e-6);
        assert_eq!(spans[0].end, t0() + Duration::minutes(22));
    }

    #[test]
    fn focus_session_survives_short_distraction() {
        let acts = [focus(0.0, 15.0), act(15.0, 3.0, Kind::Distraction, "娛樂"), focus(18.0, 15.0)];
        let s = focus_sessions(&acts, Duration::minutes(5), Duration::minutes(25), &[]);
        assert_eq!(s.len(), 1);
        assert!((s[0].active_seconds - 1800.0).abs() < 1e-6);
    }

    #[test]
    fn long_distraction_splits_and_short_sessions_drop() {
        let acts = [focus(0.0, 20.0), act(20.0, 10.0, Kind::Distraction, "娛樂"), focus(30.0, 30.0)];
        let s = focus_sessions(&acts, Duration::minutes(5), Duration::minutes(25), &[]);
        assert_eq!(s.iter().map(|x| x.start).collect::<Vec<_>>(), vec![t0() + Duration::minutes(30)]);
    }

    #[test]
    fn barrier_splits_even_short_gaps() {
        let acts = [focus(0.0, 10.0), focus(12.0, 10.0)];
        let rest = [(t0() + Duration::minutes(10), t0() + Duration::minutes(12))];
        assert_eq!(merge_spans(&acts, Duration::minutes(5), &rest).len(), 2);
        assert_eq!(merge_spans(&acts, Duration::minutes(5), &[]).len(), 1);
    }

    #[test]
    fn duration_format() {
        assert_eq!(fmt_duration(59.0), "<1m");
        assert_eq!(fmt_duration(42.0 * 60.0), "42m");
        assert_eq!(fmt_duration(125.0 * 60.0), "2h 05m");
    }
}
