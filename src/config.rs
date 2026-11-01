//! Configuration loading (same TOML format as the Python version).

use std::collections::BTreeSet;
use std::path::PathBuf;

use chrono::Duration;
use serde::{Deserialize, Serialize};

use crate::categorize::{Categorizer, CategoryRule, Override, ProjectRule};
use crate::sound::SoundSettings;

pub const DEFAULT_CONFIG: &str = include_str!("../default_config.toml");

/// The `[timer]` table; see `default_config.toml` for meanings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub server_url: String,
    pub poll_seconds: f64,
    pub break_gap_min: f64,
    pub max_interruption_min: f64,
    pub min_session_min: f64,
    pub break_after_min: f64,
    pub break_repeat_min: f64,
    pub distraction_alert_min: f64,
    pub distraction_merge_sec: f64,
    pub notify_focus_start: bool,
    pub notify_sound: String,
    pub launch_activitywatch: bool,
    pub menubar: bool,
    pub menubar_fps: f64,
    /// Filled from the separate `[sound]` table.
    #[serde(skip)]
    pub sound: SoundSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server_url: "http://localhost:5600".into(),
            poll_seconds: 30.0,
            break_gap_min: 5.0,
            max_interruption_min: 5.0,
            min_session_min: 25.0,
            break_after_min: 50.0,
            break_repeat_min: 15.0,
            distraction_alert_min: 10.0,
            distraction_merge_sec: 60.0,
            notify_focus_start: true,
            notify_sound: "Glass".into(),
            launch_activitywatch: true,
            menubar: true,
            menubar_fps: 12.0,
            sound: SoundSettings::default(),
        }
    }
}

/// Minutes (fractional allowed) to a chrono duration.
pub fn minutes(m: f64) -> Duration {
    Duration::milliseconds((m * 60_000.0).round() as i64)
}

impl Settings {
    pub fn break_gap(&self) -> Duration {
        minutes(self.break_gap_min)
    }
    pub fn max_interruption(&self) -> Duration {
        minutes(self.max_interruption_min)
    }
    pub fn min_session(&self) -> Duration {
        minutes(self.min_session_min)
    }
    pub fn distraction_merge(&self) -> Duration {
        minutes(self.distraction_merge_sec / 60.0)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    timer: Settings,
    #[serde(default)]
    category: Vec<CategoryRule>,
    #[serde(default)]
    project: Vec<ProjectRule>,
    #[serde(default)]
    sound: SoundSettings,
}

/// Parse a config document.
///
/// # Errors
/// Returns a message on TOML errors, unknown keys or bad regexes.
pub fn parse(text: &str, overrides: Vec<Override>) -> Result<(Settings, Categorizer), String> {
    let raw: RawConfig = toml::from_str(text).map_err(|e| e.to_string())?;
    let settings = Settings { sound: raw.sound, ..raw.timer };
    Ok((settings, Categorizer::from_rules(raw.category, overrides, raw.project)?))
}

fn config_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/aw-timer")
}

/// Manual classifications written by `aw-timer report -i`.
pub fn overrides_path() -> PathBuf {
    config_dir().join("overrides.toml")
}

#[derive(Default, Deserialize, Serialize)]
struct OverridesFile {
    #[serde(default, rename = "override")]
    overrides: Vec<Override>,
}

/// Load manual overrides; a missing file means none.
///
/// # Errors
/// Returns a message if the file exists but cannot be parsed.
pub fn load_overrides() -> Result<Vec<Override>, String> {
    let path = overrides_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<OverridesFile>(&text)
            .map(|f| f.overrides)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Add overrides, replacing earlier ones for the same match target.
///
/// # Errors
/// Returns a message on read, parse or write failure.
pub fn save_overrides(new: &[Override]) -> Result<(), String> {
    let mut all = load_overrides()?;
    for o in new {
        all.retain(|x| !(x.match_on == o.match_on && x.value.to_lowercase() == o.value.to_lowercase()));
        all.push(o.clone());
    }
    let body = toml::to_string(&OverridesFile { overrides: all }).map_err(|e| e.to_string())?;
    let text = format!("# 由 `aw-timer report -i` 產生的手動分類，優先於 config.toml 的規則。\n\n{body}");
    let path = overrides_path();
    std::fs::create_dir_all(config_dir()).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Calendars hidden from the report timeline (chosen in its settings tab).
pub fn calendars_path() -> PathBuf {
    config_dir().join("calendars.toml")
}

#[derive(Default, Deserialize, Serialize)]
struct CalendarsFile {
    /// EventKit calendar identifiers.
    #[serde(default)]
    hidden: BTreeSet<String>,
}

/// Identifiers of hidden calendars; a missing or unreadable file hides none.
pub fn hidden_calendars() -> BTreeSet<String> {
    std::fs::read_to_string(calendars_path())
        .ok()
        .and_then(|t| toml::from_str::<CalendarsFile>(&t).ok())
        .map(|f| f.hidden)
        .unwrap_or_default()
}

/// Replace the set of hidden calendars.
///
/// # Errors
/// Returns a message on write failure.
pub fn save_hidden_calendars(hidden: &BTreeSet<String>) -> Result<(), String> {
    let body = toml::to_string(&CalendarsFile { hidden: hidden.clone() }).map_err(|e| e.to_string())?;
    let text = format!("# 由報表視窗的「設定」產生：這些行事曆不顯示在時間線上。\n\n{body}");
    let path = calendars_path();
    std::fs::create_dir_all(config_dir()).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Files whose changes should make a running timer reload its categories.
pub fn watched_files(explicit: Option<&str>) -> Vec<PathBuf> {
    resolve_path(explicit).into_iter().chain([overrides_path()]).collect()
}

/// Where the config comes from: CLI flag > `$AW_TIMER_CONFIG` > user file
/// > built-in default (`None`).
pub fn resolve_path(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(PathBuf::from(p));
    }
    if let Ok(p) = std::env::var("AW_TIMER_CONFIG")
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    let user = config_dir().join("config.toml");
    user.exists().then_some(user)
}

/// Load settings and categories.
///
/// # Errors
/// Returns a human-readable message including the path.
pub fn load(explicit: Option<&str>) -> Result<(Settings, Categorizer, String), String> {
    let overrides = load_overrides()?;
    match resolve_path(explicit) {
        Some(path) => {
            let shown = path.display().to_string();
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{shown}: {e}"))?;
            let (s, c) = parse(&text, overrides).map_err(|e| format!("{shown}: {e}"))?;
            Ok((s, c, shown))
        }
        None => {
            let (s, c) = parse(DEFAULT_CONFIG, overrides)?;
            Ok((s, c, "（內建預設設定）".into()))
        }
    }
}
