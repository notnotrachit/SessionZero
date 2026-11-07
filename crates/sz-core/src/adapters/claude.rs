//! Claude Code adapter. Format notes: docs/formats/claude.md.
//!
//! Transcripts are `projects/<slug>/<UUID>.jsonl`. The slug is lossy, so the project path comes
//! from the transcript `cwd` field or the index `projectPath`, never from the slug.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rayon::prelude::*;
use serde::Deserialize;
use serde_json::Value;

use crate::adapter::Adapter;
use crate::model::{AgentKind, Artifact, PreviewMsg, Session};
use crate::util::{mtime, truncate};

const TITLE_MAX: usize = 120;
const PREVIEW_TEXT_MAX: usize = 2000;

pub struct ClaudeAdapter {
    home: PathBuf,
}

impl ClaudeAdapter {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }
}

impl Adapter for ClaudeAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Claude
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.home.join(".claude/projects")]
    }

    fn scan(&self) -> anyhow::Result<Vec<Session>> {
        let claude = self.home.join(".claude");
        let shared = load_shared(&claude);

        let mut projects: Vec<Project> = Vec::new();
        let mut jobs: Vec<(usize, PathBuf)> = Vec::new();
        for dir in project_dirs(&claude.join("projects")) {
            let project = load_project(dir);
            let idx = projects.len();
            jobs.extend(transcripts(&project.dir).into_iter().map(|t| (idx, t)));
            projects.push(project);
        }

        let mut sessions: Vec<Session> = jobs
            .par_iter()
            .filter_map(|(idx, path)| build_session(path, &projects[*idx], &shared))
            .collect();
        sessions.sort_by(|a, b| {
            b.last_active
                .cmp(&a.last_active)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(sessions)
    }

    fn preview(&self, s: &Session, max: usize) -> anyhow::Result<Vec<PreviewMsg>> {
        let mut out = Vec::new();
        let Some(path) = s.artifacts.iter().find_map(|a| match a {
            Artifact::File(p) if p.extension().is_some_and(|e| e == "jsonl") => Some(p),
            _ => None,
        }) else {
            return Ok(out);
        };
        if max == 0 {
            return Ok(out);
        }
        for_each_line(path, |line| {
            if !(line.contains("\"type\":\"user\"") || line.contains("\"type\":\"assistant\"")) {
                return true;
            }
            let Some(rec) = parse_rec(line) else {
                return true;
            };
            if rec.sidechain.unwrap_or(false) {
                return true;
            }
            let Some(kind) = rec
                .kind
                .filter(|k| matches!(k.as_str(), "user" | "assistant"))
            else {
                return true;
            };
            let Some(text) = rec
                .message
                .as_ref()
                .map(message_text)
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
            else {
                return true;
            };
            out.push(PreviewMsg {
                role: kind,
                text: truncate(&text, PREVIEW_TEXT_MAX),
                ts: extract_timestamp(line),
            });
            out.len() < max
        });
        Ok(out)
    }

    fn process_names(&self) -> &'static [&'static str] {
        &["claude"]
    }
}

/// Lookup data read once per scan and shared by every transcript.
struct Shared {
    claude_dir: PathBuf,
    history_file: PathBuf,
    /// Session ids that appear in `history.jsonl` as `sessionId`.
    history_ids: HashSet<String>,
    /// Session id -> `todos/<id>-*.json` files.
    todos: HashMap<String, Vec<PathBuf>>,
}

/// One `projects/<slug>` directory and its `sessions-index.json`.
struct Project {
    dir: PathBuf,
    index_file: PathBuf,
    /// Raw index text. Used to decide whether the index holds a row for a session.
    index_text: Option<String>,
    entries: HashMap<String, IndexMeta>,
}

#[derive(Clone, Default)]
struct IndexMeta {
    summary: Option<String>,
    first_prompt: Option<String>,
    project_path: Option<String>,
    created: Option<String>,
    modified: Option<String>,
}

#[derive(Deserialize)]
struct HistoryRow {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

/// The fields of a transcript record this adapter reads.
#[derive(Deserialize)]
struct Rec {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "isSidechain")]
    sidechain: Option<bool>,
    cwd: Option<String>,
    message: Option<Value>,
}

/// Fields read from one transcript file.
#[derive(Default)]
struct Transcript {
    first_ts: Option<DateTime<Utc>>,
    last_ts: Option<DateTime<Utc>>,
    messages: u64,
    cwd: Option<String>,
    first_user: Option<String>,
}

fn load_shared(claude: &Path) -> Shared {
    let history_file = claude.join("history.jsonl");
    let mut history_ids = HashSet::new();
    for_each_line(&history_file, |line| {
        if line.contains("\"sessionId\"") {
            if let Ok(HistoryRow {
                session_id: Some(id),
            }) = serde_json::from_str::<HistoryRow>(line)
            {
                history_ids.insert(id);
            }
        }
        true
    });

    let mut todos: HashMap<String, Vec<PathBuf>> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(claude.join("todos")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".json") {
                continue;
            }
            if let Some(id) = id_prefix(&name) {
                todos
                    .entry(id.to_string())
                    .or_default()
                    .push(entry.path());
            }
        }
    }

    Shared {
        claude_dir: claude.to_path_buf(),
        history_file,
        history_ids,
        todos,
    }
}

fn project_dirs(projects: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(projects)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

/// Top-level `*.jsonl` files of a project directory. `*.jsonl.wakatime` is not a transcript.
fn transcripts(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".jsonl"))
        })
        .collect()
}

fn load_project(dir: PathBuf) -> Project {
    let index_file = dir.join("sessions-index.json");
    let index_text = std::fs::read_to_string(&index_file).ok();
    let mut entries = HashMap::new();
    if let Some(root) = index_text
        .as_deref()
        .and_then(|t| serde_json::from_str::<Value>(t).ok())
    {
        for e in root
            .get("entries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(id) = str_field(e, "sessionId") {
                entries.insert(
                    id,
                    IndexMeta {
                        summary: str_field(e, "summary"),
                        first_prompt: str_field(e, "firstPrompt"),
                        project_path: str_field(e, "projectPath"),
                        created: str_field(e, "created"),
                        modified: str_field(e, "modified"),
                    },
                );
            }
        }
    }
    Project {
        dir,
        index_file,
        index_text,
        entries,
    }
}

fn build_session(path: &Path, project: &Project, shared: &Shared) -> Option<Session> {
    let file_name = path.file_name()?.to_str()?;
    let id = file_name.strip_suffix(".jsonl")?.to_string();

    let mut artifacts = vec![Artifact::File(path.to_path_buf())];
    let wakatime = path.with_file_name(format!("{file_name}.wakatime"));
    if wakatime.is_file() {
        artifacts.push(Artifact::File(wakatime));
    }
    let subagents = project.dir.join(&id);
    if subagents.is_dir() {
        artifacts.push(Artifact::Dir(subagents));
    }
    if project
        .index_text
        .as_deref()
        .is_some_and(|t| t.contains(&id))
    {
        artifacts.push(Artifact::JsonArrayEntry {
            file: project.index_file.clone(),
            array_path: "entries".into(),
            field: "sessionId".into(),
            value: id.clone(),
        });
    }
    if shared.history_ids.contains(&id) {
        artifacts.push(Artifact::JsonlLines {
            file: shared.history_file.clone(),
            field: "sessionId".into(),
            value: id.clone(),
        });
    }
    for dir in [
        shared.claude_dir.join("session-env").join(&id),
        shared.claude_dir.join("file-history").join(&id),
    ] {
        if dir.is_dir() {
            artifacts.push(Artifact::Dir(dir));
        }
    }
    if let Some(files) = shared.todos.get(&id) {
        artifacts.extend(files.iter().cloned().map(Artifact::File));
    }

    let meta = scan_transcript(path);
    let index = project.entries.get(&id);
    let fallback_ts = mtime(path);

    let title = index
        .and_then(|m| m.summary.as_deref().or(m.first_prompt.as_deref()))
        .and_then(normalize_title)
        .or_else(|| meta.first_user.as_deref().and_then(normalize_title));
    let project_dir = meta
        .cwd
        .or_else(|| index.and_then(|m| m.project_path.clone()))
        .map(PathBuf::from);

    let mut session = Session {
        parent_id: None,
        agent: AgentKind::Claude,
        id,
        title,
        project: project_dir,
        created: meta
            .first_ts
            .or_else(|| index.and_then(|m| m.created.as_deref()).and_then(parse_ts))
            .or(fallback_ts),
        last_active: meta
            .last_ts
            .or_else(|| index.and_then(|m| m.modified.as_deref()).and_then(parse_ts))
            .or(fallback_ts),
        message_count: Some(meta.messages),
        bytes: 0,
        artifacts,
    };
    session.recompute_bytes();
    Some(session)
}

/// Reads one transcript. Counts non-sidechain user and assistant records, and takes the first
/// `cwd`, the first and last timestamps and the first user text.
fn scan_transcript(path: &Path) -> Transcript {
    let mut t = Transcript::default();
    for_each_line(path, |line| {
        if let Some(ts) = extract_timestamp(line) {
            t.first_ts.get_or_insert(ts);
            t.last_ts = Some(ts);
        }
        let may_count =
            line.contains("\"type\":\"user\"") || line.contains("\"type\":\"assistant\"");
        let needs_cwd = t.cwd.is_none() && line.contains("\"cwd\":\"");
        if !may_count && !needs_cwd {
            return true;
        }
        let Some(rec) = parse_rec(line) else {
            return true;
        };
        if t.cwd.is_none() {
            t.cwd = rec.cwd.filter(|c| !c.trim().is_empty());
        }
        let kind = rec.kind.as_deref();
        if !matches!(kind, Some("user") | Some("assistant")) || rec.sidechain.unwrap_or(false) {
            return true;
        }
        t.messages += 1;
        if kind == Some("user") && t.first_user.is_none() {
            t.first_user = rec
                .message
                .as_ref()
                .map(message_text)
                .filter(|s| !s.trim().is_empty());
        }
        true
    });
    t
}

/// Calls `f` for each non-empty line of `path`, with surrounding whitespace trimmed. Invalid
/// UTF-8 is replaced. `f` returns false to stop early. Returns false when the file cannot be
/// opened.
fn for_each_line(path: &Path, mut f: impl FnMut(&str) -> bool) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf);
        let trimmed = line.trim();
        if !trimmed.is_empty() && !f(trimmed) {
            break;
        }
    }
    true
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

/// Text of a message. `content` is either a string or an array of blocks, and only `text`
/// blocks count.
fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Session id prefix of a `<UUID>-...` or `<UUID>.` file name.
fn id_prefix(name: &str) -> Option<&str> {
    let id = name.get(..36)?;
    let next = name.as_bytes().get(36)?;
    (is_uuid(id.as_bytes()) && matches!(next, b'-' | b'.')).then_some(id)
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
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/claude")
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

        let mut sessions = ClaudeAdapter::new(&home).scan().expect("scan");
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

            for (key, count) in e["index_refs"].as_object().expect("index_refs") {
                let present = s.artifacts.iter().any(|a| match a {
                    Artifact::JsonArrayEntry { file, .. } | Artifact::JsonlLines { file, .. } => {
                        rel(&home, file) == *key
                    }
                    _ => false,
                });
                assert_eq!(
                    present,
                    count.as_u64().expect("count") > 0,
                    "index ref {key} of {id}"
                );
            }
            assert_eq!(s.bytes, s.artifacts.iter().map(Artifact::own_bytes).sum::<u64>());
        }
    }

    #[test]
    fn preview_returns_non_sidechain_messages_in_order() {
        let home = fixture_home();
        let adapter = ClaudeAdapter::new(&home);
        let sessions = adapter.scan().expect("scan");
        let s = sessions
            .iter()
            .find(|s| s.id == "5f1c0a01-1a2b-4c3d-8e9f-000000000001")
            .expect("fixture session");

        let msgs = adapter.preview(s, 10).expect("preview");
        let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
        assert_eq!(msgs[0].text, "Rename the parser module to ast_parse.");
        assert_eq!(adapter.preview(s, 1).expect("preview").len(), 1);
    }

    #[test]
    fn sidechain_and_malformed_records_are_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let proj = dir.path().join(".claude/projects/-tmp-x");
        std::fs::create_dir_all(&proj).expect("mkdir");
        let id = "5f1c0a09-1111-4222-8333-000000000009";
        let body = [
            r#"{"type":"user","cwd":"/tmp/x","isSidechain":false,"message":{"role":"user","content":"Hello there"},"sessionId":"x","timestamp":"2026-11-01T10:00:00.000Z"}"#,
            "{broken",
            r#"{"type":"assistant","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"side"}]},"timestamp":"2026-11-01T10:00:01.000Z"}"#,
            r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"tool_use","name":"x"}]},"timestamp":"2026-11-01T10:00:02.000Z"}"#,
            r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"Done."}]},"timestamp":"2026-11-01T10:00:03.000Z"}"#,
        ]
        .join("\n");
        std::fs::write(proj.join(format!("{id}.jsonl")), body).expect("write");
        std::fs::write(proj.join(format!("{id}.jsonl.wakatime")), "{}").expect("write");

        let adapter = ClaudeAdapter::new(dir.path());
        let sessions = adapter.scan().expect("scan");
        assert_eq!(sessions.len(), 1, "wakatime file must not be a session");
        let s = &sessions[0];
        assert_eq!(s.title.as_deref(), Some("Hello there"));
        assert_eq!(s.message_count, Some(3));
        assert_eq!(s.project, Some(PathBuf::from("/tmp/x")));
        assert_eq!(s.last_active, Some(iso("2026-11-01T10:00:03Z")));
        assert_eq!(s.artifacts.len(), 2);

        let msgs = adapter.preview(s, 10).expect("preview");
        let texts: Vec<&str> = msgs.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["Hello there", "Done."]);
    }

    #[test]
    fn id_prefix_matches_todo_names() {
        let id = "5f1c0a01-1a2b-4c3d-8e9f-000000000001";
        assert_eq!(id_prefix(&format!("{id}-agent-x.json")), Some(id));
        assert_eq!(id_prefix(&format!("{id}.json")), Some(id));
        assert_eq!(id_prefix("notes.json"), None);
    }

    #[test]
    #[ignore]
    fn live_scan() {
        let home = dirs::home_dir().expect("home dir");
        let started = Instant::now();
        let sessions = ClaudeAdapter::new(&home).scan().expect("scan");
        let elapsed = started.elapsed();
        let total: u64 = sessions.iter().map(|s| s.bytes).sum();
        println!(
            "claude: {} sessions, {} bytes, scan {:?}",
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
