//! Locations of SessionZero's own data: the trash store and the scan cache.

use std::path::PathBuf;

/// `$SZ_DATA_DIR` when set, otherwise `~/.sessionzero`. Created on demand.
pub fn data_dir() -> PathBuf {
    let dir = match std::env::var_os("SZ_DATA_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".sessionzero"),
    };
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Root of the trash store, `<data_dir>/trash`.
pub fn trash_dir() -> PathBuf {
    data_dir().join("trash")
}

/// Scan cache file, `<data_dir>/scan-cache.json`.
pub fn cache_file() -> PathBuf {
    data_dir().join("scan-cache.json")
}

/// Serializes tests that set `SZ_DATA_DIR`, since the environment is process-global.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sz_data_dir_overrides_location() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("sz");
        std::env::set_var("SZ_DATA_DIR", &target);
        assert_eq!(data_dir(), target);
        assert!(target.is_dir(), "data dir is created on demand");
        assert_eq!(trash_dir(), target.join("trash"));
        assert_eq!(cache_file(), target.join("scan-cache.json"));
        std::env::remove_var("SZ_DATA_DIR");
    }
}
