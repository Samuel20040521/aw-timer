//! Minimal ActivityWatch REST client.

use std::fmt;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

/// The local ActivityWatch server could not be reached or returned garbage.
#[derive(Debug)]
pub struct Unavailable(pub String);

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unavailable {}

impl From<ureq::Error> for Unavailable {
    fn from(e: ureq::Error) -> Self {
        Unavailable(e.to_string())
    }
}

impl From<std::io::Error> for Unavailable {
    fn from(e: std::io::Error) -> Self {
        Unavailable(e.to_string())
    }
}

/// A non-AFK window event.
#[derive(Debug, Clone)]
pub struct WindowEvent {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub app: String,
    pub title: String,
}

#[derive(Deserialize)]
struct RawEvent {
    timestamp: String,
    #[serde(default)]
    duration: f64,
    #[serde(default)]
    data: RawData,
}

#[derive(Deserialize, Default)]
struct RawData {
    #[serde(default)]
    app: String,
    #[serde(default)]
    title: String,
}

/// How long the bucket list is reused before asking the server again.
const BUCKETS_TTL: Duration = Duration::from_secs(60);

/// Thin wrapper around the aw-server API (`/api/0`).
pub struct AwClient {
    base: String,
    agent: ureq::Agent,
    /// `(window bucket ids, afk bucket ids, when fetched)`.
    buckets: Option<(Vec<String>, Vec<String>, Instant)>,
}

impl AwClient {
    /// Create a client for `base_url` (e.g. `http://localhost:5600`).
    pub fn new(base_url: &str) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(10))
            .build();
        Self {
            base: format!("{}/api/0", base_url.trim_end_matches('/')),
            agent,
            buckets: None,
        }
    }

    fn get(&self, path: &str) -> Result<Value, Unavailable> {
        Ok(self.agent.get(&format!("{}{path}", self.base)).call()?.into_json()?)
    }

    /// Every window and AFK bucket, cached for [`BUCKETS_TTL`].
    ///
    /// ActivityWatch names buckets after the hostname, and macOS changes the
    /// hostname with the network (e.g. `Mac.localdomain` →
    /// `…-MacBook-Air.local`), starting new buckets. Reading all of them
    /// keeps the history from before the change.
    fn resolve_buckets(&mut self) -> Result<(Vec<String>, Vec<String>), Unavailable> {
        if let Some((win, afk, at)) = &self.buckets
            && at.elapsed() < BUCKETS_TTL
        {
            return Ok((win.clone(), afk.clone()));
        }
        let buckets = self.get("/buckets/")?;
        let all = buckets
            .as_object()
            .ok_or_else(|| Unavailable("unexpected /buckets/ response".into()))?;
        let pick = |bucket_type: &str| -> Result<Vec<String>, Unavailable> {
            let mut ids: Vec<String> = all
                .values()
                .filter(|b| b["type"] == bucket_type)
                .filter_map(|b| b["id"].as_str().map(str::to_owned))
                .collect();
            ids.sort();
            if ids.is_empty() {
                return Err(Unavailable(format!("no '{bucket_type}' bucket yet; is ActivityWatch running?")));
            }
            Ok(ids)
        };
        let (win, afk) = (pick("currentwindow")?, pick("afkstatus")?);
        self.buckets = Some((win.clone(), afk.clone(), Instant::now()));
        Ok((win, afk))
    }

    /// Window events overlapping non-AFK time in `[start, end)`.
    ///
    /// The AFK intersection is done server-side with the ActivityWatch query
    /// language, the same way the official web UI computes active time.
    pub fn window_events(
        &mut self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<WindowEvent>, Unavailable> {
        let (window_ids, afk_ids) = self.resolve_buckets()?;
        let query = merged_query(&window_ids, &afk_ids);
        let body = json!({
            "timeperiods": [format!("{}/{}", start.to_rfc3339(), end.to_rfc3339())],
            "query": query,
        });
        let resp: Vec<Vec<RawEvent>> = self
            .agent
            .post(&format!("{}/query/", self.base))
            .send_json(body)?
            .into_json()?;
        let mut events = Vec::new();
        for ev in resp.into_iter().next().unwrap_or_default() {
            if ev.duration <= 0.0 {
                continue;
            }
            let start = DateTime::parse_from_rfc3339(&ev.timestamp)
                .map_err(|e| Unavailable(format!("bad timestamp {:?}: {e}", ev.timestamp)))?
                .with_timezone(&Utc);
            let micros = (ev.duration * 1e6).round() as i64;
            events.push(WindowEvent {
                start,
                end: start + chrono::Duration::microseconds(micros),
                app: ev.data.app,
                title: ev.data.title,
            });
        }
        Ok(events)
    }
}

/// Query joining all window buckets and all AFK buckets, keeping window
/// events that overlap non-AFK time.
fn merged_query(window_ids: &[String], afk_ids: &[String]) -> Vec<String> {
    // serde_json::to_string yields a correctly escaped string literal.
    let lit = |s: &str| serde_json::to_string(s).expect("string serialization");
    let mut q = Vec::new();
    for (var, ids) in [("afk", afk_ids), ("win", window_ids)] {
        for (i, id) in ids.iter().enumerate() {
            let events = format!("flood(query_bucket({}))", lit(id));
            q.push(if i == 0 { format!("{var} = {events};") } else { format!("{var} = concat({var}, {events});") });
        }
    }
    q.push(r#"not_afk = filter_keyvals(afk, "status", ["not-afk"]);"#.to_owned());
    q.push("win = filter_period_intersect(win, not_afk);".to_owned());
    q.push("RETURN = sort_by_timestamp(win);".to_owned());
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_query_concats_every_bucket() {
        let q = merged_query(&["w1".into(), "w2".into()], &["a1".into()]);
        assert_eq!(q[0], r#"afk = flood(query_bucket("a1"));"#);
        assert_eq!(q[2], r#"win = concat(win, flood(query_bucket("w2")));"#);
        assert!(q.last().unwrap().starts_with("RETURN"));
    }
}
