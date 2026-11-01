//! Read events from the macOS Calendar app (EventKit).
//!
//! The first request shows the system permission prompt; its wording comes
//! from `Info.plist`, which `build.rs` embeds in the binary. Calls block the
//! calling thread until the user answers, so use a worker thread.

use std::collections::BTreeSet;
use std::sync::mpsc;

use block2::RcBlock;
use chrono::{DateTime, TimeZone, Utc};
use objc2::runtime::Bool;
use objc2_app_kit::{NSColor, NSColorSpace};
use objc2_event_kit::{EKAuthorizationStatus, EKCalendar, EKEntityType, EKEventStore};
use objc2_foundation::{NSDate, NSError, NSObjectProtocol};
use serde::Serialize;

/// One calendar event.
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub all_day: bool,
    pub calendar: String,
    /// The calendar's colour as `0xRRGGBB` (sRGB).
    pub color: u32,
}

/// One calendar the user can show or hide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarInfo {
    /// EventKit identifier, stable on this Mac.
    pub id: String,
    pub title: String,
    /// Account it belongs to ("iCloud", a Google address, "其他"…).
    pub source: String,
    pub color: u32,
}

/// Why no events could be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    /// The user said no, or access is restricted by policy.
    NoAccess,
}

/// Ask for full Calendar access if it has not been decided yet.
fn ensure_access(store: &EKEventStore) -> Result<(), Denied> {
    // SAFETY: plain class method.
    let status = unsafe { EKEventStore::authorizationStatusForEntityType(EKEntityType::Event) };
    // `Authorized` and `FullAccess` share a value (3).
    if status == EKAuthorizationStatus::FullAccess {
        return Ok(());
    }
    if status != EKAuthorizationStatus::NotDetermined {
        return Err(Denied::NoAccess);
    }
    let (tx, rx) = mpsc::channel::<bool>();
    let block = RcBlock::new(move |granted: Bool, _err: *mut NSError| {
        let _ = tx.send(granted.as_bool());
    });
    // `requestFullAccessToEvents…` is macOS 14+; older systems only have
    // `requestAccessToEntityType:completion:`.
    // SAFETY: the block lives until the completion handler has run (EventKit
    // copies it), and both selectors take this block signature.
    unsafe {
        if store.respondsToSelector(objc2::sel!(requestFullAccessToEventsWithCompletion:)) {
            store.requestFullAccessToEventsWithCompletion(RcBlock::as_ptr(&block));
        } else {
            #[allow(deprecated)]
            store.requestAccessToEntityType_completion(EKEntityType::Event, RcBlock::as_ptr(&block));
        }
    }
    // The prompt waits for the user; give up after 10 minutes.
    match rx.recv_timeout(std::time::Duration::from_secs(600)) {
        Ok(true) => Ok(()),
        _ => Err(Denied::NoAccess),
    }
}

fn to_chrono(d: &NSDate) -> DateTime<Utc> {
    let secs = d.timeIntervalSince1970();
    Utc.timestamp_opt(secs.floor() as i64, ((secs.fract()) * 1e9) as u32).single().unwrap_or_default()
}

/// `0xRRGGBB` of a colour in sRGB; grey if it cannot be converted.
fn rgb(color: &NSColor) -> u32 {
    color.colorUsingColorSpace(&NSColorSpace::sRGBColorSpace()).map_or(0x8e8e93, |c| {
        let ch = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
        (ch(c.redComponent()) << 16) | (ch(c.greenComponent()) << 8) | ch(c.blueComponent())
    })
}

fn info(c: &EKCalendar) -> CalendarInfo {
    // SAFETY: EKCalendar / EKSource property getters.
    unsafe {
        CalendarInfo {
            id: c.calendarIdentifier().to_string(),
            title: c.title().to_string(),
            source: c.source().map_or_else(|| "其他".to_owned(), |s| s.title().to_string()),
            color: rgb(&c.color()),
        }
    }
}

/// Every event calendar on this Mac, grouped by account, then by title.
///
/// # Errors
/// [`Denied::NoAccess`] when Calendar access is not granted.
pub fn calendars() -> Result<Vec<CalendarInfo>, Denied> {
    // SAFETY: EKEventStore's plain initializer.
    let store = unsafe { EKEventStore::new() };
    ensure_access(&store)?;
    // SAFETY: plain getter.
    let all = unsafe { store.calendarsForEntityType(EKEntityType::Event) };
    let mut out: Vec<CalendarInfo> = all.iter().map(|c| info(&c)).collect();
    out.sort_by(|a, b| a.source.cmp(&b.source).then_with(|| a.title.cmp(&b.title)));
    Ok(out)
}

/// Events overlapping `[start, end)` from every calendar not in `hidden`,
/// sorted by start.
///
/// # Errors
/// [`Denied::NoAccess`] when Calendar access is not granted.
pub fn events(start: DateTime<Utc>, end: DateTime<Utc>, hidden: &BTreeSet<String>) -> Result<Vec<Event>, Denied> {
    // SAFETY: EKEventStore's plain initializer.
    let store = unsafe { EKEventStore::new() };
    ensure_access(&store)?;
    let (from, to) = (
        NSDate::dateWithTimeIntervalSince1970(start.timestamp() as f64),
        NSDate::dateWithTimeIntervalSince1970(end.timestamp() as f64),
    );
    // SAFETY: valid dates; `None` means all calendars.
    let found = unsafe {
        let predicate = store.predicateForEventsWithStartDate_endDate_calendars(&from, &to, None);
        store.eventsMatchingPredicate(&predicate)
    };
    let mut out: Vec<Event> = found
        .iter()
        .filter_map(|e| {
            // SAFETY: EKEvent property getters.
            unsafe {
                let cal = e.calendar().map(|c| info(&c));
                if cal.as_ref().is_some_and(|c| hidden.contains(&c.id)) {
                    return None;
                }
                Some(Event {
                    title: e.title().to_string(),
                    start: to_chrono(&e.startDate()),
                    end: to_chrono(&e.endDate()),
                    all_day: e.isAllDay(),
                    calendar: cal.as_ref().map(|c| c.title.clone()).unwrap_or_default(),
                    color: cal.map_or(0x8e8e93, |c| c.color),
                })
            }
        })
        .collect();
    out.sort_by_key(|e| e.start);
    Ok(out)
}
