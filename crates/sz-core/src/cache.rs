//! Scan result cache.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;

use crate::model::Session;
use crate::paths;
use crate::trash::write_atomic;

/// One cached parse result, valid while the file keeps its size and modification time.
#[derive(Serialize, Deserialize, Clone)]
struct FileEntry {
    size: u64,
    mtime_ns: u128,
    value: Value,
}

/// Per-file parse results, so an unchanged transcript is not parsed again on the next scan.
///
/// Entries are keyed by path and checked against the file's size and modification time. A file
/// whose stamp changed is parsed again. Only entries seen during this run are saved, so entries
/// for deleted files drop out. The cache is persisted only for the real home directory. For any
/// other home, it lives in memory for one run.
pub struct ParseCache {
    file: Option<PathBuf>,
    loaded: HashMap<String, FileEntry>,
    fresh: Mutex<HashMap<String, FileEntry>>,
}

/// `(size, mtime in ns since the epoch)` of a file, or `None` when it cannot be stat'ed.
fn stamp(path: &Path) -> Option<(u64, u128)> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((meta.len(), mtime))
}

impl ParseCache {
    /// Cache backed by `file`, or memory only when `file` is `None`. A missing or unreadable
    /// file starts empty.
    pub fn load(file: Option<PathBuf>) -> Self {
        let loaded = file
            .as_ref()
            .and_then(|f| fs::read(f).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            file,
            loaded,
            fresh: Mutex::new(HashMap::new()),
        }
    }

    /// Cache for one agent's adapter. Persisted as `<data_dir>/parse-cache/<agent>.json` when
    /// `home` is the real home directory. Other homes, such as test fixtures, stay in memory.
    pub fn for_home(home: &Path, agent: &str) -> Self {
        let real = dirs::home_dir().as_deref() == Some(home);
        let file = real.then(|| paths::data_dir().join("parse-cache").join(format!("{agent}.json")));
        Self::load(file)
    }

    /// Parse result for `path` when its stamp still matches. Hits are kept for the next save.
    pub fn get<T: DeserializeOwned>(&self, path: &Path) -> Option<T> {
        let key = path.to_string_lossy().into_owned();
        let (size, mtime_ns) = stamp(path)?;
        // An entry parsed earlier in this run wins over the one loaded from disk.
        let entry = self
            .fresh
            .lock()
            .ok()
            .and_then(|fresh| fresh.get(&key).cloned())
            .or_else(|| self.loaded.get(&key).cloned())?;
        if entry.size != size || entry.mtime_ns != mtime_ns {
            return None;
        }
        let value = serde_json::from_value(entry.value.clone()).ok()?;
        if let Ok(mut fresh) = self.fresh.lock() {
            fresh.insert(key, entry);
        }
        Some(value)
    }

    /// Returns the cached value for `path`, or runs `parse` and caches what it returns. `None`
    /// results are not cached, so a file that failed to parse is tried again next time.
    ///
    /// The stamp is taken before `parse` runs. A file that changes during the parse therefore
    /// gets a stale stamp and is parsed again on the next run.
    pub fn get_or_parse<T: Serialize + DeserializeOwned>(
        &self,
        path: &Path,
        parse: impl FnOnce() -> Option<T>,
    ) -> Option<T> {
        if let Some(hit) = self.get(path) {
            return Some(hit);
        }
        let stamped = stamp(path);
        let value = parse()?;
        if let (Some((size, mtime_ns)), Ok(json)) = (stamped, serde_json::to_value(&value)) {
            if let Ok(mut fresh) = self.fresh.lock() {
                let key = path.to_string_lossy().into_owned();
                fresh.insert(
                    key,
                    FileEntry {
                        size,
                        mtime_ns,
                        value: json,
                    },
                );
            }
        }
        Some(value)
    }

    /// Writes this run's entries atomically, replacing the previous file. A cache with no backing
    /// file does nothing.
    pub fn save(&self) -> Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let fresh = self
            .fresh
            .lock()
            .map_err(|_| anyhow::anyhow!("parse cache lock poisoned"))?;
        write_atomic(file, &serde_json::to_vec(&*fresh)?)
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct KeyedEntry<V> {
    fp: String,
    value: V,
}

/// Values keyed by a caller-chosen string (for example a session id) and checked against a
/// caller-chosen fingerprint string. A stored value is served only while its fingerprint still
/// matches the one the caller computes now.
///
/// Like `ParseCache`, a save keeps only the entries written or hit during this run, so entries for
/// deleted keys drop out. The cache is persisted only for the real home directory (see
/// `KeyedCache::for_home`). Any other home keeps it in memory.
pub struct KeyedCache<V> {
    file: Option<PathBuf>,
    loaded: HashMap<String, KeyedEntry<V>>,
    fresh: Mutex<HashMap<String, KeyedEntry<V>>>,
}

impl<V: Serialize + DeserializeOwned + Clone> KeyedCache<V> {
    /// Cache backed by `file`, or memory only when `file` is `None`. A missing or unreadable file
    /// starts empty.
    pub fn load(file: Option<PathBuf>) -> Self {
        let loaded = file
            .as_ref()
            .and_then(|f| fs::read(f).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            file,
            loaded,
            fresh: Mutex::new(HashMap::new()),
        }
    }

    /// Cache named `name`, persisted as `<data_dir>/parse-cache/<name>.json` only when `home` is
    /// the real home directory.
    pub fn for_home(home: &Path, name: &str) -> Self {
        let real = dirs::home_dir().as_deref() == Some(home);
        let file =
            real.then(|| paths::data_dir().join("parse-cache").join(format!("{name}.json")));
        Self::load(file)
    }

    /// The value stored for `key` when its fingerprint equals `fp`. A hit is kept for the next save.
    pub fn get(&self, key: &str, fp: &str) -> Option<V> {
        let entry = self
            .fresh
            .lock()
            .ok()
            .and_then(|fresh| fresh.get(key).cloned())
            .or_else(|| self.loaded.get(key).cloned())?;
        if entry.fp != fp {
            return None;
        }
        let value = entry.value.clone();
        if let Ok(mut fresh) = self.fresh.lock() {
            fresh.insert(key.to_owned(), entry);
        }
        Some(value)
    }

    /// Stores `value` for `key` under fingerprint `fp`, replacing any earlier entry from this run.
    pub fn put(&self, key: &str, fp: &str, value: V) {
        if let Ok(mut fresh) = self.fresh.lock() {
            fresh.insert(
                key.to_owned(),
                KeyedEntry {
                    fp: fp.to_owned(),
                    value,
                },
            );
        }
    }

    /// Writes this run's entries atomically. A cache with no backing file does nothing.
    pub fn save(&self) -> Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let fresh = self
            .fresh
            .lock()
            .map_err(|_| anyhow::anyhow!("keyed cache lock poisoned"))?;
        write_atomic(file, &serde_json::to_vec(&*fresh)?)
    }
}

#[derive(Serialize)]
struct CacheOut<'a> {
    saved_at: DateTime<Utc>,
    sessions: &'a [Session],
}

#[derive(Deserialize)]
struct CacheIn {
    saved_at: DateTime<Utc>,
    sessions: Vec<Session>,
}

fn read_cache() -> Option<CacheIn> {
    let text = fs::read_to_string(paths::cache_file()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Cached sessions regardless of age. `None` when there is no readable cache.
pub fn load() -> Option<Vec<Session>> {
    read_cache().map(|c| c.sessions)
}

/// Cached sessions when they were saved within `max_age`.
pub fn load_if_fresh(max_age: Duration) -> Option<Vec<Session>> {
    let cache = read_cache()?;
    if Utc::now() - cache.saved_at <= max_age {
        Some(cache.sessions)
    } else {
        None
    }
}

/// Writes the cache as `{saved_at, sessions}`, atomically.
pub fn save(sessions: &[Session]) -> Result<()> {
    let out = CacheOut {
        saved_at: Utc::now(),
        sessions,
    };
    write_atomic(&paths::cache_file(), &serde_json::to_vec(&out)?)
}

#[cfg(test)]
#[path = "cache_parse_tests.rs"]
mod parse_cache_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentKind, Artifact};
    use std::path::PathBuf;

    use crate::paths::ENV_LOCK;

    fn sample() -> Vec<Session> {
        vec![Session {
            parent_id: None,
            agent: AgentKind::Grok,
            id: "s1".into(),
            title: Some("hello".into()),
            project: Some(PathBuf::from("/work/p")),
            created: None,
            last_active: Some(Utc::now()),
            message_count: Some(4),
            bytes: 12,
            artifacts: vec![Artifact::File(PathBuf::from("/work/p/s1.json"))],
        }]
    }

    #[test]
    fn save_then_load_round_trips_and_respects_age() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        std::env::set_var("SZ_DATA_DIR", tmp.path());

        assert!(load().is_none(), "no cache yet");
        save(&sample()).expect("save");

        let loaded = load().expect("cache present");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].key(), "grok:s1");
        assert_eq!(loaded[0].bytes, 12);
        assert!(load_if_fresh(Duration::hours(1)).is_some());
        assert!(load_if_fresh(Duration::seconds(-1)).is_none());

        std::env::remove_var("SZ_DATA_DIR");
    }

    #[test]
    fn corrupt_cache_reads_as_none() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        std::env::set_var("SZ_DATA_DIR", tmp.path());
        std::fs::write(paths::cache_file(), b"{not json").unwrap();
        assert!(load().is_none());
        assert!(load_if_fresh(Duration::hours(1)).is_none());
        std::env::remove_var("SZ_DATA_DIR");
    }
}
