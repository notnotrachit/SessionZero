//! Codex CLI adapter. Format notes: docs/formats/codex.md.
//!
//! Rollouts are `rollout-<TS>-<UUID>.jsonl` (or `.jsonl.zst`) under `sessions/**` and
//! `archived_sessions/`. Compressed rollouts are stream-decoded like plain ones. When one session
//! id has both a plain and a compressed file, both are listed as artifacts of one session.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDateTime, Utc};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use walkdir::WalkDir;

use crate::adapter::Adapter;
use crate::cache::ParseCache;
use crate::model::{AgentKind, Artifact, PreviewMsg, Session};
use crate::util::{mtime, truncate};

const TITLE_MAX: usize = 120;
const PREVIEW_TEXT_MAX: usize = 2000;

pub struct CodexAdapter {
    home: PathBuf,
}

impl CodexAdapter {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }
}

impl Adapter for CodexAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![
            self.home.join(".codex/sessions"),
            self.home.join(".codex/archived_sessions"),
        ]
    }

    fn scan(&self) -> anyhow::Result<Vec<Session>> {
        let codex = self.home.join(".codex");
        let shared = load_shared(&codex);
        let groups = group_rollouts(rollout_files(&codex));
        let cache = ParseCache::for_home(&self.home, "codex");
        let mut sessions: Vec<Session> = groups
            .par_iter()
            .filter_map(|(id, paths)| build_session(id, paths, &shared, &cache))
            .collect();
        if let Err(err) = cache.save() {
            eprintln!("sz: codex parse cache not saved: {err:#}");
        }
        sessions.sort_by(|a, b| {
            b.last_active
                .cmp(&a.last_active)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(sessions)
    }

    fn preview(&self, s: &Session, max: usize) -> anyhow::Result<Vec<PreviewMsg>> {
        let mut out = Vec::new();
        if max == 0 {
            return Ok(out);
        }
        // Prefer the plain rollout when both forms exist. Both hold the same transcript.
        let paths: Vec<&PathBuf> = s
            .artifacts
            .iter()
            .filter_map(|a| match a {
                Artifact::File(p) if is_rollout_name(p) => Some(p),
                _ => None,
            })
            .collect();
        let Some(path) = paths
            .iter()
            .find(|p| !is_zst(p))
            .or_else(|| paths.first())
        else {
            return Ok(out);
        };
        let Ok(reader) = open_text(path) else {
            return Ok(out);
        };
        let _ = each_line(reader, |line| {
            if !is_message_line(line) {
                return true;
            }
            let Some(payload) = parse_rec(line).and_then(|r| r.payload) else {
                return true;
            };
            let Some(item) = payload.get("item") else {
                return true;
            };
            let role = match item.get("type").and_then(Value::as_str) {
                Some("UserMessage") => "user",
                Some("AgentMessage") => "assistant",
                _ => return true,
            };
            let text = item_text(item);
            let text = text.trim();
            if text.is_empty() {
                return true;
            }
            out.push(PreviewMsg {
                role: role.to_string(),
                text: truncate(text, PREVIEW_TEXT_MAX),
                ts: extract_timestamp(line),
            });
            out.len() < max
        });
        Ok(out)
    }

    fn process_names(&self) -> &'static [&'static str] {
        &["codex"]
    }
}

/// Lookup data read once per scan and shared by every rollout.
struct Shared {
    index_file: PathBuf,
    /// Session id -> thread_name from `session_index.jsonl`. Value is None when the row has no name.
    index: HashMap<String, Option<String>>,
    history_file: PathBuf,
    /// Session ids that appear in `history.jsonl`.
    history_ids: HashSet<String>,
    /// Session id -> `shell_snapshots/<id>.<epoch>.sh` files.
    shells: HashMap<String, Vec<PathBuf>>,
}

#[derive(Deserialize)]
struct IndexRow {
    id: Option<String>,
    thread_name: Option<String>,
}

#[derive(Deserialize)]
struct HistoryRow {
    session_id: Option<String>,
}

#[derive(Deserialize)]
struct Rec {
    payload: Option<Value>,
}

/// Fields read from one rollout file.
#[derive(Default, Serialize, Deserialize)]
struct Meta {
    project: Option<String>,
    created: Option<DateTime<Utc>>,
    last_active: Option<DateTime<Utc>>,
    messages: u64,
    title: Option<String>,
}

fn load_shared(codex: &Path) -> Shared {
    let index_file = codex.join("session_index.jsonl");
    let mut index: HashMap<String, Option<String>> = HashMap::new();
    for_each_line(&index_file, |line| {
        if let Ok(row) = serde_json::from_str::<IndexRow>(line) {
            if let Some(id) = row.id {
                let slot = index.entry(id).or_insert(None);
                if row.thread_name.is_some() {
                    *slot = row.thread_name;
                }
            }
        }
        true
    });

    let history_file = codex.join("history.jsonl");
    let mut history_ids = HashSet::new();
    for_each_line(&history_file, |line| {
        if line.contains("\"session_id\"") {
            if let Ok(HistoryRow {
                session_id: Some(id),
            }) = serde_json::from_str::<HistoryRow>(line)
            {
                history_ids.insert(id);
            }
        }
        true
    });

    let mut shells: HashMap<String, Vec<PathBuf>> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(codex.join("shell_snapshots")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".sh") {
                continue;
            }
            if let Some((id, _)) = name.split_once('.') {
                shells
                    .entry(id.to_string())
                    .or_default()
                    .push(entry.path());
            }
        }
    }

    Shared {
        index_file,
        index,
        history_file,
        history_ids,
        shells,
    }
}

/// Every rollout file under `sessions/` and `archived_sessions/`, plain or zstd. Sorted so the
/// grouping and the merged fields are deterministic.
fn rollout_files(codex: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for root in [codex.join("sessions"), codex.join("archived_sessions")] {
        for entry in WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("rollout-")
                && (name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"))
            {
                files.push(entry.into_path());
            }
        }
    }
    files
}

/// Groups rollout files by session id. Within a group the plain `.jsonl` comes before `.zst`.
fn group_rollouts(files: Vec<PathBuf>) -> Vec<(String, Vec<PathBuf>)> {
    let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for path in files {
        let Some(id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(rollout_id)
        else {
            continue;
        };
        groups.entry(id).or_default().push(path);
    }
    let mut out: Vec<(String, Vec<PathBuf>)> = groups
        .into_iter()
        .map(|(id, mut paths)| {
            paths.sort_by_key(|p| is_zst(p));
            (id, paths)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn is_zst(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "zst")
}

fn is_rollout_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("rollout-") && rollout_id(n).is_some())
}

/// Builds one session from every rollout file that shares `id`.
fn build_session(
    id: &str,
    paths: &[PathBuf],
    shared: &Shared,
    cache: &ParseCache,
) -> Option<Session> {
    let id = id.to_string();
    let mut artifacts: Vec<Artifact> = paths.iter().cloned().map(Artifact::File).collect();
    if let Some(snaps) = shared.shells.get(&id) {
        artifacts.extend(snaps.iter().cloned().map(Artifact::File));
    }
    if shared.index.contains_key(&id) {
        artifacts.push(Artifact::JsonlLines {
            file: shared.index_file.clone(),
            field: "id".into(),
            value: id.clone(),
        });
    }
    if shared.history_ids.contains(&id) {
        artifacts.push(Artifact::JsonlLines {
            file: shared.history_file.clone(),
            field: "session_id".into(),
            value: id.clone(),
        });
    }

    let index_title = shared.index.get(&id).cloned().flatten();
    let fallback_ts = paths.iter().filter_map(|p| mtime(p)).max();
    let start_ts = paths
        .iter()
        .filter_map(|p| p.file_name()?.to_str().and_then(name_timestamp))
        .min();

    // Each file is read on its own. A file that cannot be read contributes nothing, and
    // message_count stays unknown when no file could be read.
    let mut meta: Option<Meta> = None;
    let mut message_count: Option<u64> = None;
    for path in paths {
        let Some(m) = cache.get_or_parse(path, || scan_rollout(path)) else {
            continue;
        };
        message_count = Some(message_count.map_or(m.messages, |c| c.max(m.messages)));
        meta = Some(match meta {
            None => m,
            Some(acc) => merge_meta(acc, m),
        });
    }
    let meta = meta.unwrap_or_default();

    let mut session = Session {
        parent_id: None,
        agent: AgentKind::Codex,
        id,
        title: index_title
            .and_then(|t| normalize_title(&t))
            .or(meta.title),
        project: meta.project.map(PathBuf::from),
        created: meta.created.or(start_ts).or(fallback_ts),
        last_active: meta.last_active.or(fallback_ts),
        message_count,
        bytes: 0,
        artifacts,
    };
    session.recompute_bytes();
    Some(session)
}

/// Combines the metadata of two copies of one session. The earliest creation, latest activity,
/// and first known project and title win.
fn merge_meta(a: Meta, b: Meta) -> Meta {
    Meta {
        project: a.project.or(b.project),
        created: min_opt(a.created, b.created),
        last_active: max_opt(a.last_active, b.last_active),
        messages: a.messages.max(b.messages),
        title: a.title.or(b.title),
    }
}

fn min_opt(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    }
}

fn max_opt(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, y) => x.or(y),
    }
}

/// Reads one rollout, plain or zstd, by streaming it. Returns None when the file cannot be
/// opened or decoding fails before the end.
fn scan_rollout(path: &Path) -> Option<Meta> {
    let mut meta = Meta::default();
    let mut first_ts: Option<DateTime<Utc>> = None;
    let mut meta_ts: Option<DateTime<Utc>> = None;
    let mut turn_checked = false;

    let reader = open_text(path).ok()?;
    each_line(reader, |line| {
        if let Some(ts) = extract_timestamp(line) {
            first_ts.get_or_insert(ts);
            meta.last_active = Some(ts);
        }

        if line.contains("\"session_meta\"") {
            if let Some(payload) = parse_rec(line).and_then(|r| r.payload) {
                meta.project = str_field(&payload, "cwd");
                meta_ts = str_field(&payload, "timestamp").and_then(|s| parse_ts(&s));
            }
        } else if !turn_checked && line.contains("\"turn_context\"") {
            turn_checked = true;
            if meta.project.is_none() {
                if let Some(payload) = parse_rec(line).and_then(|r| r.payload) {
                    meta.project = str_field(&payload, "cwd");
                }
            }
        }

        if line.contains("item_completed") && is_message_line(line) {
            if let Some(payload) = parse_rec(line).and_then(|r| r.payload) {
                if let Some(item) = payload.get("item") {
                    match item.get("type").and_then(Value::as_str) {
                        Some("UserMessage") => {
                            meta.messages += 1;
                            if meta.title.is_none() {
                                meta.title = normalize_title(&item_text(item));
                            }
                        }
                        Some("AgentMessage") => meta.messages += 1,
                        _ => {}
                    }
                }
            }
        }
        true
    })
    .ok()?;
    meta.created = meta_ts.or(first_ts);
    Some(meta)
}

/// Opens a text file for line reading. A `.zst` path is stream-decoded.
fn open_text(path: &Path) -> io::Result<Box<dyn BufRead>> {
    let file = BufReader::with_capacity(256 * 1024, File::open(path)?);
    if is_zst(path) {
        Ok(Box::new(BufReader::with_capacity(
            256 * 1024,
            zstd::stream::read::Decoder::with_buffer(file)?,
        )))
    } else {
        Ok(Box::new(file))
    }
}

/// Calls `f` for each non-empty line of `path`, with surrounding whitespace trimmed. Returns false
/// when the file cannot be opened. Used for the small shared index and history files.
fn for_each_line(path: &Path, f: impl FnMut(&str) -> bool) -> bool {
    match open_text(path) {
        Ok(reader) => each_line(reader, f).is_ok(),
        Err(_) => false,
    }
}

/// Calls `f` for each non-empty trimmed line. Invalid UTF-8 is replaced. `f` returns false to stop
/// early. A read or decode error is returned, so callers can tell a truncated file from a short one.
fn each_line(mut reader: Box<dyn BufRead>, mut f: impl FnMut(&str) -> bool) -> io::Result<()> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        let trimmed = line.trim();
        if !trimmed.is_empty() && !f(trimmed) {
            break;
        }
    }
    Ok(())
}

/// Cheap prefilter: a user or agent message item. Callers must check `item_completed` first.
fn is_message_line(line: &str) -> bool {
    line.contains("\"UserMessage\"") || line.contains("\"AgentMessage\"")
}

fn parse_rec(line: &str) -> Option<Rec> {
    serde_json::from_str(line).ok()
}

/// Top-level `timestamp` of a JSON line, read without a full parse.
fn extract_timestamp(line: &str) -> Option<DateTime<Utc>> {
    const KEY: &str = "\"timestamp\":\"";
    let start = line.find(KEY)? + KEY.len();
    let rest = &line[start..];
    parse_ts(&rest[..rest.find('"')?])
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Session start time from `rollout-YYYY-MM-DDTHH-MM-SS-...`, which is UTC.
fn name_timestamp(file_name: &str) -> Option<DateTime<Utc>> {
    let stamp = file_name.strip_prefix("rollout-")?.get(..19)?;
    let naive = NaiveDateTime::parse_from_str(stamp, "%Y-%m-%dT%H-%M-%S").ok()?;
    Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

/// Session id from `rollout-<TS>-<UUID>[_<uuid>].jsonl[.zst]`. The first UUID-shaped run after
/// the timestamp is the session id.
fn rollout_id(file_name: &str) -> Option<String> {
    let stem = file_name
        .strip_suffix(".jsonl.zst")
        .or_else(|| file_name.strip_suffix(".jsonl"))?;
    let rest = stem.strip_prefix("rollout-")?;
    let body = rest
        .get(20..)
        .filter(|_| rest.as_bytes().get(19) == Some(&b'-'))
        .unwrap_or(rest);
    find_uuid(body).map(str::to_string)
}

fn find_uuid(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    if b.len() < 36 {
        return None;
    }
    (0..=b.len() - 36)
        .find(|&i| is_uuid(&b[i..i + 36]))
        .map(|i| &s[i..i + 36])
}

fn is_uuid(b: &[u8]) -> bool {
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Joins the `text` fields of an item's `content` blocks.
fn item_text(item: &Value) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Collapses whitespace and truncates a title. Returns None when nothing is left.
fn normalize_title(raw: &str) -> Option<String> {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then(|| truncate(&collapsed, TITLE_MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn fixture_home() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/codex")
    }

    fn rel(home: &Path, p: &Path) -> String {
        p.strip_prefix(home)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned()
    }

    fn iso(s: &str) -> DateTime<Utc> {
        parse_ts(s).expect("valid rfc3339 in test data")
    }

    #[test]
    fn fixture_matches_expected_json() {
        let home = fixture_home();
        let text = std::fs::read_to_string(home.join("EXPECTED.json")).expect("read expected");
        let expected: Value = serde_json::from_str(&text).expect("parse expected");
        let expected = expected["sessions"].as_array().expect("sessions array");

        let mut sessions = CodexAdapter::new(&home).scan().expect("scan");
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(sessions.len(), expected.len());

        for (s, e) in sessions.iter().zip(expected) {
            let id = s.id.as_str();
            assert_eq!(id, e["id"].as_str().expect("id"));
            assert_eq!(s.title.as_deref(), e["title"].as_str(), "title of {id}");
            assert_eq!(
                s.project
                    .as_deref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .as_deref(),
                e["project"].as_str(),
                "project of {id}"
            );
            assert_eq!(
                s.message_count,
                e["message_count"].as_u64(),
                "message_count of {id}"
            );
            assert_eq!(
                s.created,
                Some(iso(e["created"].as_str().expect("created"))),
                "created of {id}"
            );
            assert_eq!(
                s.last_active,
                Some(iso(e["last_active"].as_str().expect("last_active"))),
                "last_active of {id}"
            );

            let mut files: Vec<String> = s
                .artifacts
                .iter()
                .filter_map(|a| match a {
                    Artifact::File(p) => Some(rel(&home, p)),
                    _ => None,
                })
                .collect();
            files.sort();
            let mut want: Vec<String> = e["artifacts"]
                .as_array()
                .expect("artifacts")
                .iter()
                .map(|v| v.as_str().expect("path").to_string())
                .collect();
            want.sort();
            assert_eq!(files, want, "file artifacts of {id}");

            for (name, count) in e["index_refs"].as_object().expect("index_refs") {
                let present = s.artifacts.iter().any(|a| match a {
                    Artifact::JsonlLines { file, .. } => {
                        file.file_name().and_then(|n| n.to_str()) == Some(name.as_str())
                    }
                    _ => false,
                });
                assert_eq!(
                    present,
                    count.as_u64().expect("count") > 0,
                    "index ref {name} of {id}"
                );
            }
            assert_eq!(s.bytes, s.artifacts.iter().map(Artifact::own_bytes).sum::<u64>());
        }
    }

    #[test]
    fn preview_returns_messages_in_order_and_respects_max() {
        let home = fixture_home();
        let adapter = CodexAdapter::new(&home);
        let sessions = adapter.scan().expect("scan");
        let s = sessions
            .iter()
            .find(|s| s.id == "019c0001-aaaa-7bbb-8ccc-000000000001")
            .expect("fixture session");

        let msgs = adapter.preview(s, 10).expect("preview");
        let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
        assert_eq!(msgs[0].text, "Add exponential backoff to the uploader retry loop.");

        assert_eq!(adapter.preview(s, 2).expect("preview").len(), 2);
    }

    #[test]
    fn rollout_id_handles_plain_zst_and_suffixed_names() {
        let plain = "rollout-2026-11-02T10-48-18-019c0001-aaaa-7bbb-8ccc-000000000001.jsonl";
        assert_eq!(
            rollout_id(plain).as_deref(),
            Some("019c0001-aaaa-7bbb-8ccc-000000000001")
        );
        let zst = "rollout-2026-10-29T15-51-50-019c0009-dddd-7bbb-8ccc-000000000009.jsonl.zst";
        assert_eq!(
            rollout_id(zst).as_deref(),
            Some("019c0009-dddd-7bbb-8ccc-000000000009")
        );
        let suffixed =
            "rollout-2026-10-29T15-51-50-01a13426-1111-7222-8333-444455556666_01a1871d-b215-7820-914d-0a99299c0f4a.jsonl";
        assert_eq!(
            rollout_id(suffixed).as_deref(),
            Some("01a13426-1111-7222-8333-444455556666")
        );
        assert_eq!(rollout_id("notes.jsonl"), None);
    }

    const ZST_ID: &str = "019c0009-dddd-7bbb-8ccc-000000000009";

    fn rollout_body(id: &str, title: &str, extra_user: bool) -> String {
        let meta = format!(
            r#"{{"timestamp":"2026-10-29T15:51:50.000Z","type":"session_meta","payload":{{"id":"{id}","timestamp":"2026-10-29T15:51:50.000Z","cwd":"/tmp/zst"}}}}"#
        );
        let user = format!(
            r#"{{"timestamp":"2026-10-29T15:52:00.000Z","type":"event_msg","payload":{{"type":"item_completed","item":{{"type":"UserMessage","content":[{{"type":"Text","text":"{title}"}}]}}}}}}"#
        );
        let agent = r#"{"timestamp":"2026-10-29T15:53:00.000Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","content":[{"type":"Text","text":"done"}]}}}"#;
        let mut lines = vec![meta, user, agent.to_string()];
        if extra_user {
            lines.push(
                r#"{"timestamp":"2026-10-29T15:54:00.000Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"Text","text":"more"}]}}}"#
                    .to_string(),
            );
        }
        lines.join("\n")
    }

    #[test]
    fn zst_rollout_is_decoded_for_title_project_and_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = dir.path().join(".codex/sessions/2026/10/29");
        std::fs::create_dir_all(&day).expect("mkdir");
        let name = format!("rollout-2026-10-29T15-51-50-{ZST_ID}.jsonl.zst");
        let body = rollout_body(ZST_ID, "Compressed title", false);
        let packed = zstd::stream::encode_all(body.as_bytes(), 0).expect("encode");
        std::fs::write(day.join(&name), &packed).expect("write");

        let sessions = CodexAdapter::new(dir.path()).scan().expect("scan");
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, ZST_ID);
        assert_eq!(s.title.as_deref(), Some("Compressed title"));
        assert_eq!(s.project, Some(PathBuf::from("/tmp/zst")));
        assert_eq!(s.message_count, Some(2));
        assert_eq!(s.created, Some(iso("2026-10-29T15:51:50Z")));
        assert_eq!(s.last_active, Some(iso("2026-10-29T15:53:00Z")));
        assert_eq!(s.bytes, packed.len() as u64);

        let msgs = CodexAdapter::new(dir.path())
            .preview(s, 10)
            .expect("preview");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].text, "Compressed title");
    }

    #[test]
    fn plain_and_zst_copies_merge_into_one_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = dir.path().join(".codex/sessions/2026/10/29");
        std::fs::create_dir_all(&day).expect("mkdir");
        let plain_name = format!("rollout-2026-10-29T15-51-50-{ZST_ID}.jsonl");
        let zst_name = format!("{plain_name}.zst");
        std::fs::write(day.join(&plain_name), rollout_body(ZST_ID, "Plain", false)).expect("write");
        let packed = zstd::stream::encode_all(rollout_body(ZST_ID, "Plain", true).as_bytes(), 0)
            .expect("encode");
        std::fs::write(day.join(&zst_name), &packed).expect("write");

        let sessions = CodexAdapter::new(dir.path()).scan().expect("scan");
        assert_eq!(sessions.len(), 1, "one id must give one session");
        let s = &sessions[0];
        assert_eq!(s.title.as_deref(), Some("Plain"));
        // The longer compressed copy has one more message, and that count wins.
        assert_eq!(s.message_count, Some(3));
        let files: Vec<String> = s
            .artifacts
            .iter()
            .filter_map(|a| match a {
                Artifact::File(p) => p.file_name().map(|n| n.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        assert_eq!(files, vec![plain_name, zst_name]);
    }

    #[test]
    fn corrupt_zst_rollout_keeps_name_facts_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = dir.path().join(".codex/sessions/2026/10/29");
        std::fs::create_dir_all(&day).expect("mkdir");
        let name = format!("rollout-2026-10-29T15-51-50-{ZST_ID}.jsonl.zst");
        std::fs::write(day.join(name), b"not really zstd").expect("write");

        let sessions = CodexAdapter::new(dir.path()).scan().expect("scan");
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, ZST_ID);
        assert!(s.title.is_none());
        assert!(s.message_count.is_none());
        assert_eq!(s.created, Some(iso("2026-10-29T15:51:50Z")));
        assert_eq!(s.bytes, 15);
    }

    #[test]
    fn malformed_and_oversized_lines_do_not_stop_the_scan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = dir.path().join(".codex/sessions/2026/10/29");
        std::fs::create_dir_all(&day).expect("mkdir");
        let id = "019c0010-eeee-7bbb-8ccc-000000000010";
        let meta = r#"{"timestamp":"2026-10-29T15:00:00.000Z","type":"session_meta","payload":{"id":"019c0010-eeee-7bbb-8ccc-000000000010","timestamp":"2026-10-29T15:00:00.000Z","cwd":"/tmp/x"}}"#;
        let huge = format!(
            r#"{{"timestamp":"2026-10-29T15:00:01.000Z","type":"event_msg","payload":{{"type":"item_completed","item":{{"type":"CommandExecution","output":"{}"}}}}}}"#,
            "x".repeat(2_000_000)
        );
        let user = r#"{"timestamp":"2026-10-29T15:00:02.000Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"Text","text":"  Fix   the\tbuild  "}]}}}"#;
        let last = r#"{"timestamp":"2026-10-29T15:05:00.000Z","type":"event_msg","payload":{"type":"task_complete"}}"#;
        let body = [meta, "{not json", &huge, user, last].join("\n");
        std::fs::write(
            day.join(format!("rollout-2026-10-29T15-00-00-{id}.jsonl")),
            body,
        )
        .expect("write");

        let sessions = CodexAdapter::new(dir.path()).scan().expect("scan");
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.title.as_deref(), Some("Fix the build"));
        assert_eq!(s.message_count, Some(1));
        assert_eq!(s.project, Some(PathBuf::from("/tmp/x")));
        assert_eq!(s.last_active, Some(iso("2026-10-29T15:05:00Z")));
    }

    #[test]
    #[ignore]
    fn live_scan() {
        let home = dirs::home_dir().expect("home dir");
        let started = Instant::now();
        let sessions = CodexAdapter::new(&home).scan().expect("scan");
        let elapsed = started.elapsed();
        let total: u64 = sessions.iter().map(|s| s.bytes).sum();
        println!(
            "codex: {} sessions, {} bytes, scan {:?}",
            sessions.len(),
            total,
            elapsed
        );
        for s in sessions.iter().take(3) {
            println!(
                "  {} | title={:?} | project={:?} | messages={:?} | bytes={}",
                s.id, s.title, s.project, s.message_count, s.bytes
            );
        }
    }
}
