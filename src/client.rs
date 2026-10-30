//! Minimal ActivityWatch REST client.

use std::fmt;
use std::time::Duration;

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

/// Thin wrapper around the aw-server API (`/api/0`).
pub struct AwClient {
    base: String,
    agent: ureq::Agent,
    bucket_ids: Option<(String, String)>,
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
            bucket_ids: None,
        }
    }

    fn get(&self, path: &str) -> Result<Value, Unavailable> {
        Ok(self.agent.get(&format!("{}{path}", self.base)).call()?.into_json()?)
    }

    /// Find `(window_bucket_id, afk_bucket_id)` for this host (cached).
    fn resolve_buckets(&mut self) -> Result<(String, String), Unavailable> {
        if let Some(ids) = &self.bucket_ids {
            return Ok(ids.clone());
        }
        let hostname = self.get("/info")?["hostname"].as_str().unwrap_or("").to_owned();
        let buckets = self.get("/buckets/")?;
        let all = buckets
            .as_object()
            .ok_or_else(|| Unavailable("unexpected /buckets/ response".into()))?;
        let pick = |bucket_type: &str| -> Result<String, Unavailable> {
            let mut candidates: Vec<&Value> =
                all.values().filter(|b| b["type"] == bucket_type).collect();
            // Prefer the bucket belonging to this machine (false sorts first).
            candidates.sort_by_key(|b| b["hostname"].as_str() != Some(hostname.as_str()));
            candidates
                .first()
                .and_then(|b| b["id"].as_str())
                .map(str::to_owned)
                .ok_or_else(|| {
                    Unavailable(format!("no '{bucket_type}' bucket yet; is ActivityWatch running?"))
                })
        };
        let ids = (pick("currentwindow")?, pick("afkstatus")?);
        self.bucket_ids = Some(ids.clone());
        Ok(ids)
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
        let (window_id, afk_id) = self.resolve_buckets()?;
        // serde_json::to_string yields a correctly escaped string literal.
        let lit = |s: &str| serde_json::to_string(s).expect("string serialization");
        let query = [
            format!("afk = flood(query_bucket({}));", lit(&afk_id)),
            format!("win = flood(query_bucket({}));", lit(&window_id)),
            r#"not_afk = filter_keyvals(afk, "status", ["not-afk"]);"#.to_owned(),
            "win = filter_period_intersect(win, not_afk);".to_owned(),
            "RETURN = sort_by_timestamp(win);".to_owned(),
        ];
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
