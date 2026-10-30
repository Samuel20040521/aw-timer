//! Rule-based categorization of window events.

use std::collections::HashMap;

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

pub const UNCATEGORIZED: &str = "未分類";

/// How a category affects focus accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Focus,
    #[default]
    Neutral,
    Distraction,
    /// Time inside a forced rest started from the menu bar.
    Rest,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Focus, Kind::Neutral, Kind::Distraction, Kind::Rest];

    /// Index into `ALL`, for fixed-size per-kind arrays.
    pub fn index(self) -> usize {
        self as usize
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Focus => "專注",
            Kind::Neutral => "中性",
            Kind::Distraction => "分心",
            Kind::Rest => "休息",
        }
    }
}

/// One `[[category]]` table as written in the config file.
#[derive(Debug, Deserialize)]
pub struct CategoryRule {
    pub name: String,
    #[serde(default)]
    pub kind: Kind,
    #[serde(default)]
    pub apps: Vec<String>,
    #[serde(default)]
    pub pattern: Option<String>,
}

/// What a manual override matches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchOn {
    /// Exact app name (case-insensitive).
    App,
    /// Substring of the window title (case-insensitive).
    Title,
}

/// A user's manual classification from the interactive report; it takes
/// precedence over every config rule.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Override {
    #[serde(rename = "match")]
    pub match_on: MatchOn,
    pub value: String,
    pub category: String,
    pub kind: Kind,
}

/// One `[[project]]` table from the config file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRule {
    pub name: String,
    /// Folders (`~` allowed); a Claude Code session whose cwd is inside one
    /// belongs to this project.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Regex over `"app title"` (and over a session cwd as a fallback).
    #[serde(default)]
    pub pattern: Option<String>,
}

#[derive(Debug)]
struct Project {
    name: String,
    paths: Vec<String>,
    pattern: Option<Regex>,
}

#[derive(Debug)]
pub struct Category {
    pub name: String,
    pub kind: Kind,
    pattern: Option<Regex>,
}

/// Map `(app, title)` to a category.
///
/// Passes, strongest signal first:
/// 0. manual overrides, most recent first;
/// 1. exact app name (case-insensitive) against each rule's `apps`;
/// 2. regex search over `"app title"` in config-file order.
///
/// Unmatched events fall into a neutral "未分類" category.
pub struct Categorizer {
    categories: Vec<Category>,
    /// Indices of regex rules (overrides never carry a pattern).
    rule_count: usize,
    overrides: Vec<(MatchOn, String, usize)>,
    projects: Vec<Project>,
    by_app: HashMap<String, usize>,
    cache: HashMap<(String, String), usize>,
}

/// Bound the memo; window titles are high-cardinality but repetitive.
const CACHE_LIMIT: usize = 4096;

impl Categorizer {
    /// Build from config rules.
    ///
    /// # Errors
    /// Returns a message if a regex fails to compile.
    pub fn from_rules(
        rules: Vec<CategoryRule>,
        overrides: Vec<Override>,
        projects: Vec<ProjectRule>,
    ) -> Result<Self, String> {
        let home = std::env::var("HOME").unwrap_or_default();
        let projects = projects
            .into_iter()
            .map(|p| {
                let pattern = p
                    .pattern
                    .as_deref()
                    .filter(|x| !x.is_empty())
                    .map(|x| RegexBuilder::new(x).case_insensitive(true).build())
                    .transpose()
                    .map_err(|e| format!("project {:?}: {e}", p.name))?;
                let paths = p
                    .paths
                    .iter()
                    .map(|x| x.strip_prefix('~').map_or_else(|| x.clone(), |rest| format!("{home}{rest}")))
                    .map(|x| x.trim_end_matches('/').to_owned())
                    .collect();
                Ok(Project { name: p.name, paths, pattern })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut categories = Vec::with_capacity(rules.len() + overrides.len() + 1);
        let mut by_app = HashMap::new();
        for (idx, rule) in rules.into_iter().enumerate() {
            let pattern = match rule.pattern.as_deref().filter(|p| !p.is_empty()) {
                Some(p) => Some(
                    RegexBuilder::new(p)
                        .case_insensitive(true)
                        .build()
                        .map_err(|e| format!("category {:?}: {e}", rule.name))?,
                ),
                None => None,
            };
            for app in &rule.apps {
                // First rule listing an app wins, consistent with file order.
                by_app.entry(app.to_lowercase()).or_insert(idx);
            }
            categories.push(Category { name: rule.name, kind: rule.kind, pattern });
        }
        let rule_count = categories.len();
        let overrides = overrides
            .into_iter()
            .map(|o| {
                categories.push(Category { name: o.category, kind: o.kind, pattern: None });
                (o.match_on, o.value.to_lowercase(), categories.len() - 1)
            })
            .collect();
        // Fallback lives at the last index.
        categories.push(Category { name: UNCATEGORIZED.into(), kind: Kind::Neutral, pattern: None });
        Ok(Self { categories, rule_count, overrides, projects, by_app, cache: HashMap::new() })
    }

    /// Classify one event; results are memoized.
    pub fn classify(&mut self, app: &str, title: &str) -> &Category {
        let key = (app.to_owned(), title.to_owned());
        let idx = match self.cache.get(&key) {
            Some(&i) => i,
            None => {
                let i = self.classify_uncached(app, title);
                if self.cache.len() >= CACHE_LIMIT {
                    self.cache.clear();
                }
                self.cache.insert(key, i);
                i
            }
        };
        &self.categories[idx]
    }

    /// Configured project names, in file order.
    pub fn project_names(&self) -> Vec<String> {
        self.projects.iter().map(|p| p.name.clone()).collect()
    }

    /// Project whose pattern matches `"app title"`.
    pub fn project_for(&self, app: &str, title: &str) -> Option<&str> {
        let haystack = format!("{app} {title}");
        self.projects
            .iter()
            .find(|p| p.pattern.as_ref().is_some_and(|r| r.is_match(&haystack)))
            .map(|p| p.name.as_str())
    }

    /// Project for a working directory: deepest matching path wins, then
    /// the pattern applied to the path itself.
    pub fn project_for_path(&self, cwd: &str) -> Option<&str> {
        let inside = |root: &str| cwd == root || cwd.starts_with(&format!("{root}/"));
        self.projects
            .iter()
            .flat_map(|p| p.paths.iter().filter(|r| inside(r)).map(move |r| (r.len(), p)))
            .max_by_key(|(len, _)| *len)
            .map(|(_, p)| p.name.as_str())
            .or_else(|| {
                self.projects
                    .iter()
                    .find(|p| p.pattern.as_ref().is_some_and(|r| r.is_match(cwd)))
                    .map(|p| p.name.as_str())
            })
    }

    fn classify_uncached(&self, app: &str, title: &str) -> usize {
        let (app_lc, title_lc) = (app.to_lowercase(), title.to_lowercase());
        for (on, value, idx) in self.overrides.iter().rev() {
            let hit = match on {
                MatchOn::App => app_lc == *value,
                MatchOn::Title => !value.is_empty() && title_lc.contains(value.as_str()),
            };
            if hit {
                return *idx;
            }
        }
        if let Some(&i) = self.by_app.get(&app_lc) {
            return i;
        }
        let haystack = format!("{app} {title}");
        self.categories[..self.rule_count]
            .iter()
            .position(|c| c.pattern.as_ref().is_some_and(|p| p.is_match(&haystack)))
            .unwrap_or(self.categories.len() - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn default_categorizer() -> Categorizer {
        config::parse(config::DEFAULT_CONFIG, Vec::new()).unwrap().1
    }

    #[test]
    fn overrides_win_and_latest_override_wins() {
        let ov = |on, value: &str, cat: &str, kind| Override {
            match_on: on, value: value.into(), category: cat.into(), kind,
        };
        let mut c = config::parse(config::DEFAULT_CONFIG, vec![
            ov(MatchOn::App, "code", "娛樂", Kind::Distraction),
            ov(MatchOn::Title, "error response", "專注", Kind::Focus),
            ov(MatchOn::App, "Code", "專注", Kind::Focus),
        ]).unwrap().1;
        assert_eq!(c.classify("Code", "x.rs").name, "專注");
        let hit = c.classify("Vivaldi", "Error Response - Vivaldi");
        assert_eq!((hit.name.as_str(), hit.kind), ("專注", Kind::Focus));
    }

    #[test]
    fn app_name_wins_over_title_regex() {
        let mut c = default_categorizer();
        assert_eq!(c.classify("Code", "youtube_dl.py").name, "寫程式");
    }

    #[test]
    fn browser_titles() {
        let mut c = default_categorizer();
        for (title, expected) in [
            ("Verilog 寫法練習場 - Vivaldi", "寫程式"),
            ("Study group | Messenger - Vivaldi", "溝通"),
            ("lofi beats - YouTube - Vivaldi", "娛樂"),
            ("NTU COOL - Vivaldi", "課業研究"),
        ] {
            assert_eq!(c.classify("Vivaldi", title).name, expected, "{title}");
        }
    }

    #[test]
    fn localized_app_and_fallback() {
        let mut c = default_categorizer();
        assert_eq!(c.classify("系統設定", "x").kind, Kind::Neutral);
        assert_eq!(c.classify("Vivaldi", "Some page").name, UNCATEGORIZED);
    }
}
