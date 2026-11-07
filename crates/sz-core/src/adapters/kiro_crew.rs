//! Kiro Crew adapter. Format spec: `docs/formats/kiro_crew.md`.
//!
//! Sessions are `~/.kiro/crew/sessions/<stem>.jsonl`, top level only. `sessions/archive/` holds
//! rotated copies and is not listed. Each transcript starts with a `{"_type": "metadata"}` header.
//!
//! Deletion artifacts per session:
//! - the transcript file,
//! - `session_map.json` key `stem` with `_` replaced by `:` (for example
//!   `dashboard:chat-1-1767866400`), when that key exists,
//! - the `<stem>.jsonl.lock` sidecar, only when the session is not live.
//!
//! Live rule (`live_session_ids`): the stem is listed in `open_slots.json` `keys`.
//!
//! Skipped on purpose:
//! - `dashboard_member-*` transcripts. The doc classes them as memory consolidation, not user
//!   sessions.
//! - `sessions/.index/session_index.db`. It is derived, and Crew rebuilds it after a deletion.
//!   Editing its FTS tables by hand is unsafe.
//! - `open_slots.json`. Its `keys` are plain strings, and the artifact model has no variant for
//!   removing a string from an array.
//! - Crew sidebar folders. A `folder_id` is not a filesystem path, so it cannot be a project.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rayon::prelude::*;
use serde::Deserialize;
use serde_json::Value;

use crate::adapter::Adapter;
use crate::model::{AgentKind, Artifact, PreviewMsg, Session};
use crate::util::truncate;

const TITLE_MAX: usize = 120;
const TEXT_MAX: usize = 2000;
const MEMORY_PREFIX: &str = "dashboard_member-";

pub struct KiroCrewAdapter {
    home: PathBuf,
}

impl KiroCrewAdapter {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }

    /// Stems of sessions listed in `open_slots.json`. Rule: see module docs.
    fn live_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .scan_store()
            .into_iter()
            .filter(|s| s.live)
            .map(|s| s.session.id)
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    fn scan_store(&self) -> Vec<Scanned> {
        let crew = self.home.join(".kiro/crew");
        let Ok(entries) = std::fs::read_dir(crew.join("sessions")) else {
            return Vec::new();
        };
        let files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension().and_then(|e| e.to_str()) == Some("jsonl")
                    && !p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with('.'))
            })
            .collect();
        let reg = Registry::read(&crew);
        files
            .par_iter()
            .filter_map(|p| crew_session(&crew, p, &reg))
            .collect()
    }
}

impl Adapter for KiroCrewAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::KiroCrew
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.home.join(".kiro/crew")]
    }

    fn scan(&self) -> anyhow::Result<Vec<Session>> {
        let mut sessions: Vec<Session> = self
            .scan_store()
            .into_iter()
            .map(|s| {
                let mut session = s.session;
                session.recompute_bytes();
                session
            })
            .collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(sessions)
    }

    fn preview(&self, s: &Session, max: usize) -> anyhow::Result<Vec<PreviewMsg>> {
        let mut out = Vec::new();
        let path = self
            .home
            .join(".kiro/crew/sessions")
            .join(format!("{}.jsonl", s.id));
        if max == 0 || !path.is_file() {
            return Ok(out);
        }
        for_each_line(&path, |line| {
            preview_line(line, &mut out);
            out.len() < max
        })?;
        Ok(out)
    }

    fn process_names(&self) -> &'static [&'static str] {
        &[
            "kirocrew",
            "KiroCrew",
            "KiroCrew Helper",
            "kirocrew-backend",
            "kiro_crew",
            "kiro-crew",
        ]
    }

    fn live_session_ids(&self) -> Vec<String> {
        self.live_ids()
    }
}

struct Scanned {
    session: Session,
    live: bool,
}

/// Registry data read once per scan.
struct Registry {
    /// `session_map.json` key to its `cwd`.
    cwd_by_key: HashMap<String, Option<String>>,
    /// Stems listed in `open_slots.json`.
    open: HashSet<String>,
}

#[derive(Deserialize)]
struct OpenSlots {
    #[serde(default)]
    keys: Vec<String>,
}

impl Registry {
    fn read(crew: &Path) -> Self {
        let cwd_by_key = std::fs::read(crew.join("session_map.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<HashMap<String, Value>>(&b).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| {
                let cwd = v.get("cwd").and_then(Value::as_str).map(str::to_string);
                (k, cwd)
            })
            .collect::<HashMap<String, Option<String>>>();
        let open = std::fs::read(crew.join("open_slots.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<OpenSlots>(&b).ok())
            .map(|s| s.keys.into_iter().collect::<HashSet<String>>())
            .unwrap_or_default();
        Self { cwd_by_key, open }
    }
}

#[derive(Deserialize)]
struct Header {
    #[serde(rename = "_type")]
    marker: Option<String>,
    title: Option<String>,
    project: Option<String>,
    created_at: Option<String>,
    last_user_at: Option<String>,
}

#[derive(Deserialize)]
struct Msg {
    role: Option<String>,
    ts: Option<String>,
}

struct Transcript {
    header: Option<Header>,
    messages: u64,
    first_user: Option<String>,
    last_msg_ts: Option<DateTime<Utc>>,
}

/// Header, user and assistant counts, the first user text, and the last message time.
/// Tool, system, inject, error and card lines are not counted.
fn summarize(path: &Path) -> Transcript {
    let mut t = Transcript {
        header: None,
        messages: 0,
        first_user: None,
        last_msg_ts: None,
    };
    let mut first_line = true;
    let _ = for_each_line(path, |line| {
        if std::mem::take(&mut first_line) {
            if let Ok(h) = serde_json::from_str::<Header>(line) {
                if h.marker.as_deref() == Some("metadata") {
                    t.header = Some(h);
                    return true;
                }
            }
        }
        let Ok(m) = serde_json::from_str::<Msg>(line) else {
            return true;
        };
        let is_user = m.role.as_deref() == Some("user");
        if !is_user && m.role.as_deref() != Some("assistant") {
            return true;
        }
        t.messages += 1;
        if let Some(ts) = m.ts.as_deref().and_then(parse_ts) {
            t.last_msg_ts = Some(ts);
        }
        if is_user && t.first_user.is_none() {
            t.first_user = line_text(line);
        }
        true
    });
    t
}

fn crew_session(crew: &Path, path: &Path, reg: &Registry) -> Option<Scanned> {
    let stem = path.file_stem()?.to_str()?.to_string();
    if stem.starts_with(MEMORY_PREFIX) {
        return None;
    }
    let t = summarize(path);
    let header = t.header.as_ref();
    let key = stem.replace('_', ":");
    let live = reg.open.contains(&stem);

    let title = header
        .and_then(|h| nonempty(h.title.clone()))
        .or_else(|| nonempty(t.first_user.clone()))
        .map(|s| truncate(&s, TITLE_MAX));
    let project = header
        .and_then(|h| nonempty(h.project.clone()))
        .or_else(|| nonempty(reg.cwd_by_key.get(&key).cloned().flatten()))
        .map(PathBuf::from);
    let created = header
        .and_then(|h| h.created_at.as_deref())
        .and_then(parse_ts);
    let last_active = header
        .and_then(|h| h.last_user_at.as_deref())
        .and_then(parse_ts)
        .or(t.last_msg_ts);

    let mut artifacts = vec![Artifact::File(path.to_path_buf())];
    if reg.cwd_by_key.contains_key(&key) {
        artifacts.push(Artifact::JsonObjectKey {
            file: crew.join("session_map.json"),
            object_path: String::new(),
            key,
        });
    }
    let lock = path.with_extension("jsonl.lock");
    if !live && lock.is_file() {
        artifacts.push(Artifact::File(lock));
    }

    let session = Session {
        parent_id: None,
        agent: AgentKind::KiroCrew,
        id: stem,
        title,
        project,
        created,
        last_active,
        message_count: Some(t.messages),
        bytes: 0,
        artifacts,
    };
    Some(Scanned { session, live })
}

fn preview_line(line: &str, out: &mut Vec<PreviewMsg>) {
    let Ok(m) = serde_json::from_str::<Msg>(line) else {
        return;
    };
    let role = match m.role.as_deref() {
        Some("user") => "user",
        Some("assistant") => "assistant",
        _ => return,
    };
    let Some(text) = line_text(line) else {
        return;
    };
    out.push(PreviewMsg {
        role: role.to_string(),
        text: truncate(&text, TEXT_MAX),
        ts: m.ts.as_deref().and_then(parse_ts),
    });
}

/// Text of a line's `content`: a string, or a list of `{kind, data}` text parts.
fn line_text(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    nonempty(body_text(v.get("content")?))
}

/// Calls `f` with each trimmed line. Invalid UTF-8 is repaired lossily. `f` returns `false` to
/// stop early. Lines that fail to parse are skipped by callers.
fn for_each_line(path: &Path, mut f: impl FnMut(&str) -> bool) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader};
    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        if !f(line.trim()) {
            break;
        }
    }
    Ok(())
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Text of a message body. A plain string is used as is. A list of `{kind, data}` parts yields
/// the `text` parts joined by newlines.
fn body_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let texts: Vec<&str> = parts
                .iter()
                .filter(|p| p.get("kind").and_then(Value::as_str) == Some("text"))
                .filter_map(|p| p.get("data").and_then(Value::as_str))
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

fn nonempty(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn fixture_home() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/kiro_crew")
    }

    fn expected() -> Vec<Value> {
        let text = std::fs::read_to_string(fixture_home().join("EXPECTED.json")).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        v["sessions"].as_array().unwrap().clone()
    }

    #[test]
    fn fixture_scan_matches_expected() {
        let adapter = KiroCrewAdapter::new(&fixture_home());
        let sessions = adapter.scan().unwrap();
        let exp = expected();
        assert_eq!(sessions.len(), exp.len());
        let live = adapter.live_session_ids();
        for e in &exp {
            let id = e["id"].as_str().unwrap();
            let s = sessions
                .iter()
                .find(|s| s.id == id)
                .unwrap_or_else(|| panic!("missing session {id}"));
            assert_eq!(s.agent, AgentKind::KiroCrew);
            // Header title "" falls back to the first user line. The fixture keeps that in
            // `title_fallback`.
            assert_eq!(
                s.title.as_deref(),
                e["title"].as_str().or(e["title_fallback"].as_str()),
                "title of {id}"
            );
            assert_eq!(s.project, e["project"].as_str().map(PathBuf::from), "project of {id}");
            assert_eq!(s.created, e["created"].as_str().and_then(parse_ts), "created of {id}");
            assert_eq!(
                s.last_active,
                e["last_active"].as_str().and_then(parse_ts),
                "last_active of {id}"
            );
            assert_eq!(s.message_count, e["message_count"].as_u64(), "messages of {id}");
            assert_eq!(
                live.contains(&id.to_string()),
                e["live"].as_bool().unwrap(),
                "live of {id}"
            );

            // Transcript and the session_map key are both deletion artifacts.
            let jsonl = fixture_home().join(e["artifacts_rel_to_home"][0].as_str().unwrap());
            assert!(s.artifacts.iter().any(|a| a.primary_path() == jsonl));
            let key = e["session_map_key"].as_str().unwrap();
            let has_key = s.artifacts.iter().any(|a| {
                matches!(a, Artifact::JsonObjectKey { key: k, .. } if k == key)
            });
            assert!(has_key, "session_map key of {id}");
            let bytes: u64 = s.artifacts.iter().map(Artifact::own_bytes).sum();
            assert_eq!(s.bytes, bytes, "bytes of {id}");
        }
    }

    #[test]
    fn fixture_preview_keeps_user_and_assistant_only() {
        let adapter = KiroCrewAdapter::new(&fixture_home());
        let sessions = adapter.scan().unwrap();
        let s = sessions
            .iter()
            .find(|s| s.id == "dashboard_chat-1-1767866400")
            .unwrap();
        let roles: Vec<String> = adapter
            .preview(s, 10)
            .unwrap()
            .into_iter()
            .map(|m| m.role)
            .collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
        assert_eq!(adapter.preview(s, 1).unwrap().len(), 1);
    }

    #[test]
    fn memory_transcripts_are_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".kiro/crew/sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join("dashboard_member-mate.memory-member-mate-abc.jsonl"),
            r#"{"role":"user","content":"x"}"#,
        )
        .unwrap();
        assert!(KiroCrewAdapter::new(dir.path()).scan().unwrap().is_empty());
    }

    #[test]
    fn lock_sidecar_is_deletable_only_when_not_live() {
        let dir = tempfile::tempdir().unwrap();
        let crew = dir.path().join(".kiro/crew");
        let sessions_dir = crew.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join("dashboard_chat-9-1.jsonl"),
            r#"{"role":"user","content":"hi"}"#,
        )
        .unwrap();
        std::fs::write(sessions_dir.join("dashboard_chat-9-1.jsonl.lock"), "").unwrap();

        let adapter = KiroCrewAdapter::new(dir.path());
        let s = &adapter.scan().unwrap()[0];
        let is_lock = |a: &Artifact| matches!(a, Artifact::File(p) if p.to_string_lossy().ends_with(".jsonl.lock"));
        assert!(s.artifacts.iter().any(is_lock));

        std::fs::write(crew.join("open_slots.json"), r#"{"keys":["dashboard_chat-9-1"],"ts":0}"#).unwrap();
        let s = &adapter.scan().unwrap()[0];
        assert!(!s.artifacts.iter().any(is_lock));
        assert_eq!(adapter.live_session_ids(), vec!["dashboard_chat-9-1".to_string()]);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".kiro/crew/sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join("dashboard_chat-3-1.jsonl"),
            "{broken\n{\"role\":\"user\",\"content\":\"first\",\"ts\":\"2026-01-08T10:00:00+00:00\"}\n\n[1,2\n{\"role\":\"assistant\",\"content\":\"ok\"}\n",
        )
        .unwrap();
        let s = &KiroCrewAdapter::new(dir.path()).scan().unwrap()[0];
        assert_eq!(s.message_count, Some(2));
        assert_eq!(s.title.as_deref(), Some("first"));
    }

    /// Read-only scan of the real home. Run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_scan() {
        let home = dirs::home_dir().expect("home dir");
        let adapter = KiroCrewAdapter::new(&home);
        let t = Instant::now();
        let sessions = adapter.scan().unwrap();
        let elapsed = t.elapsed();
        let total: u64 = sessions.iter().map(|s| s.bytes).sum();
        println!(
            "kiro_crew: count={} total_bytes={} scan_time={:?}",
            sessions.len(),
            total,
            elapsed
        );
        for s in sessions.iter().take(3) {
            println!(
                "sample: id={} project={:?} bytes={} messages={:?}",
                s.id,
                s.project,
                s.bytes,
                s.message_count
            );
        }
        println!("live ids: {:?}", adapter.live_session_ids());
    }
}
