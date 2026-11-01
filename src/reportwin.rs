//! Native report window: a day timeline next to your calendar, totals,
//! projects, one-click classification of uncategorized windows,
//! week / month / year trends, and settings (which calendars to show).
//!
//! Days run from 04:00 to 04:00 (see [`export::DAY_START_HOUR`]).
//!
//! Power: nothing runs while the window is closed (the whole view tree is
//! dropped on close), and nothing runs periodically while it is open. Data is
//! loaded on a worker thread when the window opens, when the day, tab or
//! period changes, after a classification, and when the window regains focus
//! on today's page (at most every 30 s). The content is a single flipped view
//! that replays a list of drawing ops; hover details are AppKit tooltips.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use block2::RcBlock;
use chrono::{Datelike, Duration, Local, NaiveDate, Utc};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSBackingStoreType, NSBezierPath, NSButton, NSColor,
    NSControlStateValueOff, NSControlStateValueOn,
    NSEventModifierFlags, NSFont, NSFontAttributeName, NSFontDescriptorSystemDesignSerif, NSFontWeightBold,
    NSFontWeightMedium, NSFontWeightRegular, NSFontWeightSemibold, NSForegroundColorAttributeName,
    NSLineBreakMode, NSLineCapStyle, NSLineJoinStyle, NSMenu, NSMenuItem, NSMutableParagraphStyle,
    NSParagraphStyleAttributeName, NSScrollView, NSSegmentSwitchTracking, NSSegmentedControl, NSStringDrawing,
    NSTextAlignment, NSTextField, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    NSArray, NSAttributedStringKey, NSDictionary, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect,
    NSSize, NSString, NSTimer, ns_string,
};

use crate::calendar::{self, CalendarInfo, Denied, Event};
use crate::categorize::{Kind, Override, UNCATEGORIZED};
use crate::claude_code::CodeIndex;
use crate::client::AwClient;
use crate::config;
use crate::export::{self, Segment, day_start, logical_day};
use crate::report::{self, Candidate};
use crate::sessions::{Activity, fmt_duration};
use crate::tracker;

const MARGIN: f64 = 24.0;
const TOP_BAR: f64 = 56.0;
const MIN_W: f64 = 760.0;
/// Width from which the timeline and the side cards sit side by side.
const TWO_COLUMNS_W: f64 = 980.0;
/// Re-load today's data on focus only if the last load is older than this.
const STALE_SECS: u64 = 30;
/// Timeline height of one hour.
const HOUR_PX: f64 = 44.0;
/// Focus share from which the hill's ground turns ochre, and green.
const HILL_OK: f64 = 0.4;
const HILL_GOOD: f64 = 0.65;
const WEEKDAYS: [&str; 7] = ["一", "二", "三", "四", "五", "六", "日"];

/// Segment order of the classification control.
const CHOICES: [(&str, Kind); 3] = [("專注", Kind::Focus), ("娛樂", Kind::Distraction), ("中性", Kind::Neutral)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Day,
    Trends,
    Settings,
}

impl Tab {
    /// Order of the window's tab control.
    const ALL: [Tab; 3] = [Tab::Day, Tab::Trends, Tab::Settings];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Week,
    Month,
    Year,
}

impl Period {
    const ALL: [Period; 3] = [Period::Week, Period::Month, Period::Year];

    fn label(self) -> &'static str {
        match self {
            Period::Week => "週",
            Period::Month => "月",
            Period::Year => "年",
        }
    }

    /// First and last logical day of the period containing `day`.
    fn range(self, day: NaiveDate) -> (NaiveDate, NaiveDate) {
        match self {
            Period::Week => {
                let monday = day - Duration::days(i64::from(day.weekday().num_days_from_monday()));
                (monday, monday + Duration::days(6))
            }
            Period::Month => {
                let first = day.with_day(1).unwrap_or(day);
                let next = if first.month() == 12 {
                    NaiveDate::from_ymd_opt(first.year() + 1, 1, 1)
                } else {
                    NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1)
                };
                (first, next.map_or(day, |n| n - Duration::days(1)))
            }
            Period::Year => (
                NaiveDate::from_ymd_opt(day.year(), 1, 1).unwrap_or(day),
                NaiveDate::from_ymd_opt(day.year(), 12, 31).unwrap_or(day),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Data for one page
// ---------------------------------------------------------------------------

/// Everything the "day" tab shows.
#[derive(Debug, Clone)]
pub struct DayView {
    day: NaiveDate,
    segments: Vec<Segment>,
    /// Seconds per kind, indexed by `Kind::index`.
    totals: [f64; 4],
    /// `(project, focus seconds, all seconds)` by descending focus.
    projects: Vec<(String, f64, f64)>,
    /// Longest focus stretch: `(start minute from 04:00, focused minutes)`.
    longest: Option<(f64, f64)>,
    uncategorized: Vec<Candidate>,
    events: Result<Vec<Event>, Denied>,
}

impl DayView {
    fn active(&self) -> f64 {
        self.totals.iter().sum()
    }

    fn focus_share(&self) -> f64 {
        let a = self.active();
        if a > 0.0 { self.totals[Kind::Focus.index()] / a } else { 0.0 }
    }
}

#[derive(Debug, Clone)]
struct Bucket {
    label: String,
    tip: String,
    secs: [f64; 4],
    selected: bool,
}

/// Everything the trends tab shows.
#[derive(Debug, Clone)]
pub struct TrendView {
    period: Period,
    from: NaiveDate,
    to: NaiveDate,
    buckets: Vec<Bucket>,
    /// `(category, kind, seconds)` by descending time.
    categories: Vec<(String, Kind, f64)>,
}

#[derive(Debug, Clone)]
pub struct Model {
    tab: Tab,
    /// The selected day; `None` on the settings tab.
    day: Option<DayView>,
    trend: Option<TrendView>,
    /// Calendars on this Mac; only loaded for the settings tab.
    calendars: Option<Result<Vec<CalendarInfo>, Denied>>,
    /// Identifiers of calendars hidden from the timeline.
    hidden: BTreeSet<String>,
}

fn minutes(s: &Segment) -> f64 {
    s.end_min - s.start_min
}

/// Build the day view from activities covering at least that day.
///
/// Time O(n log n) for n activities.
fn build_day(
    day: NaiveDate,
    acts: &[Activity],
    events: Result<Vec<Event>, Denied>,
    max_gap_min: f64,
) -> DayView {
    let key = day.to_string();
    let in_day: Vec<Activity> = acts.iter().filter(|a| logical_day(a.start) == day).cloned().collect();
    let segments: Vec<Segment> = export::segments(acts.to_vec()).into_iter().filter(|s| s.day == key).collect();
    let mut totals = [0.0; 4];
    let mut projects: HashMap<String, [f64; 2]> = HashMap::new();
    for s in &segments {
        totals[s.kind.index()] += minutes(s) * 60.0;
        if !s.project.is_empty() && s.kind != Kind::Rest {
            let e = projects.entry(s.project.clone()).or_default();
            e[1] += minutes(s) * 60.0;
            if s.kind == Kind::Focus {
                e[0] += minutes(s) * 60.0;
            }
        }
    }
    let mut projects: Vec<(String, f64, f64)> = projects.into_iter().map(|(n, [f, a])| (n, f, a)).collect();
    projects.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // Longest focus stretch: focus segments joined across gaps < max_gap_min.
    let mut longest: Option<(f64, f64)> = None;
    let mut cur: Option<(f64, f64, f64)> = None; // start, end, focused minutes
    let mut flush = |c: Option<(f64, f64, f64)>| {
        if let Some((start, _, m)) = c
            && longest.is_none_or(|(_, best)| m > best)
        {
            longest = Some((start, m));
        }
    };
    for s in segments.iter().filter(|s| s.kind == Kind::Focus) {
        cur = match cur {
            Some((start, end, m)) if s.start_min - end < max_gap_min => Some((start, end.max(s.end_min), m + minutes(s))),
            other => {
                flush(other);
                Some((s.start_min, s.end_min, minutes(s)))
            }
        };
    }
    flush(cur);

    let uncategorized = report::candidates(&in_day, usize::MAX)
        .into_iter()
        .filter(|c| c.category == UNCATEGORIZED)
        .collect();
    DayView { day, segments, totals, projects, longest, uncategorized, events }
}

/// Build the trends view for the period containing `day`.
fn build_trend(period: Period, day: NaiveDate, acts: &[Activity]) -> TrendView {
    let (from, to) = period.range(day);
    let rows = export::daily(acts);
    let mut per_day: HashMap<String, [f64; 4]> = HashMap::new();
    let mut cats: HashMap<String, (Kind, f64)> = HashMap::new();
    for r in &rows {
        let Ok(d) = NaiveDate::parse_from_str(&r.day, "%Y-%m-%d") else { continue };
        if d < from || d > to {
            continue;
        }
        per_day.entry(r.day.clone()).or_default()[r.kind.index()] += r.seconds;
        cats.entry(r.category.clone()).or_insert((r.kind, 0.0)).1 += r.seconds;
    }
    let buckets = if period == Period::Year {
        (1..=12)
            .map(|m| {
                let mut secs = [0.0; 4];
                for (d, s) in &per_day {
                    if d[5..7].parse::<u32>().ok() == Some(m) {
                        for k in 0..4 {
                            secs[k] += s[k];
                        }
                    }
                }
                Bucket { label: format!("{m}月"), tip: format!("{} 年 {m} 月", from.year()), secs, selected: day.month() == m }
            })
            .collect()
    } else {
        from.iter_days()
            .take_while(|d| *d <= to)
            .map(|d| {
                let wd = WEEKDAYS[d.weekday().num_days_from_monday() as usize];
                let label = if period == Period::Week { format!("週{wd}") } else { d.day().to_string() };
                let secs = per_day.get(&d.to_string()).copied().unwrap_or_default();
                Bucket { label, tip: format!("{} 月 {} 日（週{wd}）", d.month(), d.day()), secs, selected: d == day }
            })
            .collect()
    };
    let mut categories: Vec<(String, Kind, f64)> = cats.into_iter().map(|(c, (k, s))| (c, k, s)).collect();
    categories.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    TrendView { period, from, to, buckets, categories }
}

// ---------------------------------------------------------------------------
// Drawing ops (resolved against the current appearance at draw time)
// ---------------------------------------------------------------------------

/// Sisyphus palette: the laurel at the summit = focus, clay that rolls back
/// down = distraction, hillside stone = neutral, the night sky = rest.
const FOCUS: u32 = 0x3f9f68;
const DISTRACTION: u32 = 0xc9564c;
const NEUTRAL: u32 = 0x8f8a7e;
const REST: u32 = 0x5a68a6;
const STONE: u32 = 0x6f6a60;
const OCHRE: u32 = 0xd39b3c;
/// Project colours, distinct from the kind colours.
const PROJECT_COLORS: [u32; 6] = [0x4e79a7, 0x9c6ade, 0x2a9d8f, 0xe08e45, 0xd16ba5, 0x9c755f];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Ink {
    Label,
    Secondary,
    Tertiary,
    /// sRGB hex with alpha.
    Rgb(u32, f64),
    Grid,
    Card,
    Border,
    Background,
}

impl Ink {
    fn kind(k: Kind) -> Self {
        Ink::Rgb(kind_rgb(k), 1.0)
    }

    fn color(self) -> Retained<NSColor> {
        match self {
            Ink::Label => NSColor::labelColor(),
            Ink::Secondary => NSColor::secondaryLabelColor(),
            Ink::Tertiary => NSColor::tertiaryLabelColor(),
            Ink::Rgb(hex, a) => NSColor::colorWithSRGBRed_green_blue_alpha(
                f64::from((hex >> 16) & 0xff) / 255.0,
                f64::from((hex >> 8) & 0xff) / 255.0,
                f64::from(hex & 0xff) / 255.0,
                a,
            ),
            Ink::Grid => NSColor::labelColor().colorWithAlphaComponent(0.07),
            Ink::Card => NSColor::labelColor().colorWithAlphaComponent(0.035),
            Ink::Border => NSColor::labelColor().colorWithAlphaComponent(0.1),
            Ink::Background => NSColor::windowBackgroundColor(),
        }
    }
}

fn kind_rgb(k: Kind) -> u32 {
    match k {
        Kind::Focus => FOCUS,
        Kind::Neutral => NEUTRAL,
        Kind::Distraction => DISTRACTION,
        Kind::Rest => REST,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Font {
    Body,
    Bold,
    Digits,
    Heading,
    Small,
    SmallBold,
    /// Serif display figures (New York), for big numbers.
    Serif(f64),
}

impl Font {
    fn ns(self) -> Retained<NSFont> {
        // SAFETY: reading AppKit's exported weight constants.
        let (regular, medium, semibold, bold) =
            unsafe { (NSFontWeightRegular, NSFontWeightMedium, NSFontWeightSemibold, NSFontWeightBold) };
        match self {
            Font::Body => NSFont::systemFontOfSize_weight(13.0, regular),
            Font::Bold => NSFont::systemFontOfSize_weight(13.0, semibold),
            Font::Digits => NSFont::monospacedDigitSystemFontOfSize_weight(12.0, regular),
            Font::Heading => NSFont::systemFontOfSize_weight(15.0, bold),
            Font::Small => NSFont::systemFontOfSize_weight(11.0, regular),
            Font::SmallBold => NSFont::systemFontOfSize_weight(11.0, medium),
            Font::Serif(size) => {
                let base = NSFont::monospacedDigitSystemFontOfSize_weight(size, regular);
                // SAFETY: reading AppKit's exported design constant.
                let serif = base.fontDescriptor().fontDescriptorWithDesign(unsafe { NSFontDescriptorSystemDesignSerif });
                serif.and_then(|d| NSFont::fontWithDescriptor_size(&d, size)).unwrap_or(base)
            }
        }
    }

    fn line_height(self) -> f64 {
        match self {
            Font::Serif(size) => size * 1.25,
            Font::Heading => 20.0,
            Font::Small | Font::SmallBold => 14.0,
            _ => 17.0,
        }
    }
}

#[derive(Debug, Clone)]
enum Op {
    /// Single line of text, truncated with "…" to fit `rect`.
    Text { text: String, rect: NSRect, font: Font, ink: Ink, right: bool },
    Fill { rect: NSRect, ink: Ink, radius: f64 },
    /// Rectangle outline.
    Frame { rect: NSRect, ink: Ink, radius: f64, width: f64 },
    /// Polyline; `dash` draws it dashed.
    Line { pts: Vec<(f64, f64)>, ink: Ink, width: f64, dash: bool },
    /// Closed filled polygon.
    Poly { pts: Vec<(f64, f64)>, ink: Ink },
}

impl Op {
    /// Vertical extent, for skipping ops outside the dirty rect.
    fn y_range(&self) -> (f64, f64) {
        match self {
            Op::Text { rect, .. } | Op::Fill { rect, .. } | Op::Frame { rect, .. } => {
                (rect.origin.y, rect.origin.y + rect.size.height)
            }
            Op::Line { pts, .. } | Op::Poly { pts, .. } => pts
                .iter()
                .fold((f64::MAX, f64::MIN), |(lo, hi), p| (lo.min(p.1), hi.max(p.1))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CtrlKind {
    /// Classify candidate `row` of the day's uncategorized list.
    Classify(usize),
    Period,
    /// Show/hide calendar `i` of the settings list.
    Calendar(usize),
}

/// A control to place at `frame`.
#[derive(Debug, Clone)]
struct Ctrl {
    frame: NSRect,
    kind: CtrlKind,
    selected: Option<usize>,
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w.max(0.0), h.max(0.0)))
}

fn path(pts: &[(f64, f64)]) -> Retained<NSBezierPath> {
    let p = NSBezierPath::bezierPath();
    if let Some(&(x, y)) = pts.first() {
        p.moveToPoint(NSPoint::new(x, y));
    }
    for &(x, y) in pts.iter().skip(1) {
        p.lineToPoint(NSPoint::new(x, y));
    }
    p
}

fn draw(op: &Op) {
    match op {
        Op::Text { text, rect, font, ink, right } => {
            let style = NSMutableParagraphStyle::new();
            style.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
            if *right {
                style.setAlignment(NSTextAlignment::Right);
            }
            let (f, c) = (font.ns(), ink.color());
            let values: [&AnyObject; 3] = [&f, &c, &style];
            // SAFETY: reading AppKit's exported attribute-name constants.
            let keys = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName, NSParagraphStyleAttributeName] };
            let attrs: Retained<NSDictionary<NSAttributedStringKey, AnyObject>> =
                NSDictionary::from_slices(&keys, &values);
            // SAFETY: valid attribute dictionary; called inside drawRect:.
            unsafe { NSString::from_str(text).drawInRect_withAttributes(*rect, Some(&attrs)) };
        }
        Op::Fill { rect, ink, radius } => {
            ink.color().setFill();
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(*rect, *radius, *radius).fill();
        }
        Op::Frame { rect, ink, radius, width } => {
            ink.color().setStroke();
            let inset = NSRect::new(
                NSPoint::new(rect.origin.x + width / 2.0, rect.origin.y + width / 2.0),
                NSSize::new((rect.size.width - width).max(0.0), (rect.size.height - width).max(0.0)),
            );
            let p = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(inset, *radius, *radius);
            p.setLineWidth(*width);
            p.stroke();
        }
        Op::Line { pts, ink, width, dash } => {
            ink.color().setStroke();
            let p = path(pts);
            p.setLineWidth(*width);
            p.setLineCapStyle(NSLineCapStyle::Round);
            p.setLineJoinStyle(NSLineJoinStyle::Round);
            if *dash {
                let pattern = [4.0_f64, 3.0];
                // SAFETY: `pattern` outlives the call; AppKit copies it.
                unsafe { p.setLineDash_count_phase(pattern.as_ptr(), 2, 0.0) };
                p.setLineCapStyle(NSLineCapStyle::Butt);
            }
            p.stroke();
        }
        Op::Poly { pts, ink } => {
            ink.color().setFill();
            let p = path(pts);
            p.closePath();
            p.fill();
        }
    }
}

// ---------------------------------------------------------------------------
// Layout: Model -> ops (pure, testable)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Layout {
    ops: Vec<Op>,
    ctrls: Vec<Ctrl>,
    tips: Vec<(NSRect, String)>,
}

impl Layout {
    fn text(&mut self, text: impl Into<String>, x: f64, y: f64, w: f64, font: Font, ink: Ink) {
        let r = rect(x, y, w, font.line_height());
        self.ops.push(Op::Text { text: text.into(), rect: r, font, ink, right: false });
    }

    fn text_right(&mut self, text: impl Into<String>, x: f64, y: f64, w: f64, font: Font, ink: Ink) {
        let r = rect(x, y, w, font.line_height());
        self.ops.push(Op::Text { text: text.into(), rect: r, font, ink, right: true });
    }

    fn fill(&mut self, r: NSRect, ink: Ink, radius: f64) {
        self.ops.push(Op::Fill { rect: r, ink, radius });
    }

    fn line(&mut self, pts: Vec<(f64, f64)>, ink: Ink, width: f64) {
        self.ops.push(Op::Line { pts, ink, width, dash: false });
    }

    fn tip(&mut self, r: NSRect, text: String) {
        self.tips.push((r, text));
    }

    /// Card background with a border; returns the inner content origin and
    /// width.
    fn card(&mut self, x: f64, y: f64, w: f64, h: f64) -> (f64, f64, f64) {
        self.fill(rect(x, y, w, h), Ink::Card, 10.0);
        self.ops.push(Op::Frame { rect: rect(x, y, w, h), ink: Ink::Border, radius: 10.0, width: 1.0 });
        (x + 16.0, y + 14.0, w - 32.0)
    }

    /// Rounded bar: `fraction` of `w` filled with `ink` over a grid track.
    fn bar(&mut self, x: f64, y: f64, w: f64, fraction: f64, ink: Ink, h: f64) {
        self.fill(rect(x, y, w, h), Ink::Grid, h / 2.0);
        let filled = w * fraction.clamp(0.0, 1.0);
        if filled > 0.5 {
            self.fill(rect(x, y, filled.max(h), h), ink, h / 2.0);
        }
    }

    /// Legend swatches with labels in one row starting at `x`.
    fn legend(&mut self, items: &[(Ink, &str)], x: f64, y: f64) {
        let mut lx = x;
        for &(ink, label) in items {
            self.fill(rect(lx, y + 3.0, 9.0, 9.0), ink, 2.0);
            self.text(label, lx + 13.0, y, 50.0, Font::Small, Ink::Secondary);
            lx += 13.0 + 11.0 * label.chars().count() as f64 + 14.0;
        }
    }
}

/// "HH:MM" of a minute offset from the 04:00 day start.
fn clock(min: f64) -> String {
    let t = ((f64::from(export::DAY_START_HOUR) * 60.0 + min).round() as i64).rem_euclid(1440);
    format!("{:02}:{:02}", t / 60, t % 60)
}

fn day_title(d: NaiveDate) -> String {
    format!("{} 月 {} 日（週{}）", d.month(), d.day(), WEEKDAYS[d.weekday().num_days_from_monday() as usize])
}

fn pct(f: f64) -> String {
    format!("{:.0}%", f * 100.0)
}

/// Project name -> colour, stable by name order.
fn project_colors(segments: &[Segment]) -> HashMap<String, u32> {
    let mut names: Vec<&str> = segments.iter().map(|s| s.project.as_str()).filter(|p| !p.is_empty()).collect();
    names.sort_unstable();
    names.dedup();
    names.iter().enumerate().map(|(i, n)| ((*n).to_owned(), PROJECT_COLORS[i % PROJECT_COLORS.len()])).collect()
}

/// Header: takeaway sentence on the left; hill and focus share on the right.
fn layout_header(l: &mut Layout, d: &DayView, x: f64, y: f64, w: f64) -> f64 {
    let share = d.focus_share();
    l.text(day_title(d.day), x, y + 4.0, w - 300.0, Font::Serif(26.0), Ink::Label);
    let mut line = format!("專注 {}，占活躍時間 {}", fmt_duration(d.totals[Kind::Focus.index()]), pct(share));
    if let Some((start, m)) = d.longest {
        line += &format!("；最長的一段專注 {}，從 {} 開始", fmt_duration(m * 60.0), clock(start));
    }
    line.push('。');
    l.text(line, x, y + 44.0, w - 300.0, Font::Body, Ink::Secondary);

    // The hill: Sisyphus pushes the boulder up the slope as far as the focus
    // share; the ground's colour says how good that is.
    let ground = if share >= HILL_GOOD { FOCUS } else if share >= HILL_OK { OCHRE } else { DISTRACTION };
    let (hw, hh) = (150.0, 68.0);
    let hx = x + w - hw - 110.0;
    let (x0, y0, x1, y1) = (hx + 8.0, y + hh - 6.0, hx + hw - 8.0, y + 10.0);
    l.ops.push(Op::Poly {
        pts: vec![(hx, y + hh), (x0, y0), (x1, y1), (hx + hw, y1 + 2.0), (hx + hw, y + hh)],
        ink: Ink::Rgb(ground, 0.22),
    });
    l.line(vec![(hx, y0 + 1.0), (x0, y0), (x1, y1), (hx + hw, y1 + 2.0)], Ink::Rgb(ground, 1.0), 2.5);
    let len = (x1 - x0).hypot(y1 - y0);
    let (ux, uy) = ((x1 - x0) / len, (y1 - y0) / len);
    let (nx, ny) = (uy, -ux); // upward normal (y grows downward)
    let at = |t: f64, up: f64| (x0 + (x1 - x0) * t + nx * up, y0 + (y1 - y0) * t + ny * up);
    let r = 7.5;
    let t = 0.12 + 0.8 * share.clamp(0.0, 1.0);
    let (bx, by) = at(t, r);
    let (fx, fy) = at(t - 22.0 / len, 0.0);
    let (kx, ky) = at(t - 28.0 / len, 0.0);
    let hip = (fx + ux * 3.0 + nx * 9.0, fy + uy * 3.0 + ny * 9.0);
    let shoulder = (hip.0 + ux * 7.0 + nx * 6.0, hip.1 + uy * 7.0 + ny * 6.0);
    let head = (shoulder.0 + ux * 3.0 + nx * 3.5, shoulder.1 + uy * 3.0 + ny * 3.5);
    let hand = (bx - ux * r * 0.95 + nx * 1.5, by - uy * r * 0.95 + ny * 1.5);
    for limb in [
        vec![(kx, ky), (kx + ux * 3.0 + nx * 5.0, ky + uy * 3.0 + ny * 5.0), hip],
        vec![(fx, fy), (fx + ux * 4.0 + nx * 5.0, fy + uy * 4.0 + ny * 5.0), hip],
        vec![hip, shoulder, hand],
    ] {
        l.line(limb, Ink::Label, 1.6);
    }
    l.fill(rect(head.0 - 2.6, head.1 - 2.6, 5.2, 5.2), Ink::Label, 2.6);
    l.fill(rect(bx - r, by - r, 2.0 * r, 2.0 * r), Ink::Rgb(STONE, 1.0), r);
    l.fill(rect(bx - r * 0.55, by - r * 0.6, r * 0.7, r * 0.7), Ink::Rgb(0xffffff, 0.25), r * 0.35);
    l.text_right(pct(share), x + w - 104.0, y + 10.0, 104.0, Font::Serif(40.0), Ink::Label);
    y + 80.0
}

/// Timeline card: activity | project | calendar lanes, 04:00 at the top.
fn layout_timeline(l: &mut Layout, d: &DayView, now_min: Option<f64>, x: f64, y: f64, w: f64) -> f64 {
    let lanes_top = y + 14.0 + 26.0 + 24.0 + 26.0;
    let body = 24.0 * HOUR_PX;
    let h = lanes_top - y + body + 18.0;
    let (cx, cy, cw) = l.card(x, y, w, h);
    l.text("一天的時間線", cx, cy, cw, Font::Heading, Ink::Label);
    l.text_right("04:00 → 隔天 04:00", cx, cy + 3.0, cw, Font::Small, Ink::Secondary);
    l.legend(
        &[
            (Ink::kind(Kind::Focus), "專注"),
            (Ink::kind(Kind::Distraction), "分心"),
            (Ink::kind(Kind::Neutral), "中性"),
            (Ink::kind(Kind::Rest), "休息"),
        ],
        cx,
        cy + 28.0,
    );
    let ty = |min: f64| lanes_top + min / 60.0 * HOUR_PX;
    let axis_w = 40.0;
    let gap = 8.0;
    let x0 = cx + axis_w;
    let usable = cw - axis_w - 2.0 * gap;
    let proj_w = (usable * 0.2).max(64.0);
    let act_w = (usable - proj_w) * 0.5;
    let cal_w = usable - proj_w - act_w;
    let (act_x, proj_x, cal_x) = (x0, x0 + act_w + gap, x0 + act_w + proj_w + 2.0 * gap);
    for (lx, name) in [(act_x, "活動"), (proj_x, "專案"), (cal_x, "行事曆")] {
        l.text(name, lx, lanes_top - 24.0, 80.0, Font::SmallBold, Ink::Secondary);
    }
    // Hour grid.
    for hour in 0..=24 {
        let gy = ty(f64::from(hour) * 60.0);
        l.fill(rect(x0, gy, cw - axis_w, 1.0), Ink::Grid, 0.0);
        let ink = if hour % 2 == 0 { Ink::Secondary } else { Ink::Tertiary };
        l.text(clock(f64::from(hour) * 60.0), cx, gy - 7.0, axis_w - 6.0, Font::Small, ink);
    }
    // Activity lane.
    for s in &d.segments {
        let (top, bottom) = (ty(s.start_min), ty(s.end_min));
        let r = rect(act_x, top, act_w, (bottom - top).max(1.0));
        l.fill(r, Ink::kind(s.kind), 0.0);
        let project = if s.project.is_empty() { String::new() } else { format!(" [{}]", s.project) };
        l.tip(
            r,
            format!(
                "{}–{}（{}）\n{} · {}\n{}{project}",
                clock(s.start_min),
                clock(s.end_min),
                fmt_duration(minutes(s) * 60.0),
                s.kind.label(),
                s.category,
                s.label
            ),
        );
    }
    if d.segments.is_empty() {
        l.text("這天沒有紀錄", act_x + 6.0, ty(30.0), act_w - 6.0, Font::Small, Ink::Secondary);
    }
    // Project lane: consecutive same-project segments < 5 min apart join.
    let colors = project_colors(&d.segments);
    let mut blocks: Vec<(String, f64, f64, f64, f64)> = Vec::new(); // name, start, end, focus, all
    for s in d.segments.iter().filter(|s| !s.project.is_empty()) {
        let f = if s.kind == Kind::Focus { minutes(s) } else { 0.0 };
        match blocks.last_mut() {
            Some(b) if b.0 == s.project && s.start_min - b.2 < 5.0 => {
                b.2 = b.2.max(s.end_min);
                b.3 += f;
                b.4 += minutes(s);
            }
            _ => blocks.push((s.project.clone(), s.start_min, s.end_min, f, minutes(s))),
        }
    }
    for (name, start, end, focus, all) in &blocks {
        let (top, bottom) = (ty(*start), ty(*end));
        let r = rect(proj_x, top, proj_w, (bottom - top).max(2.0));
        l.fill(r, Ink::Rgb(colors[name], 1.0), 3.0);
        if bottom - top >= 16.0 {
            l.text(name.clone(), proj_x + 5.0, top + 1.0, proj_w - 8.0, Font::SmallBold, Ink::Rgb(0xffffff, 1.0));
        }
        l.tip(
            r,
            format!("{name}\n{}–{}\n專注 {} / 共 {}", clock(*start), clock(*end), fmt_duration(focus * 60.0), fmt_duration(all * 60.0)),
        );
    }
    // Calendar lane.
    let origin = day_start(d.day);
    match &d.events {
        Err(Denied::NoAccess) => {
            l.text("沒有行事曆權限", cal_x + 4.0, ty(20.0), cal_w - 4.0, Font::SmallBold, Ink::Secondary);
            l.text("系統設定 → 隱私權與安全性", cal_x + 4.0, ty(20.0) + 16.0, cal_w - 4.0, Font::Small, Ink::Tertiary);
            l.text("→ 行事曆 → 開啟 aw-timer", cal_x + 4.0, ty(20.0) + 30.0, cal_w - 4.0, Font::Small, Ink::Tertiary);
        }
        Ok(events) => {
            let all_day: Vec<&str> = events.iter().filter(|e| e.all_day).map(|e| e.title.as_str()).collect();
            if !all_day.is_empty() {
                l.text(format!("全天：{}", all_day.join("、")), cal_x + 50.0, lanes_top - 24.0, cal_w - 50.0, Font::Small, Ink::Secondary);
            }
            let timed: Vec<&Event> = events.iter().filter(|e| !e.all_day).collect();
            if timed.is_empty() {
                l.text("這天沒有行程", cal_x + 4.0, ty(20.0), cal_w - 4.0, Font::Small, Ink::Tertiary);
                l.text("要顯示哪些行事曆，可在「設定」選擇", cal_x + 4.0, ty(20.0) + 16.0, cal_w - 4.0, Font::Small, Ink::Tertiary);
            }
            for e in timed {
                let s = ((e.start - origin).num_seconds() as f64 / 60.0).clamp(0.0, 1440.0);
                let t = ((e.end - origin).num_seconds() as f64 / 60.0).clamp(0.0, 1440.0);
                if t <= s {
                    continue;
                }
                let (top, bottom) = (ty(s), ty(t));
                let r = rect(cal_x, top, cal_w, (bottom - top).max(4.0));
                l.fill(r, Ink::Background, 4.0);
                l.fill(r, Ink::Rgb(e.color, 0.16), 4.0);
                l.ops.push(Op::Frame { rect: r, ink: Ink::Rgb(e.color, 1.0), radius: 4.0, width: 1.5 });
                if bottom - top >= 16.0 {
                    l.text(e.title.clone(), cal_x + 6.0, top + 1.0, cal_w - 10.0, Font::SmallBold, Ink::Label);
                }
                if bottom - top >= 30.0 {
                    l.text(format!("{}–{}", clock(s), clock(t)), cal_x + 6.0, top + 15.0, cal_w - 10.0, Font::Small, Ink::Secondary);
                }
                l.tip(r, format!("{}\n{}–{}\n{}", e.title, clock(s), clock(t), e.calendar));
            }
        }
    }
    // "Now" marker on the current logical day.
    if let Some(m) = now_min {
        let ny = ty(m);
        l.ops.push(Op::Line { pts: vec![(x0 - 6.0, ny), (cx + cw, ny)], ink: Ink::Label, width: 1.5, dash: true });
    }
    y + h
}

/// Day totals as 2×2 tiles.
fn layout_totals(l: &mut Layout, d: &DayView, x: f64, y: f64, w: f64) -> f64 {
    let gap = 10.0;
    let tw = (w - gap) / 2.0;
    let th = 70.0;
    for (i, k) in [Kind::Focus, Kind::Distraction, Kind::Neutral, Kind::Rest].into_iter().enumerate() {
        let tx = x + (i % 2) as f64 * (tw + gap);
        let ty = y + (i / 2) as f64 * (th + gap);
        l.fill(rect(tx, ty, tw, th), Ink::Card, 10.0);
        l.ops.push(Op::Frame { rect: rect(tx, ty, tw, th), ink: Ink::Border, radius: 10.0, width: 1.0 });
        l.fill(rect(tx + 14.0, ty + 15.0, 8.0, 8.0), Ink::kind(k), 2.0);
        l.text(k.label(), tx + 28.0, ty + 11.0, tw - 36.0, Font::Small, Ink::Secondary);
        l.text(fmt_duration(d.totals[k.index()]), tx + 14.0, ty + 30.0, tw - 22.0, Font::Serif(24.0), Ink::Label);
    }
    y + 2.0 * th + gap
}

fn layout_projects(l: &mut Layout, d: &DayView, x: f64, y: f64, w: f64) -> f64 {
    let rows = d.projects.len().max(1) as f64;
    let h = 14.0 + 24.0 + 20.0 + rows * 26.0 + 10.0;
    let (cx, cy, cw) = l.card(x, y, w, h);
    l.text("專案", cx, cy, cw, Font::Heading, Ink::Label);
    l.text("深色是專注，淺色是其他活動", cx, cy + 24.0, cw, Font::Small, Ink::Secondary);
    let mut ry = cy + 46.0;
    if d.projects.is_empty() {
        l.text("這一天沒有歸到專案的時間。", cx, ry, cw, Font::Body, Ink::Tertiary);
    }
    let colors = project_colors(&d.segments);
    let max_all = d.projects.iter().map(|p| p.2).fold(1.0, f64::max);
    for (name, focus, all) in &d.projects {
        let color = colors.get(name).copied().unwrap_or(PROJECT_COLORS[0]);
        l.text(name.clone(), cx, ry, 80.0, Font::Bold, Ink::Label);
        let bw = cw - 90.0 - 130.0;
        l.fill(rect(cx + 90.0, ry + 4.0, (bw * all / max_all).max(4.0), 10.0), Ink::Rgb(color, 0.3), 3.0);
        l.fill(rect(cx + 90.0, ry + 4.0, (bw * focus / max_all).max(2.0), 10.0), Ink::Rgb(color, 1.0), 3.0);
        l.text_right(format!("{} / {}", fmt_duration(*focus), fmt_duration(*all)), cx + cw - 124.0, ry, 124.0, Font::Digits, Ink::Label);
        ry += 26.0;
    }
    y + h
}

fn layout_uncategorized(l: &mut Layout, d: &DayView, x: f64, y: f64, w: f64) -> f64 {
    let row_h = 32.0;
    let rows = d.uncategorized.len().max(1) as f64;
    let h = 14.0 + 24.0 + 20.0 + 8.0 + rows * row_h + 8.0;
    let (cx, cy, cw) = l.card(x, y, w, h);
    l.text(format!("未分類 {} 項", d.uncategorized.len()), cx, cy, cw, Font::Heading, Ink::Label);
    l.text("選一個類型就會記住，之後同樣的視窗自動歸類", cx, cy + 24.0, cw, Font::Small, Ink::Secondary);
    let mut ry = cy + 50.0;
    if d.uncategorized.is_empty() {
        l.text("這一天沒有未分類的項目。", cx, ry + 4.0, cw, Font::Body, Ink::Tertiary);
    }
    let seg_w = 150.0;
    for (i, c) in d.uncategorized.iter().enumerate() {
        l.text_right(fmt_duration(c.seconds), cx, ry + 5.0, 46.0, Font::Digits, Ink::Secondary);
        let lw = cw - 56.0 - seg_w - 8.0;
        l.text(c.label.clone(), cx + 56.0, ry + 4.0, lw, Font::Body, Ink::Label);
        l.tip(rect(cx + 56.0, ry, lw, row_h - 4.0), c.label.clone());
        l.ctrls.push(Ctrl { frame: rect(cx + cw - seg_w, ry, seg_w, 24.0), kind: CtrlKind::Classify(i), selected: None });
        if i + 1 < d.uncategorized.len() {
            l.fill(rect(cx + 56.0, ry + row_h - 4.0, cw - 56.0, 1.0), Ink::Grid, 0.0);
        }
        ry += row_h;
    }
    y + h
}

fn layout_trends(l: &mut Layout, t: &TrendView, x: f64, y: f64, w: f64) -> f64 {
    // Period chart.
    let chart_h = 240.0;
    let h1 = 14.0 + 30.0 + 22.0 + chart_h + 30.0;
    let (cx, cy, cw) = l.card(x, y, w, h1);
    l.text("每週・每月・每年", cx, cy + 3.0, cw, Font::Heading, Ink::Label);
    let selected = Period::ALL.iter().position(|p| *p == t.period);
    l.ctrls.push(Ctrl { frame: rect(cx + cw - 150.0, cy, 150.0, 24.0), kind: CtrlKind::Period, selected });
    let span = match t.period {
        Period::Year => format!("{} 年", t.from.year()),
        _ => format!("{}/{}–{}/{}", t.from.month(), t.from.day(), t.to.month(), t.to.day()),
    };
    let unit = if t.period == Period::Year { "每月" } else { "每天" };
    l.text(format!("{span}，{unit}各類型的時數"), cx, cy + 32.0, cw - 260.0, Font::Small, Ink::Secondary);
    l.legend(
        &[
            (Ink::kind(Kind::Focus), "專注"),
            (Ink::kind(Kind::Distraction), "分心"),
            (Ink::kind(Kind::Neutral), "中性"),
            (Ink::kind(Kind::Rest), "休息"),
        ],
        cx + cw - 240.0,
        cy + 32.0,
    );
    let top = cy + 62.0;
    let max_h = t.buckets.iter().map(|b| b.secs.iter().sum::<f64>()).fold(0.0, f64::max) / 3600.0;
    let step = [1.0, 2.0, 4.0, 5.0, 10.0, 20.0, 25.0, 50.0, 100.0].into_iter().find(|s| max_h / s <= 4.0).unwrap_or(200.0);
    let ymax = ((max_h / step).ceil() * step).max(step);
    let axis = 34.0;
    let plot_w = cw - axis;
    let yof = |hours: f64| top + chart_h - hours / ymax * chart_h;
    let mut tick = 0.0;
    while tick <= ymax + 1e-9 {
        let ink = if tick == 0.0 { Ink::Tertiary } else { Ink::Grid };
        l.fill(rect(cx + axis, yof(tick), plot_w, 1.0), ink, 0.0);
        l.text_right(format!("{tick}h"), cx, yof(tick) - 7.0, axis - 6.0, Font::Small, Ink::Secondary);
        tick += step;
    }
    let n = t.buckets.len().max(1) as f64;
    let col = plot_w / n;
    let bw = (col * 0.7).min(32.0);
    let every = (n / (plot_w / 30.0)).ceil().max(1.0) as usize;
    for (i, b) in t.buckets.iter().enumerate() {
        let bx = cx + axis + i as f64 * col + (col - bw) / 2.0;
        let mut acc = 0.0;
        for k in Kind::ALL {
            let v = b.secs[k.index()] / 3600.0;
            if v <= 0.0 {
                continue;
            }
            let (y_top, y_bot) = (yof(acc + v), yof(acc));
            l.fill(rect(bx, y_top, bw, (y_bot - y_top).max(0.5)), Ink::kind(k), 2.0);
            acc += v;
        }
        let total: f64 = b.secs.iter().sum();
        let lines: Vec<String> = Kind::ALL
            .iter()
            .filter(|k| b.secs[k.index()] > 0.0)
            .map(|k| format!("{} {}", k.label(), fmt_duration(b.secs[k.index()])))
            .collect();
        let body = if total > 0.0 { lines.join("\n") } else { "沒有紀錄".to_owned() };
        l.tip(rect(cx + axis + i as f64 * col, top, col, chart_h), format!("{}\n{body}", b.tip));
        if i % every == 0 {
            let (ink, font) = if b.selected { (Ink::Label, Font::SmallBold) } else { (Ink::Secondary, Font::Small) };
            // Label left-aligned on its column; short labels stay within it.
            l.text(b.label.clone(), cx + axis + i as f64 * col + (col - 22.0).max(0.0) / 2.0, top + chart_h + 6.0, col.max(24.0), font, ink);
        }
    }
    let y2 = y + h1 + 16.0;

    // Categories in the same period.
    let rows = t.categories.len().max(1) as f64;
    let h2 = 14.0 + 24.0 + 20.0 + rows * 26.0 + 10.0;
    let (cx, cy, cw) = l.card(x, y2, w, h2);
    l.text("同一期間的分類", cx, cy, cw, Font::Heading, Ink::Label);
    l.text("每個分類的總時數與占比，顏色是它屬於的類型", cx, cy + 24.0, cw, Font::Small, Ink::Secondary);
    let total: f64 = t.categories.iter().map(|c| c.2).sum::<f64>().max(1.0);
    let max = t.categories.first().map_or(1.0, |c| c.2.max(1.0));
    let mut ry = cy + 46.0;
    if t.categories.is_empty() {
        l.text("這段期間沒有紀錄。", cx, ry, cw, Font::Body, Ink::Tertiary);
    }
    for (cat, kind, sec) in &t.categories {
        l.text(cat.clone(), cx, ry, 110.0, Font::Body, Ink::Label);
        l.bar(cx + 116.0, ry + 4.0, cw - 116.0 - 130.0, sec / max, Ink::kind(*kind), 10.0);
        l.text_right(format!("{:.1}h · {}", sec / 3600.0, pct(sec / total)), cx + cw - 124.0, ry, 124.0, Font::Digits, Ink::Label);
        ry += 26.0;
    }
    y2 + h2
}

/// Settings: which calendars the timeline shows, grouped by account.
fn layout_settings(l: &mut Layout, m: &Model, x: f64, y: f64, w: f64) -> f64 {
    let w = w.min(640.0);
    let row_h = 26.0;
    let cals = match &m.calendars {
        Some(Ok(c)) => c.as_slice(),
        _ => &[],
    };
    let groups = {
        let mut g: Vec<&str> = cals.iter().map(|c| c.source.as_str()).collect();
        g.dedup();
        g.len()
    };
    let body = if cals.is_empty() { 60.0 } else { cals.len() as f64 * row_h + groups as f64 * 30.0 };
    let h = 14.0 + 24.0 + 20.0 + 10.0 + body + 10.0;
    let (cx, cy, cw) = l.card(x, y, w, h);
    l.text("行事曆", cx, cy, cw, Font::Heading, Ink::Label);
    l.text("勾選的行事曆會顯示在「這一天」的時間線上，改了立刻生效。", cx, cy + 24.0, cw, Font::Small, Ink::Secondary);
    let mut ry = cy + 54.0;
    match &m.calendars {
        Some(Err(Denied::NoAccess)) => {
            l.text("aw-timer 沒有讀取行事曆的權限。", cx, ry, cw, Font::Bold, Ink::Label);
            l.text("到「系統設定 → 隱私權與安全性 → 行事曆」打開 aw-timer，再回到這裡按 ↻。", cx, ry + 22.0, cw, Font::Small, Ink::Secondary);
        }
        Some(Ok(_)) if cals.is_empty() => {
            l.text("這台 Mac 上沒有行事曆。", cx, ry, cw, Font::Body, Ink::Tertiary);
        }
        None => l.text("載入中…", cx, ry, cw, Font::Body, Ink::Tertiary),
        Some(Ok(_)) => {}
    }
    let mut last_source: Option<&str> = None;
    for (i, c) in cals.iter().enumerate() {
        if last_source != Some(c.source.as_str()) {
            if last_source.is_some() {
                ry += 6.0;
            }
            l.text(c.source.clone(), cx, ry, cw, Font::SmallBold, Ink::Secondary);
            ry += 24.0;
            last_source = Some(c.source.as_str());
        }
        l.fill(rect(cx + 2.0, ry + 6.0, 10.0, 10.0), Ink::Rgb(c.color, 1.0), 5.0);
        let shown = !m.hidden.contains(&c.id);
        l.ctrls.push(Ctrl {
            frame: rect(cx + 20.0, ry, cw - 20.0, 22.0),
            kind: CtrlKind::Calendar(i),
            selected: Some(usize::from(shown)),
        });
        ry += row_h;
    }
    y + h
}

/// Lay out the whole page for content width `width`.
fn layout(m: &Model, width: f64, now_min: Option<f64>) -> (Layout, f64) {
    let mut l = Layout::default();
    let (x, w) = (MARGIN, width - 2.0 * MARGIN);
    let Some(day) = m.day.as_ref().filter(|_| m.tab != Tab::Settings) else {
        let y = layout_settings(&mut l, m, x, 16.0, w);
        return (l, y + 28.0);
    };
    let mut y = layout_header(&mut l, day, x, 12.0, w) + 12.0;
    match (m.tab, &m.trend) {
        (Tab::Trends, Some(t)) => y = layout_trends(&mut l, t, x, y, w),
        _ => {
            let gap = 16.0;
            if width >= TWO_COLUMNS_W {
                let left_w = (w - gap) * 0.58;
                let (rx, rw) = (x + left_w + gap, w - left_w - gap);
                let left_end = layout_timeline(&mut l, day, now_min, x, y, left_w);
                let mut ry = layout_totals(&mut l, day, rx, y, rw) + gap;
                ry = layout_projects(&mut l, day, rx, ry, rw) + gap;
                ry = layout_uncategorized(&mut l, day, rx, ry, rw);
                y = left_end.max(ry);
            } else {
                y = layout_totals(&mut l, day, x, y, w) + gap;
                y = layout_timeline(&mut l, day, now_min, x, y, w) + gap;
                y = layout_projects(&mut l, day, x, y, w) + gap;
                y = layout_uncategorized(&mut l, day, x, y, w);
            }
        }
    }
    (l, y + 28.0)
}

// ---------------------------------------------------------------------------
// Views and controller
// ---------------------------------------------------------------------------

pub struct ViewIvars {
    ops: RefCell<Vec<Op>>,
}

define_class!(
    // SAFETY: NSView subclass that only overrides drawing hooks; no Drop.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "AwTimerReportView"]
    #[ivars = ViewIvars]
    pub struct ReportView;

    impl ReportView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            let (lo, hi) = (dirty.origin.y, dirty.origin.y + dirty.size.height);
            for op in self.ivars().ops.borrow().iter() {
                let (top, bottom) = op.y_range();
                if top <= hi && bottom >= lo {
                    draw(op);
                }
            }
        }
    }
);

impl ReportView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars { ops: RefCell::new(Vec::new()) });
        // SAFETY: designated NSView initializer.
        unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, MIN_W, 400.0)] }
    }
}

type Loaded = Result<Model, String>;

/// Views alive only while the window is open.
struct Open {
    window: Retained<NSWindow>,
    scroll: Retained<NSScrollView>,
    doc: Retained<ReportView>,
    status: Retained<NSTextField>,
    tabs: Retained<NSSegmentedControl>,
    next: Retained<NSButton>,
    ctrls: Vec<Retained<NSView>>,
    /// Tooltip texts; AppKit does not retain a tooltip's owner.
    tips: Vec<Retained<NSString>>,
}

pub struct ControllerIvars {
    config_path: Option<String>,
    /// Standalone `report -w`: closing the window ends the process.
    quit_on_close: bool,
    day: Cell<NaiveDate>,
    tab: Cell<Tab>,
    period: Cell<Period>,
    open: RefCell<Option<Open>>,
    data: RefCell<Option<Model>>,
    /// Bumped per request so that a slow, stale load is ignored.
    seq: Cell<u64>,
    inbox: Arc<Mutex<Option<(u64, Loaded)>>>,
    /// Transcript index kept while the window is open, so reloads only read
    /// newly appended lines.
    code: Arc<Mutex<CodeIndex>>,
    poll: RefCell<Option<Retained<NSTimer>>>,
    loaded_at: Cell<Option<Instant>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and we add no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AwTimerReportController"]
    #[ivars = ControllerIvars]
    pub struct ReportController;

    impl ReportController {
        #[unsafe(method(prevDay:))]
        fn prev_day(&self, _sender: Option<&AnyObject>) {
            self.go(self.ivars().day.get() - Duration::days(1));
        }

        #[unsafe(method(nextDay:))]
        fn next_day(&self, _sender: Option<&AnyObject>) {
            let next = self.ivars().day.get() + Duration::days(1);
            if next <= today() {
                self.go(next);
            }
        }

        #[unsafe(method(goToday:))]
        fn go_today(&self, _sender: Option<&AnyObject>) {
            self.go(today());
        }

        #[unsafe(method(reload:))]
        fn reload_action(&self, _sender: Option<&AnyObject>) {
            self.load();
        }

        #[unsafe(method(switchTab:))]
        fn switch_tab(&self, sender: Option<&AnyObject>) {
            let Some(ctrl) = sender.and_then(|s| s.downcast_ref::<NSSegmentedControl>()) else { return };
            let tab = usize::try_from(ctrl.selectedSegment()).ok().and_then(|i| Tab::ALL.get(i)).copied().unwrap_or(Tab::Day);
            if tab != self.ivars().tab.get() {
                self.ivars().tab.set(tab);
                self.load();
            }
        }

        #[unsafe(method(switchPeriod:))]
        fn switch_period(&self, sender: Option<&AnyObject>) {
            let Some(ctrl) = sender.and_then(|s| s.downcast_ref::<NSSegmentedControl>()) else { return };
            if let Some(&p) = usize::try_from(ctrl.selectedSegment()).ok().and_then(|i| Period::ALL.get(i)) {
                self.ivars().period.set(p);
                self.load();
            }
        }

        /// A calendar checkbox in settings changed; its tag indexes the list.
        #[unsafe(method(toggleCalendar:))]
        fn toggle_calendar(&self, sender: Option<&AnyObject>) {
            let Some(button) = sender.and_then(|s| s.downcast_ref::<NSButton>()) else { return };
            let shown = button.state() == NSControlStateValueOn;
            let saved = {
                let mut data = self.ivars().data.borrow_mut();
                let Some(m) = data.as_mut() else { return };
                let Some(Ok(cals)) = &m.calendars else { return };
                let Some(cal) = usize::try_from(button.tag()).ok().and_then(|i| cals.get(i)) else { return };
                if shown {
                    m.hidden.remove(&cal.id);
                } else {
                    m.hidden.insert(cal.id.clone());
                }
                config::save_hidden_calendars(&m.hidden)
            };
            if let Err(e) = saved {
                self.set_status(&format!("儲存失敗：{e}"));
            }
        }

        /// A classification segment was clicked; the control's tag is the
        /// row in the day's uncategorized list.
        #[unsafe(method(classify:))]
        fn classify(&self, sender: Option<&AnyObject>) {
            let Some(ctrl) = sender.and_then(|s| s.downcast_ref::<NSSegmentedControl>()) else { return };
            let (row, seg) = (ctrl.tag(), ctrl.selectedSegment());
            let choice = usize::try_from(seg).ok().and_then(|i| CHOICES.get(i));
            let cand = usize::try_from(row)
                .ok()
                .and_then(|i| self.ivars().data.borrow().as_ref()?.day.as_ref()?.uncategorized.get(i).cloned());
            let (Some(&(category, kind)), Some(cand)) = (choice, cand) else { return };
            let ov = Override { match_on: cand.on, value: cand.value, category: category.into(), kind };
            match config::save_overrides(&[ov]) {
                Ok(()) => self.load(),
                Err(e) => self.set_status(&format!("儲存失敗：{e}")),
            }
        }
    }

    unsafe impl NSObjectProtocol for ReportController {}

    unsafe impl NSWindowDelegate for ReportController {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _n: &NSNotification) {
            // Drop every view and the data; AppKit keeps the window alive
            // until the close finishes.
            let iv = self.ivars();
            iv.seq.set(iv.seq.get() + 1);
            self.stop_polling();
            iv.data.replace(None);
            iv.open.replace(None);
            if let Ok(mut code) = iv.code.try_lock() {
                *code = CodeIndex::default();
            }
            if iv.quit_on_close {
                NSApplication::sharedApplication(MainThreadMarker::from(self)).terminate(None);
            }
        }

        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, _n: &NSNotification) {
            self.render();
        }

        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _n: &NSNotification) {
            let iv = self.ivars();
            let stale = iv.loaded_at.get().is_none_or(|t| t.elapsed().as_secs() >= STALE_SECS);
            if iv.day.get() == today() && stale && iv.poll.borrow().is_none() {
                self.load();
            }
        }
    }
);

/// The current logical day (it starts at 04:00).
fn today() -> NaiveDate {
    logical_day(Utc::now())
}

/// Load one page on the calling (worker) thread.
fn load_model(day: NaiveDate, tab: Tab, period: Period, config_path: Option<&str>, code: &Mutex<CodeIndex>) -> Loaded {
    let hidden = config::hidden_calendars();
    if tab == Tab::Settings {
        return Ok(Model { tab, day: None, trend: None, calendars: Some(calendar::calendars()), hidden });
    }
    let (settings, mut cat, _) = config::load(config_path)?;
    let mut client = AwClient::new(&settings.server_url);
    let (from, to) = match tab {
        Tab::Trends => period.range(day),
        _ => (day, day),
    };
    let mut code = code.lock().unwrap_or_else(|e| e.into_inner());
    let (acts, ..) =
        tracker::load_day(&mut client, &mut cat, &mut code, day_start(from), day_start(to + Duration::days(1)))
            .map_err(|e| format!("連不到 ActivityWatch（{e}）。請確認它有在執行。"))?;
    drop(code);
    let events = calendar::events(day_start(day), day_start(day + Duration::days(1)), &hidden);
    let day_view = build_day(day, &acts, events, settings.max_interruption_min);
    let trend = (tab == Tab::Trends).then(|| build_trend(period, day, &acts));
    Ok(Model { tab, day: Some(day_view), trend, calendars: None, hidden })
}

impl ReportController {
    pub fn new(mtm: MainThreadMarker, config_path: Option<String>) -> Retained<Self> {
        Self::with_options(mtm, config_path, false)
    }

    fn with_options(mtm: MainThreadMarker, config_path: Option<String>, quit_on_close: bool) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ControllerIvars {
            config_path,
            quit_on_close,
            day: Cell::new(today()),
            tab: Cell::new(Tab::Day),
            period: Cell::new(Period::Week),
            open: RefCell::new(None),
            data: RefCell::new(None),
            seq: Cell::new(0),
            inbox: Arc::new(Mutex::new(None)),
            code: Arc::new(Mutex::new(CodeIndex::default())),
            poll: RefCell::new(None),
            loaded_at: Cell::new(None),
        });
        // SAFETY: plain NSObject initializer.
        unsafe { msg_send![super(this), init] }
    }

    /// Open (or bring forward) the window on today's report.
    pub fn show(&self) {
        let mtm = MainThreadMarker::from(self);
        if self.ivars().open.borrow().is_none() {
            let open = self.build_window(mtm);
            self.ivars().open.replace(Some(open));
            self.ivars().day.set(today());
            self.load();
        } else if self.ivars().day.get() != today() {
            self.go(today());
        }
        crate::menubar::activate(mtm);
        if let Some(open) = self.ivars().open.borrow().as_ref() {
            open.window.makeKeyAndOrderFront(None);
        }
    }

    fn build_window(&self, mtm: MainThreadMarker) -> Open {
        ensure_main_menu(mtm);
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        // SAFETY: standard designated initializer.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm), rect(0.0, 0.0, 1120.0, 900.0), style, NSBackingStoreType::Buffered, false)
        };
        // SAFETY: we keep our own reference and drop it in windowWillClose:.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(ns_string!("aw-timer 報表"));
        window.setContentMinSize(NSSize::new(MIN_W, 480.0));
        window.center();
        window.setFrameAutosaveName(ns_string!("aw-timer-report-v2"));
        window.setDelegate(Some(ProtocolObject::from_ref(self)));
        let content = window.contentView().expect("window has a content view");
        let bounds = content.bounds();
        let (cw, ch) = (bounds.size.width, bounds.size.height);

        // Top bar (fixed): tabs, status, day navigation.
        let bar = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, ch - TOP_BAR, cw, TOP_BAR));
        bar.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin);
        let target: &AnyObject = self;
        let labels = NSArray::from_retained_slice(&[
            NSString::from_str("這一天"),
            NSString::from_str("每週・每月・每年"),
            NSString::from_str("設定"),
        ]);
        // SAFETY: `self` implements switchTab: and outlives the window.
        let tabs = unsafe {
            NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
                &labels, NSSegmentSwitchTracking::SelectOne, Some(target), Some(sel!(switchTab:)), mtm)
        };
        tabs.setSelectedSegment(0);
        tabs.setFrame(rect(MARGIN, 15.0, 300.0, 26.0));
        bar.addSubview(&tabs);
        let status = NSTextField::labelWithString(ns_string!(""), mtm);
        status.setFont(Some(&NSFont::systemFontOfSize(12.0)));
        status.setTextColor(Some(&NSColor::secondaryLabelColor()));
        status.setFrame(rect(MARGIN + 312.0, 19.0, (cw - MARGIN - 312.0 - 260.0).max(80.0), 17.0));
        status.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        bar.addSubview(&status);
        let button = |label: &str, action, key: &str, cmd: bool, x: f64, w: f64| {
            // SAFETY: `self` implements `action` and outlives the window.
            let b = unsafe { NSButton::buttonWithTitle_target_action(&NSString::from_str(label), Some(target), Some(action), mtm) };
            b.setKeyEquivalent(&NSString::from_str(key));
            b.setKeyEquivalentModifierMask(if cmd { NSEventModifierFlags::Command } else { NSEventModifierFlags::empty() });
            b.setFrame(rect(cw - MARGIN - x, 14.0, w, 28.0));
            b.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
            bar.addSubview(&b);
            b
        };
        // Arrow keys switch days; ⌘R reloads; ⌘T jumps to today.
        button("↻", sel!(reload:), "r", true, 36.0, 36.0);
        let next = button("▶", sel!(nextDay:), "\u{F703}", false, 84.0, 40.0);
        button("今天", sel!(goToday:), "t", true, 146.0, 60.0);
        button("◀", sel!(prevDay:), "\u{F702}", false, 188.0, 40.0);
        content.addSubview(&bar);

        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), rect(0.0, 0.0, cw, ch - TOP_BAR));
        scroll.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
        scroll.setHasVerticalScroller(true);
        scroll.setDrawsBackground(false);
        let doc = ReportView::new(mtm);
        scroll.setDocumentView(Some(&doc));
        content.addSubview(&scroll);

        Open { window, scroll, doc, status, tabs, next, ctrls: Vec::new(), tips: Vec::new() }
    }

    fn go(&self, day: NaiveDate) {
        self.ivars().day.set(day);
        self.load();
    }

    fn set_status(&self, text: &str) {
        if let Some(open) = self.ivars().open.borrow().as_ref() {
            open.status.setStringValue(&NSString::from_str(text));
        }
    }

    /// Start loading the selected page on a worker thread; a short-lived
    /// timer picks up the result on the main thread, then stops.
    fn load(&self) {
        let iv = self.ivars();
        if iv.open.borrow().is_none() {
            return;
        }
        let seq = iv.seq.get() + 1;
        iv.seq.set(seq);
        self.update_header();
        self.set_status("載入中…");
        let (day, tab, period) = (iv.day.get(), iv.tab.get(), iv.period.get());
        let (path, inbox, code) = (iv.config_path.clone(), Arc::clone(&iv.inbox), Arc::clone(&iv.code));
        std::thread::spawn(move || {
            let result = load_model(day, tab, period, path.as_deref(), &code);
            *inbox.lock().unwrap_or_else(|e| e.into_inner()) = Some((seq, result));
        });
        if iv.poll.borrow().is_none() {
            let this = self.retain();
            let block = RcBlock::new(move |_t: NonNull<NSTimer>| this.take_result());
            // SAFETY: the block is retained by the timer; the timer is
            // invalidated in `stop_polling`.
            let timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.03, true, &block) };
            iv.poll.replace(Some(timer));
        }
    }

    fn take_result(&self) {
        let iv = self.ivars();
        let Some((seq, result)) = iv.inbox.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
        if seq != iv.seq.get() {
            return; // stale; the newer request is still running
        }
        self.stop_polling();
        match result {
            Ok(model) => {
                iv.data.replace(Some(model));
                iv.loaded_at.set(Some(Instant::now()));
                self.set_status(&format!("更新於 {}", Local::now().format("%H:%M")));
            }
            Err(e) => {
                iv.data.replace(None);
                self.set_status(&e);
            }
        }
        self.render();
    }

    fn stop_polling(&self) {
        if let Some(t) = self.ivars().poll.replace(None) {
            t.invalidate();
        }
    }

    fn update_header(&self) {
        let iv = self.ivars();
        let Some(open) = iv.open.borrow().as_ref().map(|o| (o.window.clone(), o.next.clone(), o.tabs.clone())) else {
            return;
        };
        let day = iv.day.get();
        open.0.setTitle(&NSString::from_str(&format!("aw-timer 報表 — {}", day_title(day))));
        open.1.setEnabled(day < today());
        open.2.setSelectedSegment(Tab::ALL.iter().position(|t| *t == iv.tab.get()).unwrap_or(0) as isize);
    }

    /// Re-layout for the current width and data; rebuild the controls and
    /// tooltips.
    fn render(&self) {
        let mtm = MainThreadMarker::from(self);
        let iv = self.ivars();
        let mut open_ref = iv.open.borrow_mut();
        let Some(open) = open_ref.as_mut() else { return };
        let width = open.scroll.contentSize().width.max(MIN_W);
        let now = Utc::now();
        let (lay, height) = match iv.data.borrow().as_ref() {
            Some(m) => {
                let now_min = m
                    .day
                    .as_ref()
                    .filter(|d| logical_day(now) == d.day)
                    .map(|d| (now - day_start(d.day)).num_seconds() as f64 / 60.0);
                layout(m, width, now_min)
            }
            None => (Layout::default(), 200.0),
        };
        let height = height.max(open.scroll.contentSize().height);
        open.doc.setFrameSize(NSSize::new(width, height));
        open.doc.ivars().ops.replace(lay.ops);
        for c in open.ctrls.drain(..) {
            c.removeFromSuperview();
        }
        let target: &AnyObject = self;
        let cal_titles: Vec<String> = iv
            .data
            .borrow()
            .as_ref()
            .and_then(|m| m.calendars.as_ref()?.as_ref().ok().map(|c| c.iter().map(|c| c.title.clone()).collect()))
            .unwrap_or_default();
        for c in lay.ctrls {
            let view: Retained<NSView> = match c.kind {
                CtrlKind::Calendar(i) => {
                    let title = NSString::from_str(cal_titles.get(i).map_or("", String::as_str));
                    // SAFETY: `self` implements toggleCalendar: and outlives the window.
                    let b = unsafe {
                        NSButton::checkboxWithTitle_target_action(&title, Some(target), Some(sel!(toggleCalendar:)), mtm)
                    };
                    b.setTag(i as isize);
                    b.setState(if c.selected == Some(1) { NSControlStateValueOn } else { NSControlStateValueOff });
                    b.setFrame(c.frame);
                    Retained::into_super(Retained::into_super(b))
                }
                CtrlKind::Classify(_) | CtrlKind::Period => {
                    let (labels, action): (Vec<&str>, _) = match c.kind {
                        CtrlKind::Period => (Period::ALL.iter().map(|p| p.label()).collect(), sel!(switchPeriod:)),
                        _ => (CHOICES.iter().map(|c| c.0).collect(), sel!(classify:)),
                    };
                    let labels: Vec<Retained<NSString>> = labels.into_iter().map(NSString::from_str).collect();
                    let labels = NSArray::from_retained_slice(&labels);
                    // SAFETY: `self` implements both actions and outlives the window.
                    let ctrl = unsafe {
                        NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
                            &labels, NSSegmentSwitchTracking::SelectOne, Some(target), Some(action), mtm)
                    };
                    ctrl.setFrame(c.frame);
                    if let CtrlKind::Classify(row) = c.kind {
                        ctrl.setTag(row as isize);
                    }
                    if let Some(i) = c.selected {
                        ctrl.setSelectedSegment(i as isize);
                    }
                    Retained::into_super(Retained::into_super(ctrl))
                }
            };
            open.doc.addSubview(&view);
            open.ctrls.push(view);
        }
        open.doc.removeAllToolTips();
        open.tips.clear();
        for (r, text) in lay.tips {
            let s = NSString::from_str(&text);
            // SAFETY: an NSString owner is shown as the tooltip text; `tips`
            // keeps it alive until the tooltips are removed.
            unsafe { open.doc.addToolTipRect_owner_userData(r, &s, std::ptr::null_mut()) };
            open.tips.push(s);
        }
        open.doc.setNeedsDisplay(true);
        drop(open_ref);
        self.update_header();
    }
}

/// `aw-timer report -w`: show only the report window; quit when it closes.
pub fn run_standalone(config_path: Option<String>) -> ! {
    let mtm = MainThreadMarker::new().expect("report window must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Accessory);
    let controller = ReportController::with_options(mtm, config_path, true);
    controller.show();
    app.run();
    std::process::exit(0)
}

/// Accessory apps have no main menu; add a minimal one so ⌘W closes the
/// report window. It is never shown (no Dock icon, no menu bar takeover).
fn ensure_main_menu(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    if app.mainMenu().is_some() {
        return;
    }
    let main = NSMenu::new(mtm);
    let app_item = NSMenuItem::new(mtm);
    let sub = NSMenu::new(mtm);
    // SAFETY: performClose: / copy: are handled through the responder chain.
    unsafe {
        for (title, action, key) in [("關閉視窗", sel!(performClose:), "w"), ("拷貝", sel!(copy:), "c")] {
            let it = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm), &NSString::from_str(title), Some(action), &NSString::from_str(key));
            sub.addItem(&it);
        }
    }
    app_item.setSubmenu(Some(&sub));
    main.addItem(&app_item);
    app.setMainMenu(Some(&main));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::tests::act;

    fn sample() -> (NaiveDate, Vec<Activity>) {
        let mut unc = act(40.0, 10.0, Kind::Neutral, UNCATEGORIZED);
        unc.app = "Mystery".into();
        let mut focus = act(0.0, 40.0, Kind::Focus, "寫程式");
        focus.project = Some("CVSD".into());
        let acts = vec![focus, unc, act(50.0, 10.0, Kind::Distraction, "娛樂")];
        (logical_day(acts[0].start), acts)
    }

    #[test]
    fn day_view_totals_projects_and_uncategorized() {
        let (day, acts) = sample();
        let d = build_day(day, &acts, Ok(Vec::new()), 5.0);
        assert!((d.totals[Kind::Focus.index()] - 2400.0).abs() < 7.0);
        assert_eq!(d.projects[0].0, "CVSD");
        assert_eq!(d.uncategorized.len(), 1);
        assert!((d.longest.unwrap().1 - 40.0).abs() < 0.2);
    }

    #[test]
    fn layout_fits_width_and_has_one_classifier_per_uncategorized() {
        let (day, acts) = sample();
        let model = Model {
            tab: Tab::Day,
            day: Some(build_day(day, &acts, Err(Denied::NoAccess), 5.0)),
            trend: None,
            calendars: None,
            hidden: BTreeSet::new(),
        };
        for width in [MIN_W, 1200.0] {
            let (l, height) = layout(&model, width, None);
            assert!(height > 24.0 * HOUR_PX);
            assert_eq!(l.ctrls.iter().filter(|c| matches!(c.kind, CtrlKind::Classify(_))).count(), 1);
            for op in &l.ops {
                if let Op::Text { rect, .. } | Op::Fill { rect, .. } = op {
                    assert!(rect.origin.x >= 0.0 && rect.origin.x + rect.size.width <= width + 0.5, "{op:?}");
                }
            }
        }
    }

    #[test]
    fn trends_bucket_by_period() {
        let (day, acts) = sample();
        for (p, n) in [(Period::Week, 7), (Period::Year, 12)] {
            let t = build_trend(p, day, &acts);
            assert_eq!(t.buckets.len(), n);
            assert!(t.buckets.iter().any(|b| b.selected));
            let sum: f64 = t.buckets.iter().map(|b| b.secs.iter().sum::<f64>()).sum();
            assert!((sum - 3600.0).abs() < 2.0);
        }
        assert_eq!(Period::Month.range(NaiveDate::from_ymd_opt(2026, 2, 10).unwrap()).1.day(), 28);
    }

    #[test]
    fn settings_lists_one_checkbox_per_calendar_with_hidden_unchecked() {
        let cal = |id: &str, source: &str| CalendarInfo { id: id.into(), title: id.into(), source: source.into(), color: 0x3366cc };
        let model = Model {
            tab: Tab::Settings,
            day: None,
            trend: None,
            calendars: Some(Ok(vec![cal("a", "iCloud"), cal("b", "iCloud"), cal("c", "Google")])),
            hidden: BTreeSet::from(["b".to_owned()]),
        };
        let (l, _) = layout(&model, 900.0, None);
        let checks: Vec<Option<usize>> =
            l.ctrls.iter().filter(|c| matches!(c.kind, CtrlKind::Calendar(_))).map(|c| c.selected).collect();
        assert_eq!(checks, vec![Some(1), Some(0), Some(1)]);
    }
}
