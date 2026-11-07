//! Tests for `ParseCache`. Kept in their own file so `cache.rs` stays readable.

use std::cell::Cell;
use std::fs;

use super::ParseCache;

#[test]
fn unchanged_file_is_served_from_cache_and_changed_file_is_reparsed() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("t.jsonl");
    fs::write(&file, b"one\n").unwrap();
    let parses = Cell::new(0);
    let cache = ParseCache::load(None);
    let parse = || {
        parses.set(parses.get() + 1);
        Some(42u64)
    };

    assert_eq!(cache.get_or_parse(&file, parse), Some(42));
    assert_eq!(cache.get_or_parse(&file, parse), Some(42));
    assert_eq!(parses.get(), 1, "second call must be a hit");

    fs::write(&file, b"one two\n").unwrap();
    assert_eq!(cache.get_or_parse(&file, parse), Some(42));
    assert_eq!(parses.get(), 2, "a size change must miss");
}

#[test]
fn saved_entries_survive_a_reload_and_vanished_files_drop_out() {
    let tmp = tempfile::tempdir().unwrap();
    let kept = tmp.path().join("kept.json");
    let gone = tmp.path().join("gone.json");
    fs::write(&kept, b"{}").unwrap();
    fs::write(&gone, b"{}").unwrap();
    let store = tmp.path().join("parse-cache.json");

    let first = ParseCache::load(Some(store.clone()));
    first.get_or_parse(&kept, || Some("k".to_string()));
    first.get_or_parse(&gone, || Some("g".to_string()));
    first.save().unwrap();

    fs::remove_file(&gone).unwrap();
    let second = ParseCache::load(Some(store.clone()));
    let got: Option<String> = second.get(&kept);
    assert_eq!(got.as_deref(), Some("k"));
    second.save().unwrap();

    let third = ParseCache::load(Some(store));
    assert_eq!(third.get::<String>(&kept).as_deref(), Some("k"));
    assert!(third.get::<String>(&gone).is_none());
}

#[test]
fn failed_parse_is_not_cached() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("bad.jsonl");
    fs::write(&file, b"x").unwrap();
    let cache = ParseCache::load(None);
    assert_eq!(cache.get_or_parse::<u64>(&file, || None), None);
    assert_eq!(cache.get_or_parse(&file, || Some(7u64)), Some(7));
}

#[test]
fn memory_only_cache_never_writes() {
    let cache = ParseCache::load(None);
    assert!(cache.save().is_ok());
}
