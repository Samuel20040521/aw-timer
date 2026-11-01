//! `aw-timer export`: classified activity as JSON rows for dashboards.
//!
//! Days run from 04:00 to 04:00 local time ("logical days"), so a late
//! night counts toward the day it started. Window titles of chat apps are
//! replaced by the app name, since they name other people.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Local, NaiveDate, TimeZone, Utc};
use serde::Serialize;

use crate::categorize::{Categorizer, Kind};
use crate::claude_code::CodeIndex;
use crate::client::AwClient;
use crate::sessions::{Activity, seconds, short_label};
use crate::client::Unavailable;
use crate::tracker;

/// Hour at which a logical day begins.
pub const DAY_START_HOUR: u32 = 4;
/// Categories whose window titles are not exported.
const PRIVATE_CATEGORIES: [&str; 2] = ["溝通", "社群"];
/// Adjacent pieces of the same kind/category/app closer than this merge.
const MERGE_GAP_SECS: f64 = 60.0;

/// One continuous stretch on the timeline.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Segment {
    pub day: String,
    /// Minutes since the start of the logical day (04:00 = 0).
    pub start_min: f64,
    pub end_min: f64,
    pub kind: Kind,
    pub category: String,
    pub app: String,
    pub label: String,
    pub project: String,
}

/// Seconds per logical day and category.
#[derive(Debug, Serialize, PartialEq)]
pub struct DailyRow {
    pub day: String,
    pub category: String,
    pub kind: Kind,
    pub seconds: f64,
}

/// Start of logical day `day` (04:00 local) in UTC.
pub fn day_start(day: NaiveDate) -> DateTime<Utc> {
    let naive = day.and_hms_opt(DAY_START_HOUR, 0, 0).expect("valid time");
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map_or_else(|| Utc.from_utc_datetime(&naive), |t| t.with_timezone(&Utc))
}

/// Logical day containing instant `t`.
pub fn logical_day(t: DateTime<Utc>) -> NaiveDate {
    (t.with_timezone(&Local) - Duration::hours(i64::from(DAY_START_HOUR))).date_naive()
}

/// Split activities at logical-day boundaries.
fn split_days(acts: Vec<Activity>) -> Vec<(NaiveDate, Activity)> {
    let mut out = Vec::with_capacity(acts.len());
    for act in acts {
        let mut piece = act;
        loop {
            let day = logical_day(piece.start);
            let boundary = day_start(day + Duration::days(1));
            if piece.end <= boundary {
                out.push((day, piece));
                break;
            }
            out.push((day, Activity { end: boundary, ..piece.clone() }));
            piece.start = boundary;
        }
    }
    out
}

fn label_for(act: &Activity) -> String {
    if PRIVATE_CATEGORIES.contains(&act.category.as_str()) { act.app.clone() } else { short_label(act) }
}

/// Timeline segments: consecutive pieces with the same kind, category, app
/// and project merge when the gap between them is under a minute; the
/// merged segment keeps the label that covered most of its time.
///
/// Time O(n) for n activities sorted by start.
pub fn segments(acts: Vec<Activity>) -> Vec<Segment> {
    struct Open {
        day: NaiveDate,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        act: Activity,
        labels: BTreeMap<String, f64>,
    }
    let close = |o: Open| {
        let origin = day_start(o.day);
        let label = o.labels.into_iter().max_by(|a, b| a.1.total_cmp(&b.1)).map(|(l, _)| l).unwrap_or_default();
        Segment {
            day: o.day.to_string(),
            start_min: (seconds(o.start - origin) / 60.0 * 10.0).round() / 10.0,
            end_min: (seconds(o.end - origin) / 60.0 * 10.0).round() / 10.0,
            kind: o.act.kind,
            category: o.act.category,
            app: o.act.app,
            label,
            project: o.act.project.unwrap_or_default(),
        }
    };
    let mut out = Vec::new();
    let mut cur: Option<Open> = None;
    for (day, act) in split_days(acts) {
        let label = label_for(&act);
        let secs = act.seconds();
        if let Some(o) = cur.as_mut() {
            let same = o.day == day
                && o.act.kind == act.kind
                && o.act.category == act.category
                && o.act.app == act.app
                && o.act.project == act.project
                && seconds(act.start - o.end) < MERGE_GAP_SECS;
            if same {
                o.end = o.end.max(act.end);
                *o.labels.entry(label).or_default() += secs;
                continue;
            }
        }
        if let Some(o) = cur.take() {
            out.push(close(o));
        }
        let labels = BTreeMap::from([(label, secs)]);
        cur = Some(Open { day, start: act.start, end: act.end, act, labels });
    }
    out.extend(cur.map(close));
    out
}

/// Seconds per logical day and category, ordered by day then category.
pub fn daily(acts: &[Activity]) -> Vec<DailyRow> {
    let mut map: BTreeMap<(NaiveDate, String), (Kind, f64)> = BTreeMap::new();
    for (day, act) in split_days(acts.to_vec()) {
        let e = map.entry((day, act.category.clone())).or_insert((act.kind, 0.0));
        e.1 += act.seconds();
    }
    map.into_iter()
        .map(|((day, category), (kind, s))| DailyRow { day: day.to_string(), category, kind, seconds: s.round() })
        .collect()
}

/// Classified activities of the last `days` logical days (today included).
///
/// # Errors
/// Fails when ActivityWatch cannot be reached.
pub fn load(client: &mut AwClient, cat: &mut Categorizer, days: i64) -> Result<Vec<Activity>, Unavailable> {
    let today = logical_day(Utc::now());
    let start = day_start(today - Duration::days(days.max(1) - 1));
    let end = day_start(today + Duration::days(1));
    let mut code = CodeIndex::default();
    let (acts, ..) = tracker::load_day(client, cat, &mut code, start, end)?;
    Ok(acts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::tests::act;

    #[test]
    fn segments_merge_neighbours_and_split_at_four() {
        let mut a = act(0.0, 10.0, Kind::Focus, "寫程式");
        let mut b = act(10.5, 9.5, Kind::Focus, "寫程式");
        let c = act(20.0, 5.0, Kind::Distraction, "娛樂");
        b.app.clone_from(&a.app);
        a.title = "x".into();
        let segs = segments(vec![a, b, c]);
        assert_eq!(segs.len(), 2, "{segs:?}");
        assert!((segs[0].end_min - segs[0].start_min - 20.0).abs() < 0.2, "{segs:?}");

        // An activity spanning 04:00 is split into two logical days.
        let day = NaiveDate::from_ymd_opt(2026, 11, 2).unwrap();
        let four = day_start(day);
        let mut late = act(0.0, 1.0, Kind::Focus, "寫程式");
        late.start = four - Duration::minutes(30);
        late.end = four + Duration::minutes(15);
        let parts = segments(vec![late]);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1].day, "2026-11-02");
        assert!((parts[0].end_min - 1440.0).abs() < 0.2 && parts[1].start_min.abs() < 0.2);
        let rows = daily(&[act(0.0, 30.0, Kind::Focus, "寫程式")]);
        assert_eq!(rows[0].seconds, 1800.0);
    }
}
