//! Live tracking: snapshot of the current state and the notifier loop.

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::mpsc::Receiver;
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, Local, NaiveDate, TimeZone, Utc};

use crate::categorize::{Categorizer, Kind};
use crate::claude_code::{CodeEvent, CodeIndex};
use crate::client::{AwClient, Unavailable};
use crate::config::{Settings, minutes};
use crate::journal::{self, Entry, Rest};
use crate::notify::notify;
use crate::sessions::{
    Activity, Barrier, Span, distraction_runs, fmt_duration, focus_sessions, focus_spans,
    seconds, work_blocks,
};

/// A span is "ongoing" if its last event ended this recently. The window
/// watcher heartbeats every second, so 2 minutes is generous.
fn ongoing_tolerance() -> Duration {
    Duration::minutes(2)
}

/// Local-midnight bounds `[start, end)` of `day`, in UTC.
pub fn day_bounds(day: NaiveDate) -> (DateTime<Utc>, DateTime<Utc>) {
    let midnight = |d: NaiveDate| {
        let naive = d.and_hms_opt(0, 0, 0).expect("valid midnight");
        // `earliest` handles DST folds; a DST gap at midnight falls back to UTC.
        Local
            .from_local_datetime(&naive)
            .earliest()
            .map_or_else(|| Utc.from_utc_datetime(&naive), |t| t.with_timezone(&Utc))
    };
    (midnight(day), midnight(day.succ_opt().expect("date in range")))
}

/// Fetch non-AFK window events and attach a category to each.
pub fn load_activities(
    client: &mut AwClient,
    categorizer: &mut Categorizer,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<Activity>, Unavailable> {
    Ok(client
        .window_events(start, end)?
        .into_iter()
        .map(|ev| {
            let cat = categorizer.classify(&ev.app, &ev.title);
            Activity {
                start: ev.start,
                end: ev.end,
                category: cat.name.clone(),
                kind: cat.kind,
                app: ev.app,
                title: ev.title,
                project: None,
                detail: None,
            }
        })
        .collect())
}

/// Derived state for one point in time.
pub struct Snapshot {
    pub now: DateTime<Utc>,
    pub activities: Vec<Activity>,
    /// Ongoing work block.
    pub block: Option<Span>,
    /// Ongoing focus session (already past the minimum).
    pub session: Option<Span>,
    /// Ongoing focus streak of any length (what the menu bar shows).
    pub streak: Option<Span>,
    /// Ongoing distraction run.
    pub distraction: Option<Span>,
    /// Forced rest in progress, if any.
    pub rest: Option<Rest>,
    /// Focus goal currently set.
    pub goal: Option<String>,
    /// Project selected manually from the menu, if any.
    pub manual_project: Option<String>,
}

impl Snapshot {
    /// Seconds of activity today, optionally filtered by kind.
    pub fn total(&self, kind: Option<Kind>) -> f64 {
        self.activities
            .iter()
            .filter(|a| kind.is_none_or(|k| a.kind == k))
            .map(Activity::seconds)
            .sum()
    }

    /// Two-line summary for a terminal or menu bar.
    pub fn status_line(&self) -> String {
        let line = format!(
            "今日 活躍 {} · 專注 {} · 分心 {}",
            fmt_duration(self.total(None)),
            fmt_duration(self.total(Some(Kind::Focus))),
            fmt_duration(self.total(Some(Kind::Distraction))),
        );
        if let Some(r) = &self.rest {
            let state = match r.planned_end {
                Some(e) => format!("剩 {}", fmt_duration(seconds(e - self.now) + 59.0)),
                None => format!("已休息 {}", fmt_duration(seconds(self.now - r.start))),
            };
            return format!("{line}\n目前：☕ 強制休息中（{state}）");
        }
        let Some(block) = &self.block else {
            return format!("{line}\n目前：休息中");
        };
        let mut now = vec![format!("距上次休息 {}", fmt_duration(seconds(self.now - block.start)))];
        if let Some(s) = &self.session {
            now.push(format!("🎯 專注時段 {}（{}）", fmt_duration(s.active_seconds), s.top_category()));
        }
        if let Some(d) = &self.distraction {
            now.push(format!("分心中 {}（{}）", fmt_duration(d.active_seconds), d.top_label()));
        }
        format!("{line}\n目前：{}", now.join(" · "))
    }
}

/// The last span if it is still running at `now`.
fn ongoing(mut spans: Vec<Span>, now: DateTime<Utc>) -> Option<Span> {
    spans.pop().filter(|s| now - s.end <= ongoing_tolerance())
}

/// Relabel the parts of activities that fall inside forced rests as
/// `Kind::Rest`, splitting activities at rest boundaries.
///
/// Time O(n·r) for n activities and r rests (r is a handful per day).
pub fn apply_rests(acts: Vec<Activity>, rests: &[Rest]) -> Vec<Activity> {
    let mut out = Vec::with_capacity(acts.len());
    for act in acts {
        let mut pieces = vec![act];
        for r in rests {
            let mut next = Vec::with_capacity(pieces.len());
            for p in pieces {
                if r.end <= p.start || r.start >= p.end {
                    next.push(p);
                    continue;
                }
                if p.start < r.start {
                    next.push(Activity { end: r.start, ..p.clone() });
                }
                out.push(Activity {
                    start: p.start.max(r.start),
                    end: p.end.min(r.end),
                    category: Kind::Rest.label().into(),
                    kind: Kind::Rest,
                    ..p.clone()
                });
                if p.end > r.end {
                    next.push(Activity { start: r.end, ..p });
                }
            }
            pieces = next;
        }
        out.extend(pieces);
    }
    out.sort_by_key(|a| a.start);
    out
}

/// Apps in which a Claude Code session may be the thing on screen.
const CODE_APPS: &[&str] = &[
    "claude", "terminal", "終端機", "iterm2", "wezterm", "ghostty", "kitty", "alacritty",
    "warp", "code", "visual studio code", "cursor", "zed",
];

/// A transcript line this long before a slice still counts for it (you
/// read Claude's answer for a while after it was written).
fn code_lookback() -> Duration {
    Duration::minutes(2)
}

/// Slice length for attributing long "Claude" windows to sessions.
fn code_slice() -> Duration {
    Duration::minutes(5)
}

/// The session with most transcript lines among `evs` (ties: most recent);
/// returns its latest event.
fn dominant(evs: &[CodeEvent]) -> Option<&CodeEvent> {
    let mut counts: HashMap<&str, (usize, &CodeEvent)> = HashMap::new();
    for ev in evs {
        let entry = counts.entry(ev.session.as_str()).or_insert((0, ev));
        entry.0 += 1;
        entry.1 = ev; // evs are time-sorted, so this ends as the latest
    }
    counts.into_values().max_by_key(|(n, ev)| (*n, ev.t)).map(|(_, ev)| ev)
}

/// Attach a project (and, for Claude Code, the session name) to activities.
///
/// Precedence per activity: Claude Code session cwd (code apps only) >
/// project pattern on `"app title"` > manual project from the menu (focus
/// time only). Long code-app activities are cut into 5-minute slices so a
/// window kept open across sessions is split between them.
///
/// Time O(n·log m) for n activity slices and m transcript events.
pub fn assign_projects(
    acts: Vec<Activity>,
    cat: &Categorizer,
    code: &CodeIndex,
    entries: &[Entry],
) -> Vec<Activity> {
    let mut out = Vec::with_capacity(acts.len());
    for act in acts {
        if act.kind == Kind::Rest {
            out.push(act);
            continue;
        }
        let is_code_app = CODE_APPS.contains(&act.app.to_lowercase().as_str());
        let has_code = is_code_app && !code.between(act.start - code_lookback(), act.end).is_empty();
        let pieces = if has_code { slices(act, code_slice()) } else { vec![act] };
        for mut p in pieces {
            if has_code && let Some(ev) = dominant(code.between(p.start - code_lookback(), p.end)) {
                p.detail = Some(code.name(ev));
                p.project = cat.project_for_path(&ev.cwd).map(str::to_owned);
            }
            if p.project.is_none() {
                p.project = cat.project_for(&p.app, &p.title).map(str::to_owned);
            }
            if p.project.is_none() && p.kind == Kind::Focus {
                p.project = journal::project_at(entries, p.start);
            }
            out.push(p);
        }
    }
    out
}

/// Cut an activity into consecutive pieces of at most `len`.
fn slices(act: Activity, len: Duration) -> Vec<Activity> {
    let mut out = Vec::new();
    let mut start = act.start;
    while act.end - start > len {
        out.push(Activity { start, end: start + len, ..act.clone() });
        start += len;
    }
    out.push(Activity { start, ..act });
    out
}

/// Rest intervals as barriers for span merging.
pub fn barriers(rests: &[Rest]) -> Vec<Barrier> {
    rests.iter().map(|r| (r.start, r.end)).collect()
}

/// Pure part of [`snapshot`]: derive ongoing spans from activities.
pub fn build_snapshot(
    activities: Vec<Activity>,
    s: &Settings,
    now: DateTime<Utc>,
    rests: &[Rest],
    goal: Option<String>,
    manual_project: Option<String>,
) -> Snapshot {
    let rest = rests.last().copied().filter(|r| r.open);
    let b = barriers(rests);
    // During a forced rest nothing is "ongoing" except the rest itself.
    let live = |spans: Vec<Span>| if rest.is_some() { None } else { ongoing(spans, now) };
    Snapshot {
        block: live(work_blocks(&activities, s.break_gap(), &b)),
        session: live(focus_sessions(&activities, s.max_interruption(), s.min_session(), &b)),
        streak: live(focus_spans(&activities, s.max_interruption(), &b)),
        distraction: live(distraction_runs(&activities, s.distraction_merge(), &b)),
        rest,
        goal,
        manual_project,
        now,
        activities,
    }
}

/// A day's activities (forced rests applied), its rests and the journal.
pub type DayData = (Vec<Activity>, Vec<Rest>, Vec<Entry>);

/// Activities of `[start, end)` with forced rests applied, plus the journal.
pub fn load_day(
    client: &mut AwClient,
    categorizer: &mut Categorizer,
    code: &mut CodeIndex,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<DayData, Unavailable> {
    let entries = journal::load();
    let now = Utc::now().min(end);
    let rests: Vec<Rest> = journal::rests(&entries, now)
        .into_iter()
        .filter(|r| r.end > start && r.start < end)
        .collect();
    let acts = apply_rests(load_activities(client, categorizer, start, end.min(Utc::now()))?, &rests);
    code.refresh(start);
    let acts = assign_projects(acts, categorizer, code, &entries);
    Ok((acts, rests, entries))
}

/// Compute the current state from today's ActivityWatch data.
///
/// The query always starts at local midnight (a fixed anchor), so span start
/// times are stable across ticks and can be used as identities.
pub fn snapshot(
    client: &mut AwClient,
    categorizer: &mut Categorizer,
    code: &mut CodeIndex,
    settings: &Settings,
) -> Result<Snapshot, Unavailable> {
    let now = Utc::now();
    let (start, end) = day_bounds(now.with_timezone(&Local).date_naive());
    let (acts, rests, entries) = load_day(client, categorizer, code, start, end)?;
    let (goal, project) = (journal::goal_at(&entries, now), journal::project_at(&entries, now));
    Ok(build_snapshot(acts, settings, now, &rests, goal, project))
}

/// Decides which notifications to send; remembers what it already sent.
pub struct Notifier {
    s: Settings,
    day: Option<NaiveDate>,
    break_reminded: HashMap<DateTime<Utc>, DateTime<Utc>>,
    distraction_alerted: HashSet<DateTime<Utc>>,
    session_announced: HashSet<DateTime<Utc>>,
    prev_rest: Option<Rest>,
}

impl Notifier {
    pub fn new(settings: Settings) -> Self {
        Self {
            s: settings,
            day: None,
            break_reminded: HashMap::new(),
            distraction_alerted: HashSet::new(),
            session_announced: HashSet::new(),
            prev_rest: None,
        }
    }

    fn reset_if_new_day(&mut self, now: DateTime<Utc>) {
        let today = now.with_timezone(&Local).date_naive();
        if self.day != Some(today) {
            self.day = Some(today);
            self.break_reminded.clear();
            self.distraction_alerted.clear();
            self.session_announced.clear();
        }
    }

    /// `(title, message)` pairs to show for this snapshot.
    pub fn decide(&mut self, snap: &Snapshot) -> Vec<(String, String)> {
        self.reset_if_new_day(snap.now);
        let mut out = Vec::new();

        // 0. Forced rest: announce a timed rest running out; stay silent while resting.
        if let Some(prev) = self.prev_rest.take()
            && snap.rest.is_none()
            && prev.planned_end.is_some_and(|e| snap.now >= e)
        {
            out.push(("休息結束 🪨".into(), "回來繼續推石頭吧".into()));
        }
        self.prev_rest = snap.rest;
        if snap.rest.is_some() {
            return out;
        }

        // 1. Break reminder: long screen time (any non-rest activity) since the last real break.
        if let Some(block) = &snap.block {
            let worked = seconds(snap.now - block.start);
            let due = self
                .break_reminded
                .get(&block.start)
                .is_none_or(|&last| snap.now - last >= minutes(self.s.break_repeat_min));
            if worked >= self.s.break_after_min * 60.0 && due {
                self.break_reminded.insert(block.start, snap.now);
                out.push((
                    "該休息了 ☕".into(),
                    format!(
                        "距上次休息已 {}（用電腦的時間都算，不只專注），起來走走、離開螢幕 {} 分鐘吧",
                        fmt_duration(worked),
                        self.s.break_gap_min
                    ),
                ));
            }
        }

        // 2. Distraction alert: once per distraction run.
        if let Some(run) = &snap.distraction
            && run.active_seconds >= self.s.distraction_alert_min * 60.0
            && self.distraction_alerted.insert(run.start)
        {
            out.push((
                "回到正軌 👀".into(),
                format!("已分心 {}（{}）", fmt_duration(run.active_seconds), run.top_label()),
            ));
        }

        // 3. Focus session reached: positive feedback, once per session.
        if self.s.notify_focus_start
            && let Some(sess) = &snap.session
            && self.session_announced.insert(sess.start)
        {
            let goal = snap.goal.as_deref().map_or(String::new(), |g| format!("\n目標：{g}"));
            let what = sess.top_project().unwrap_or(sess.top_category());
            out.push((
                "進入專注時段 🎯".into(),
                format!("已專注 {}（{what}），繼續保持！{goal}", fmt_duration(sess.active_seconds)),
            ));
        }
        out
    }
}

/// Main loop of the background timer.
///
/// * `publish` receives every snapshot (or the error) so a UI can mirror it.
/// * `reload` returns a fresh categorizer when the config files changed.
/// * `poke` wakes the loop early (e.g. right after a menu action).
pub fn run_forever(
    client: &mut AwClient,
    categorizer: &mut Categorizer,
    settings: &Settings,
    verbose: bool,
    mut publish: impl FnMut(Result<&Snapshot, &Unavailable>),
    mut reload: impl FnMut() -> Option<Categorizer>,
    poke: Option<Receiver<()>>,
) -> ! {
    let mut notifier = Notifier::new(settings.clone());
    let mut last_launch: Option<Instant> = None;
    let poll = StdDuration::from_secs_f64(settings.poll_seconds.max(1.0));
    let mut code = CodeIndex::default();
    loop {
        if let Some(fresh) = reload() {
            *categorizer = fresh;
            println!("categories reloaded");
        }
        match snapshot(client, categorizer, &mut code, settings) {
            Ok(snap) => {
                let stamp = snap.now.with_timezone(&Local).format("%H:%M:%S");
                for (title, message) in notifier.decide(&snap) {
                    notify(&title, &message, &settings.notify_sound);
                    println!("{stamp} notify: {title} — {message}");
                }
                if verbose {
                    println!("{stamp} {}", snap.status_line());
                }
                publish(Ok(&snap));
            }
            Err(e) => {
                eprintln!("ActivityWatch unavailable: {e}");
                publish(Err(&e));
                // Relaunch at most every 5 minutes to avoid fighting the user.
                let due = last_launch.is_none_or(|t| t.elapsed() > StdDuration::from_secs(300));
                if settings.launch_activitywatch && due {
                    last_launch = Some(Instant::now());
                    let _ = Command::new("open").args(["-ga", "ActivityWatch"]).status();
                }
            }
        }
        match &poke {
            // Drain extra pokes so a burst of clicks causes one refresh.
            Some(rx) => {
                let _ = rx.recv_timeout(poll);
                while rx.try_recv().is_ok() {}
            }
            None => std::thread::sleep(poll),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::tests::{act, focus, t0};

    fn snap(now_min: f64, acts: Vec<Activity>) -> Snapshot {
        build_snapshot(acts, &Settings::default(), t0() + minutes(now_min), &[], None, None)
    }

    fn titles(out: Vec<(String, String)>) -> Vec<String> {
        out.into_iter().map(|(t, _)| t).collect()
    }

    #[test]
    fn break_reminder_once_then_repeat() {
        let mut n = Notifier::new(Settings::default());
        let brk = "該休息了 ☕".to_owned();
        assert!(!titles(n.decide(&snap(45.0, vec![focus(0.0, 45.0)]))).contains(&brk));
        assert!(titles(n.decide(&snap(51.0, vec![focus(0.0, 51.0)]))).contains(&brk));
        assert!(!titles(n.decide(&snap(55.0, vec![focus(0.0, 55.0)]))).contains(&brk));
        assert!(titles(n.decide(&snap(66.0, vec![focus(0.0, 66.0)]))).contains(&brk));
    }

    fn rest(start: f64, end: f64, planned: Option<f64>, open: bool) -> Rest {
        Rest {
            start: t0() + minutes(start),
            end: t0() + minutes(end),
            planned_end: planned.map(|p| t0() + minutes(p)),
            open,
        }
    }

    #[test]
    fn apply_rests_splits_and_relabels() {
        let out = apply_rests(vec![focus(0.0, 30.0)], &[rest(10.0, 15.0, None, false)]);
        let kinds: Vec<_> = out.iter().map(|a| (a.kind, a.seconds() / 60.0)).collect();
        assert_eq!(kinds, vec![(Kind::Focus, 10.0), (Kind::Rest, 5.0), (Kind::Focus, 15.0)]);
    }

    #[test]
    fn forced_rest_silences_and_announces_end() {
        let mut n = Notifier::new(Settings::default());
        let r = rest(51.0, 52.0, Some(56.0), true);
        let acts = apply_rests(vec![focus(0.0, 52.0)], &[r]);
        let s = build_snapshot(acts.clone(), &Settings::default(), t0() + minutes(52.0), &[r], None, None);
        assert!(s.block.is_none() && s.rest.is_some());
        assert!(n.decide(&s).is_empty());
        let done = rest(51.0, 56.0, Some(56.0), false);
        let s = build_snapshot(acts, &Settings::default(), t0() + minutes(56.5), &[done], None, None);
        assert_eq!(titles(n.decide(&s)), vec!["休息結束 🪨".to_owned()]);
    }

    #[test]
    fn projects_from_code_session_pattern_and_manual() {
        let toml = r#"
            [[project]]
            name = "CVSD"
            paths = ["/Users/x/Developer/CVSD"]
            pattern = 'cvsd|verilog'
            [[project]]
            name = "LSV"
            paths = ["/Users/x/Developer/LSV"]
            pattern = '\blsv\b'
        "#;
        let (_, cat) = crate::config::parse(toml, Vec::new()).unwrap();
        let ev = |m: f64, s: &str, cwd: &str| CodeEvent { t: t0() + minutes(m), session: s.into(), cwd: cwd.into() };
        let code = CodeIndex::from_events(
            vec![ev(1.0, "a", "/Users/x/Developer/LSV/PA1"), ev(2.0, "a", "/Users/x/Developer/LSV/PA1"),
                 ev(7.0, "b", "/Users/x/Developer/CVSD/hw3")],
            [("a".to_owned(), "PA1 debug".to_owned())].into(),
        );
        let mut claude = focus(0.0, 10.0);
        claude.app = "Claude".into();
        let mut page = focus(10.0, 2.0);
        page.app = "Vivaldi".into();
        page.title = "Verilog 寫法練習場 - Vivaldi".into();
        let other = focus(12.0, 3.0); // "Code" app, no transcript lines, no pattern hit
        let entries = vec![Entry::Project { t: t0(), name: "LSV".into() }];
        let out = assign_projects(vec![claude, page, other], &cat, &code, &entries);
        let got: Vec<_> = out.iter().map(|a| (a.project.as_deref(), a.detail.as_deref())).collect();
        assert_eq!(got, vec![
            (Some("LSV"), Some("PA1 debug")), // 0–5 min: session a
            (Some("CVSD"), Some("hw3")),      // 5–10 min: session b (no title → cwd name)
            (Some("CVSD"), None),             // page title pattern
            (Some("LSV"), None),              // manual project
        ]);
    }

    #[test]
    fn distraction_and_focus_alert_once() {
        let mut n = Notifier::new(Settings::default());
        let mut yt = act(30.0, 11.0, Kind::Distraction, "娛樂");
        yt.app = "Vivaldi".into();
        yt.title = "lofi - YouTube - Vivaldi".into();
        let acts = vec![focus(0.0, 30.0), yt];
        let out: HashMap<_, _> = n.decide(&snap(41.0, acts.clone())).into_iter().collect();
        assert!(out["回到正軌 👀"].contains("lofi"));
        assert!(n.decide(&snap(41.5, acts)).is_empty());
    }
}
