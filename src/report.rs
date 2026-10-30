//! Daily terminal report.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use chrono::{Datelike, Duration, Local, NaiveDate, Timelike};
use unicode_width::UnicodeWidthStr;

use crate::categorize::{Kind, MatchOn, Override, UNCATEGORIZED};
use crate::config::Settings;
use crate::journal::{self, Entry, Rest};
use crate::sessions::{Activity, fmt_duration, focus_sessions, page_title, seconds, short_label};
use crate::tracker::barriers;

const WEEKDAYS: [&str; 7] = ["一", "二", "三", "四", "五", "六", "日"];

/// ANSI styling, disabled when stdout is not a terminal.
struct Style {
    on: bool,
}

impl Style {
    fn wrap(&self, code: &str, t: &str) -> String {
        if self.on { format!("\x1b[{code}m{t}\x1b[0m") } else { t.to_owned() }
    }
    fn bold(&self, t: &str) -> String {
        self.wrap("1", t)
    }
    fn dim(&self, t: &str) -> String {
        self.wrap("2", t)
    }
    fn kind(&self, k: Kind, t: &str) -> String {
        let code = match k {
            Kind::Focus => "32",
            Kind::Neutral => "90",
            Kind::Distraction => "31",
            Kind::Rest => "36",
        };
        self.wrap(code, t)
    }
}

/// Horizontal bar with eighth-block resolution, padded to `width` columns.
fn bar(fraction: f64, width: usize) -> String {
    const PARTS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let eighths = (fraction.clamp(0.0, 1.0) * width as f64 * 8.0).round() as usize;
    let (full, part) = (eighths / 8, eighths % 8);
    let mut s = "█".repeat(full) + PARTS[part];
    let used = full + usize::from(part > 0);
    s.push_str(&" ".repeat(width.saturating_sub(used)));
    s
}

/// Seconds per local hour and kind; events crossing an hour are split.
///
/// Time O(n + h) where h is the number of hour boundaries crossed.
fn hourly(acts: &[Activity]) -> BTreeMap<u32, [f64; 4]> {
    let mut out: BTreeMap<u32, [f64; 4]> = BTreeMap::new();
    for a in acts {
        let (mut cur, end) = (a.start.with_timezone(&Local), a.end.with_timezone(&Local));
        while cur < end {
            let hour_start = cur
                .with_minute(0)
                .and_then(|t| t.with_second(0))
                .and_then(|t| t.with_nanosecond(0))
                .unwrap_or(cur);
            let next = end.min(hour_start + Duration::hours(1));
            out.entry(cur.hour()).or_default()[a.kind.index()] += seconds(next - cur);
            cur = next;
        }
    }
    out
}

/// `(key, seconds)` pairs sorted by descending time, ties by key.
fn ranked<K: Ord + Clone>(m: &HashMap<K, f64>) -> Vec<(K, f64)> {
    let mut v: Vec<(K, f64)> = m.iter().map(|(k, s)| (k.clone(), *s)).collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

/// Render the report for one day.
pub fn render(
    acts: &[Activity],
    day: NaiveDate,
    settings: &Settings,
    color: bool,
    rests: &[Rest],
    entries: &[Entry],
) -> String {
    let st = Style { on: color };
    let mut o = String::new();
    // `writeln!` into a String cannot fail.
    macro_rules! line { ($($t:tt)*) => { let _ = writeln!(o, $($t)*); } }

    let weekday = WEEKDAYS[day.weekday().num_days_from_monday() as usize];
    line!("{}", st.bold(&format!("📅 {}（週{weekday}）", day.format("%Y-%m-%d"))));
    let total: f64 = acts.iter().map(Activity::seconds).sum();
    if total < 60.0 {
        line!("這天還沒有足夠的活動紀錄。");
        return o;
    }

    let mut by_kind = [0.0_f64; 4];
    for a in acts {
        by_kind[a.kind.index()] += a.seconds();
    }
    // Only mention forced rest on days that had one.
    let shown_kinds: Vec<Kind> =
        Kind::ALL.into_iter().filter(|&k| k != Kind::Rest || by_kind[k.index()] > 0.0).collect();
    let kinds: Vec<String> = shown_kinds
        .iter()
        .map(|&k| {
            let s = by_kind[k.index()];
            st.kind(k, &format!("{} {}（{:.0}%）", k.label(), fmt_duration(s), s / total * 100.0))
        })
        .collect();
    line!("活躍 {}   {}", st.bold(&fmt_duration(total)), kinds.join("   "));

    // Focus sessions.
    let sessions =
        focus_sessions(acts, settings.max_interruption(), settings.min_session(), &barriers(rests));
    line!();
    let head = st.bold(&format!("🎯 專注時段 {} 個", sessions.len()));
    if sessions.is_empty() {
        line!(
            "{head}{}",
            st.dim(&format!(
                "（累積專注 ≥ {} 分鐘、中斷 < {} 分鐘才算）",
                settings.min_session_min, settings.max_interruption_min
            ))
        );
    } else {
        let sum: f64 = sessions.iter().map(|s| s.active_seconds).sum();
        line!("{head}，共 {}", fmt_duration(sum));
    }
    for s in &sessions {
        let goal = journal::goal_at(entries, s.end).map_or(String::new(), |g| format!("  🎯 {g}"));
        let project = s.top_project().map_or(String::new(), |p| format!("[{p}] "));
        line!(
            "  {}–{}  {:>7} 專注 / {:>7}  {project}{} · {}{}",
            s.start.with_timezone(&Local).format("%H:%M"),
            s.end.with_timezone(&Local).format("%H:%M"),
            fmt_duration(s.active_seconds),
            fmt_duration(seconds(s.end - s.start)),
            s.top_category(),
            s.top_label(),
            st.bold(&goal)
        );
    }
    if !rests.is_empty() {
        let total: f64 = rests.iter().map(|r| seconds(r.end - r.start)).sum();
        line!("{}", st.kind(Kind::Rest, &format!("☕ 強制休息 {} 次，共 {}", rests.len(), fmt_duration(total))));
    }

    // Projects: focus time and all time per project.
    let mut by_project: HashMap<String, [f64; 2]> = HashMap::new();
    for a in acts.iter().filter(|a| a.kind != Kind::Rest) {
        if let Some(p) = &a.project {
            let e = by_project.entry(p.clone()).or_default();
            e[1] += a.seconds();
            if a.kind == Kind::Focus {
                e[0] += a.seconds();
            }
        }
    }
    if !by_project.is_empty() {
        line!();
        line!("{}", st.bold("📁 專案"));
        let focus_total: f64 = by_kind[Kind::Focus.index()].max(1.0);
        let mut rows: Vec<(String, [f64; 2])> = by_project.into_iter().collect();
        rows.sort_by(|a, b| b.1[0].total_cmp(&a.1[0]).then_with(|| a.0.cmp(&b.0)));
        let width = rows.iter().map(|(n, _)| n.width()).max().unwrap_or(0);
        for (name, [focus, all]) in rows {
            let pad = " ".repeat(width - name.width());
            line!(
                "  {name}{pad}  {} 專注 {:>7}  {}",
                st.kind(Kind::Focus, &bar(focus / focus_total, 16)),
                fmt_duration(focus),
                st.dim(&format!("（含其他活動共 {}）", fmt_duration(all)))
            );
        }
    }

    // Claude Code sessions recognised from local transcripts.
    let mut code: HashMap<(String, String), f64> = HashMap::new();
    for a in acts {
        if let Some(d) = &a.detail {
            *code.entry((d.clone(), a.project.clone().unwrap_or_default())).or_default() += a.seconds();
        }
    }
    if !code.is_empty() {
        line!();
        line!("{}", st.bold("🤖 Claude Code sessions"));
        for ((name, project), sec) in ranked(&code).into_iter().take(8) {
            let name: String = name.chars().take(40).collect();
            let tag = if project.is_empty() { String::new() } else { format!("  [{project}]") };
            line!("  {:>7}  {name}{}", fmt_duration(sec), st.dim(&tag));
        }
    }


    // Categories.
    let mut by_cat: HashMap<String, f64> = HashMap::new();
    let mut cat_kind: HashMap<&str, Kind> = HashMap::new();
    for a in acts {
        *by_cat.entry(a.category.clone()).or_default() += a.seconds();
        cat_kind.insert(&a.category, a.kind);
    }
    line!();
    line!("{}", st.bold("📊 分類"));
    let width = by_cat.keys().map(|c| c.width()).max().unwrap_or(0);
    for (cat, sec) in ranked(&by_cat) {
        let pad = " ".repeat(width - cat.width());
        line!(
            "  {cat}{pad}  {} {:>7}  {:>3.0}%",
            st.kind(cat_kind[cat.as_str()], &bar(sec / total, 20)),
            fmt_duration(sec),
            sec / total * 100.0
        );
    }

    // Hourly timeline, 2 minutes per character.
    let hours = hourly(acts);
    let legend: Vec<String> = shown_kinds.iter().map(|&k| st.kind(k, &format!("█{}", k.label()))).collect();
    line!();
    line!("{}  {}", st.bold("🕐 每小時"), legend.join(" "));
    if let (Some(&first), Some(&last)) = (hours.keys().next(), hours.keys().next_back()) {
        for h in first..=last {
            let per = hours.get(&h).copied().unwrap_or_default();
            let mut cells = String::new();
            let mut used = 0;
            for k in Kind::ALL {
                let n = (per[k.index()] / 120.0).round() as usize;
                used += n;
                cells += &st.kind(k, &"█".repeat(n));
            }
            let busy: f64 = per.iter().sum();
            let tail = if busy > 0.0 { st.dim(&fmt_duration(busy)) } else { String::new() };
            line!("  {h:02}  {cells}{} {tail}", " ".repeat(30usize.saturating_sub(used)));
        }
    }

    // Top windows.
    let mut by_label: HashMap<(String, String), f64> = HashMap::new();
    for a in acts {
        *by_label.entry((short_label(a), a.category.clone())).or_default() += a.seconds();
    }
    line!();
    line!("{}", st.bold("🔝 最常用"));
    for ((label, cat), sec) in ranked(&by_label).into_iter().take(8) {
        line!("  {:>7}  {label}  {}", fmt_duration(sec), st.dim(&cat));
    }

    // Uncategorized: help the user refine the config.
    let mut unc: HashMap<String, f64> = HashMap::new();
    for a in acts.iter().filter(|a| a.category == UNCATEGORIZED) {
        let title: String = a.title.chars().take(60).collect();
        *unc.entry(format!("{} | {title}", a.app)).or_default() += a.seconds();
    }
    if !unc.is_empty() {
        line!();
        line!("{}", st.bold("❓ 未分類（可加進設定檔的規則）"));
        for (label, sec) in ranked(&unc).into_iter().take(5) {
            line!("  {:>7}  {label}", fmt_duration(sec));
        }
    }
    o.truncate(o.trim_end().len());
    o
}

/// Something the user can classify by hand from the report.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub label: String,
    pub on: MatchOn,
    pub value: String,
    pub category: String,
    pub seconds: f64,
}

/// Up to `limit` classifiable items: uncategorized ones first, then the
/// most-used. Browser windows are matched by page title, others by app.
pub fn candidates(acts: &[Activity], limit: usize) -> Vec<Candidate> {
    let mut map: HashMap<(MatchOn, String), Candidate> = HashMap::new();
    for a in acts.iter().filter(|a| a.kind != Kind::Rest) {
        let (on, value, label) = match page_title(a) {
            Some(t) => (MatchOn::Title, t.to_owned(), format!("{t}（{}）", a.app)),
            None => (MatchOn::App, a.app.clone(), a.app.clone()),
        };
        map.entry((on, value.to_lowercase()))
            .or_insert_with(|| Candidate { label, on, value, category: a.category.clone(), seconds: 0.0 })
            .seconds += a.seconds();
    }
    let mut v: Vec<Candidate> = map.into_values().collect();
    v.sort_by(|a, b| {
        (b.category == UNCATEGORIZED)
            .cmp(&(a.category == UNCATEGORIZED))
            .then(b.seconds.total_cmp(&a.seconds))
            .then_with(|| a.label.cmp(&b.label))
    });
    v.truncate(limit);
    v
}

/// The numbered list shown under the report.
pub fn render_candidates(cands: &[Candidate], color: bool) -> String {
    let st = Style { on: color };
    let mut o = String::new();
    let _ = writeln!(o, "{}", st.bold("✏️  手動分類：輸入「編號＋代號」，可一次多個（如 1f 3e），直接 Enter 結束"));
    let _ = writeln!(o, "   f = 專注    e = 娛樂（分心）    n = 中性");
    for (i, c) in cands.iter().enumerate() {
        let label: String = c.label.chars().take(50).collect();
        let _ = writeln!(o, "  [{:>2}] {:>7}  {label}  {}", i + 1, fmt_duration(c.seconds), st.dim(&c.category));
    }
    o
}

/// Parse input like `"1f 3e 12n"` into overrides for `cands`.
///
/// # Errors
/// Returns a message naming the first invalid token.
pub fn parse_choices(input: &str, cands: &[Candidate]) -> Result<Vec<Override>, String> {
    input
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .map(|tok| {
            let split = tok.find(|c: char| !c.is_ascii_digit()).unwrap_or(tok.len());
            let (num, code) = tok.split_at(split);
            let cand = num
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .and_then(|i| cands.get(i))
                .ok_or_else(|| format!("無效的編號：{tok}"))?;
            let (category, kind) = match code.to_lowercase().as_str() {
                "f" => ("專注", Kind::Focus),
                "e" => ("娛樂", Kind::Distraction),
                "n" => ("中性", Kind::Neutral),
                _ => return Err(format!("無效的代號：{tok}（用 f / e / n）")),
            };
            Ok(Override { match_on: cand.on, value: cand.value.clone(), category: category.into(), kind })
        })
        .collect()
}

/// Parse `YYYY-MM-DD`, `today`/`yesterday` or `-N` (days ago).
pub fn parse_day(text: Option<&str>) -> Result<NaiveDate, String> {
    let today = Local::now().date_naive();
    match text {
        None | Some("today") => Ok(today),
        Some("yesterday") => Ok(today - Duration::days(1)),
        Some(t) => {
            if let Ok(n) = t.trim_start_matches('-').parse::<i64>() {
                return Ok(today - Duration::days(n.abs()));
            }
            NaiveDate::parse_from_str(t, "%Y-%m-%d").map_err(|e| format!("無效日期 {t:?}：{e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::tests::act;

    #[test]
    fn candidates_put_uncategorized_first_and_parse_choices() {
        let mut page = act(0.0, 1.0, Kind::Neutral, UNCATEGORIZED);
        page.app = "Vivaldi".into();
        page.title = "Error response - Vivaldi".into();
        let acts = vec![act(1.0, 30.0, Kind::Focus, "寫程式"), page];
        let c = candidates(&acts, 10);
        assert_eq!((c[0].on, c[0].value.as_str()), (MatchOn::Title, "Error response"));
        let ov = parse_choices("1e, 2f", &c).unwrap();
        assert_eq!((ov[0].kind, ov[0].category.as_str()), (Kind::Distraction, "娛樂"));
        assert_eq!((ov[1].match_on, ov[1].value.as_str()), (MatchOn::App, "Code"));
        assert!(parse_choices("9f", &c).is_err());
        assert!(parse_choices("1x", &c).is_err());
    }
}
