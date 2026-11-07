//! User settings stored as JSON at `<data_dir>/config.json`, and the trash auto-delete policy.
//!
//! A missing file means defaults. The file is written atomically, like the trash manifests.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::paths;
use crate::trash::{write_atomic, PurgeReport, Trash};

/// Trash entries older than this are deleted automatically unless the user changes it.
pub const DEFAULT_TRASH_RETENTION_DAYS: u32 = 30;

/// Setting an env var to a non-empty value turns automatic deletion off. Tests use it.
pub const NO_AUTO_PURGE_ENV: &str = "SZ_NO_AUTO_PURGE";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    /// Days a trash entry is kept before it is deleted. 0 keeps entries forever.
    pub trash_retention_days: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            trash_retention_days: DEFAULT_TRASH_RETENTION_DAYS,
        }
    }
}

impl Config {
    /// `<data_dir>/config.json`.
    pub fn path() -> PathBuf {
        paths::data_dir().join("config.json")
    }

    /// Reads the config at the default path. A missing file yields the defaults.
    pub fn load() -> Result<Config> {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Result<Config> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Writes the config to the default path.
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        write_atomic(path, &bytes)
    }

    /// The age after which entries are deleted, or `None` when they are kept forever.
    pub fn retention(&self) -> Option<Duration> {
        (self.trash_retention_days > 0)
            .then(|| Duration::days(i64::from(self.trash_retention_days)))
    }

    /// Whole days until an entry created at `created` is auto-deleted, rounded up. `Some(0)` for
    /// an entry already past its retention. `None` when retention is off.
    pub fn days_until_purge(&self, created: DateTime<Utc>, now: DateTime<Utc>) -> Option<i64> {
        let retention = self.retention()?;
        let secs = (created + retention - now).num_seconds().max(0);
        Some((secs + 86_399) / 86_400)
    }

    /// Text for the auto-delete column: `auto-deletes in N days`, `auto-deletes today`, or
    /// `kept forever`.
    pub fn auto_delete_text(&self, created: DateTime<Utc>, now: DateTime<Utc>) -> String {
        match self.days_until_purge(created, now) {
            None => "kept forever".to_string(),
            Some(0) => "auto-deletes today".to_string(),
            Some(1) => "auto-deletes in 1 day".to_string(),
            Some(n) => format!("auto-deletes in {n} days"),
        }
    }

    /// Short form for narrow columns: `in N days`, `today`, or `kept forever`.
    pub fn short_auto_delete_text(&self, created: DateTime<Utc>, now: DateTime<Utc>) -> String {
        match self.days_until_purge(created, now) {
            None => "kept forever".to_string(),
            Some(0) => "today".to_string(),
            Some(1) => "in 1 day".to_string(),
            Some(n) => format!("in {n} days"),
        }
    }

    /// Human form of the setting: `30 days` or `never`.
    pub fn retention_text(&self) -> String {
        match self.trash_retention_days {
            0 => "never".to_string(),
            1 => "1 day".to_string(),
            n => format!("{n} days"),
        }
    }
}

/// Parses a retention value: a number of days with an optional `d` (`7`, `7d`), or `never` / `0`
/// for no automatic deletion.
pub fn parse_retention(input: &str) -> Result<u32> {
    let text = input.trim().to_ascii_lowercase();
    if text == "never" {
        return Ok(0);
    }
    let digits = text.strip_suffix('d').unwrap_or(&text);
    match digits.parse::<u32>() {
        Ok(n) => Ok(n),
        Err(_) => bail!("invalid retention '{input}': use a number of days (7 or 7d) or never"),
    }
}

/// Whether the environment turns automatic deletion off (`SZ_NO_AUTO_PURGE` set and non-empty).
pub fn auto_purge_disabled() -> bool {
    std::env::var_os(NO_AUTO_PURGE_ENV).is_some_and(|v| !v.is_empty())
}

/// Deletes trash entries older than the configured retention. Does nothing when retention is
/// off. Returns an empty report in that case.
pub fn auto_purge(trash: &Trash, config: &Config, now: DateTime<Utc>) -> Result<PurgeReport> {
    match config.retention() {
        None => Ok(PurgeReport::default()),
        Some(retention) => trash.purge_expired(retention, now),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, day, 12, 0, 0).unwrap()
    }

    #[test]
    fn default_is_thirty_days_and_missing_file_loads_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = Config::load_from(&tmp.path().join("config.json")).expect("load");
        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.trash_retention_days, 30);
        assert_eq!(cfg.retention(), Some(Duration::days(30)));
    }

    #[test]
    fn save_then_load_round_trips_and_leaves_no_temp_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nested").join("config.json");
        let cfg = Config {
            trash_retention_days: 7,
        };
        cfg.save_to(&path).expect("save");
        assert_eq!(Config::load_from(&path).expect("load"), cfg);
        let names: Vec<String> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["config.json".to_string()]);
    }

    #[test]
    fn corrupt_file_is_an_error_not_a_silent_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.json");
        fs::write(&path, "{ not json").unwrap();
        assert!(Config::load_from(&path).is_err());
    }

    #[test]
    fn parse_retention_accepts_days_and_never() {
        assert_eq!(parse_retention("7").unwrap(), 7);
        assert_eq!(parse_retention("7d").unwrap(), 7);
        assert_eq!(parse_retention(" 30D ").unwrap(), 30);
        assert_eq!(parse_retention("never").unwrap(), 0);
        assert_eq!(parse_retention("0").unwrap(), 0);
        assert!(parse_retention("soon").is_err());
        assert!(parse_retention("-3").is_err());
        assert!(parse_retention("7h").is_err());
    }

    #[test]
    fn days_until_purge_rounds_up_and_handles_never() {
        let cfg = Config {
            trash_retention_days: 30,
        };
        assert_eq!(cfg.days_until_purge(at(1), at(1)), Some(30));
        assert_eq!(cfg.days_until_purge(at(1), at(8)), Some(23));
        assert_eq!(
            cfg.days_until_purge(at(1), at(1) + Duration::hours(1)),
            Some(30)
        );
        assert_eq!(
            cfg.days_until_purge(at(1), at(31) + Duration::days(5)),
            Some(0)
        );
        assert_eq!(
            cfg.auto_delete_text(at(1), at(8)),
            "auto-deletes in 23 days"
        );
        assert_eq!(cfg.short_auto_delete_text(at(1), at(8)), "in 23 days");
        let never = Config {
            trash_retention_days: 0,
        };
        assert_eq!(never.days_until_purge(at(1), at(8)), None);
        assert_eq!(never.auto_delete_text(at(1), at(8)), "kept forever");
        assert_eq!(never.retention_text(), "never");
    }
}
