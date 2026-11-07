//! GitHub Copilot CLI adapter. Format notes: docs/formats/copilot.md.
//!
//! A session is one directory `~/.copilot/session-state/<UUID>/`. The shared files
//! (`vscode.session.metadata.cache.json`, `history-session-state/`, `logs/`) are attached to the
//! session whose id they carry. The orphan cache keys and logs are not sessions and are ignored.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rayon::prelude::*;
use serde_json::Value;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::adapter::Adapter;
use crate::model::{AgentKind, Artifact, PreviewMsg, Session};
use crate::util::{mtime, normalize_project, truncate};

const TITLE_MAX: usize = 120;
const PREVIEW_TEXT_MAX: usize = 2000;
const PROCESS_NAMES: &[&str] = &["copilot", "github-copilot", "@github/copilot"];
const CACHE_FILE: &str = "vscode.session.metadata.cache.json";

pub struct CopilotAdapter {
    home: PathBuf,
}

impl CopilotAdapter {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }

    fn copilot_dir(&self) -> PathBuf {
        self.home.join(".copilot")
    }
}

impl Adapter for CopilotAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Copilot
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.copilot_dir()]
    }

    fn scan(&self) -> anyhow::Result<Vec<Session>> {
        let root = self.copilot_dir();
        let shared = Shared::load(&root);
        let dirs = session_dirs(&root.join("session-state"));
        let mut sessions: Vec<Session> = dirs
            .par_iter()
            .filter_map(|dir| build_session(dir, &self.home, &shared))
            .collect();
        sessions.sort_by(|a, b| {
            b.last_active
                .cmp(&a.last_active)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(sessions)
    }

    fn preview(&self, s: &Session, max: usize) -> anyhow::Result<Vec<PreviewMsg>> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let events = self
            .copilot_dir()
            .join("session-state")
            .join(&s.id)
            .join("events.jsonl");
        if events.is_file() {
            return Ok(preview_events(&events, max));
        }
        let history = s.artifacts.iter().find_map(|a| match a {
            Artifact::File(p) if is_history_file(p) => Some(p),
            _ => None,
        });
        Ok(history
            .map(|p| preview_history(p, max))
            .unwrap_or_default())
    }

    fn process_names(&self) -> &'static [&'static str] {
        PROCESS_NAMES
    }

    fn live_session_ids(&self) -> Vec<String> {
        let known: HashSet<String> = session_dirs(&self.copilot_dir().join("session-state"))
            .iter()
            .filter_map(|d| d.file_name()?.to_str().map(str::to_string))
            .collect();
        live_ids_from(&copilot_process_argvs(), &known)
    }
}

/// Lookup data read once per scan and shared by every session.
struct Shared {
    /// Session id -> files in `history-session-state/`, sorted.
    history: HashMap<String, Vec<PathBuf>>,
    /// Session id -> `logs/<id>.log`.
    logs: HashMap<String, PathBuf>,
    cache_file: PathBuf,
    /// Every key of the metadata cache, including keys with no session directory.
    cache_keys: HashSet<String>,
    /// Cache key -> `firstUserMessage`, when the entry has one.
    cache_first_message: HashMap<String, String>,
}

impl Shared {
    fn load(root: &Path) -> Self {
        let mut history: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for entry in std::fs::read_dir(root.join("history-session-state"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if let Some(id) = history_file_session_id(&path) {
                history.entry(id).or_default().push(path);
            }
        }
        for files in history.values_mut() {
            files.sort();
        }

        let mut logs = HashMap::new();
        for entry in std::fs::read_dir(root.join("logs")).into_iter().flatten().flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".log") {
                if path.is_file() && !id.starts_with("process-") {
                    logs.insert(id.to_string(), path);
                }
            }
        }

        let cache_file = root.join(CACHE_FILE);
        let mut cache_keys = HashSet::new();
        let mut cache_first_message = HashMap::new();
        if let Some(map) = std::fs::read(&cache_file)
            .ok()
            .and_then(|b| serde_json::from_slice::<HashMap<String, Value>>(&b).ok())
        {
            for (key, value) in map {
                if let Some(msg) = value.get("firstUserMessage").and_then(Value::as_str) {
                    if let Some(t) = normalize_title(msg) {
                        cache_first_message.insert(key.clone(), t);
                    }
                }
                cache_keys.insert(key);
            }
        }

        Self {
            history,
            logs,
            cache_file,
            cache_keys,
            cache_first_message,
        }
    }
}

/// Subdirectories of `session-state/`. Hidden entries are skipped.
fn session_dirs(session_state: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(session_state)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !n.starts_with('.'))
        })
        .collect()
}

/// Session id of `session_<id>_<epoch-ms>.json`.
fn history_file_session_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_prefix("session_")?.strip_suffix(".json")?;
    let (id, _millis) = stem.rsplit_once('_')?;
    (!id.is_empty()).then(|| id.to_string())
}

fn is_history_file(path: &Path) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == "history-session-state")
}

fn build_session(dir: &Path, home: &Path, shared: &Shared) -> Option<Session> {
    let id = dir.file_name()?.to_str()?.to_string();
    let ws = read_workspace(&dir.join("workspace.yaml"));
    let events = scan_events(&dir.join("events.jsonl"));
    let history_files = shared.history.get(&id).cloned().unwrap_or_default();
    let history = history_files.first().and_then(|p| scan_history(p));

    let mut session = Session::new(AgentKind::Copilot, id.clone());
    session.title = ws
        .summary
        .as_deref()
        .and_then(normalize_title)
        .or_else(|| shared.cache_first_message.get(&id).cloned())
        .or_else(|| {
            events
                .as_ref()
                .and_then(|e| e.first_user.as_deref())
                .and_then(normalize_title)
        })
        .or_else(|| {
            history
                .as_ref()
                .and_then(|h| h.first_user.as_deref())
                .and_then(normalize_title)
        });

    let mut project = ws
        .cwd
        .clone()
        .or_else(|| events.as_ref().and_then(|e| e.cwd.clone()))
        .map(PathBuf::from);
    normalize_project(&mut project, Some(home));
    session.project = project;

    let events_last = events.as_ref().and_then(|e| e.last_ts);
    session.created = ws
        .created
        .or_else(|| events.as_ref().and_then(|e| e.start))
        .or_else(|| history.as_ref().and_then(|h| h.start))
        .or_else(|| mtime(dir));
    session.last_active = max_opt(ws.updated, events_last)
        .or_else(|| history.as_ref().and_then(|h| h.last_ts))
        .or_else(|| mtime(dir));

    session.message_count = Some(match (&events, &history) {
        (Some(e), _) => e.messages,
        (None, Some(h)) => h.messages,
        (None, None) => 0,
    });

    let mut artifacts = vec![Artifact::Dir(dir.to_path_buf())];
    artifacts.extend(history_files.into_iter().map(Artifact::File));
    if shared.cache_keys.contains(&id) {
        artifacts.push(Artifact::JsonObjectKey {
            file: shared.cache_file.clone(),
            object_path: String::new(),
            key: id.clone(),
        });
    }
    if let Some(log) = shared.logs.get(&id) {
        artifacts.push(Artifact::File(log.clone()));
    }
    session.artifacts = artifacts;
    session.recompute_bytes();
    Some(session)
}

fn max_opt(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

#[derive(Default)]
struct Workspace {
    summary: Option<String>,
    cwd: Option<String>,
    created: Option<DateTime<Utc>>,
    updated: Option<DateTime<Utc>>,
}

/// Reads the top-level scalar keys of `workspace.yaml` that this adapter uses. Quoted values and
/// block scalars (`|`, `>`) are handled. Nested mappings are ignored. A missing file yields an
/// empty result.
fn read_workspace(path: &Path) -> Workspace {
    let mut ws = Workspace::default();
    let Ok(text) = std::fs::read_to_string(path) else {
        return ws;
    };
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if line.is_empty() || line.starts_with(char::is_whitespace) || line.starts_with('#') {
            continue;
        }
        let Some((key, raw)) = line.split_once(':') else {
            continue;
        };
        let mut value = raw.trim().to_string();
        if matches!(value.as_str(), "" | "|" | ">" | "|-" | ">-" | "|+" | ">+") {
            let mut parts = Vec::new();
            while let Some(&next) = lines.peek() {
                if !next.starts_with(char::is_whitespace) || next.trim().is_empty() {
                    break;
                }
                parts.push(next.trim().to_string());
                lines.next();
            }
            value = parts.join(" ");
        }
        let value = unquote(&value);
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "summary" => ws.summary = Some(value),
            "cwd" => ws.cwd = Some(value),
            "created_at" => ws.created = parse_ts(&value),
            "updated_at" => ws.updated = parse_ts(&value),
            _ => {}
        }
    }
    ws
}

fn unquote(v: &str) -> String {
    let b = v.as_bytes();
    if v.len() >= 2 && b[0] == b'"' && b[v.len() - 1] == b'"' {
        return serde_json::from_str::<String>(v).unwrap_or_else(|_| v[1..v.len() - 1].to_string());
    }
    if v.len() >= 2 && b[0] == b'\'' && b[v.len() - 1] == b'\'' {
        return v[1..v.len() - 1].replace("''", "'");
    }
    v.to_string()
}

#[derive(Default)]
struct Events {
    /// `startTime` of the `session.start` record, else the first timestamp.
    start: Option<DateTime<Utc>>,
    last_ts: Option<DateTime<Utc>>,
    /// Non-empty main-thread user and assistant messages.
    messages: u64,
    /// `context.cwd` of the `session.start` record.
    cwd: Option<String>,
    first_user: Option<String>,
}

/// Reads `events.jsonl`. Malformed lines are skipped. Subagent messages (those with a
/// `parentToolCallId`) are not counted. Returns None when the file is missing.
fn scan_events(path: &Path) -> Option<Events> {
    if !path.is_file() {
        return None;
    }
    let mut ev = Events::default();
    for_each_line(path, |line| {
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            return true;
        };
        let kind = rec.get("type").and_then(Value::as_str).unwrap_or("");
        let ts = rec
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_ts);
        if let Some(ts) = ts {
            ev.start.get_or_insert(ts);
            ev.last_ts = Some(ts);
        }
        let data = rec.get("data");
        match kind {
            "session.start" => {
                if let Some(start) = data
                    .and_then(|d| d.get("startTime"))
                    .and_then(Value::as_str)
                    .and_then(parse_ts)
                {
                    ev.start = Some(start);
                }
                if ev.cwd.is_none() {
                    ev.cwd = data
                        .and_then(|d| d.pointer("/context/cwd"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                }
            }
            "user.message" | "assistant.message" => {
                if is_subagent(data) {
                    return true;
                }
                let Some(text) = message_content(data) else {
                    return true;
                };
                ev.messages += 1;
                if kind == "user.message" && ev.first_user.is_none() {
                    ev.first_user = Some(text);
                }
            }
            _ => {}
        }
        true
    });
    Some(ev)
}

#[derive(Default)]
struct History {
    start: Option<DateTime<Utc>>,
    last_ts: Option<DateTime<Utc>>,
    messages: u64,
    first_user: Option<String>,
}

/// Reads one `history-session-state/session_<id>_<ms>.json` file. Counts `user` and `assistant`
/// chat messages that have text. Returns None when the file cannot be read as JSON.
fn scan_history(path: &Path) -> Option<History> {
    let root: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let mut h = History {
        start: root
            .get("startTime")
            .and_then(Value::as_str)
            .and_then(parse_ts),
        ..History::default()
    };
    for msg in root
        .get("chatMessages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let role = msg.get("role").and_then(Value::as_str);
        if !matches!(role, Some("user") | Some("assistant")) {
            continue;
        }
        let Some(text) = msg
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
        else {
            continue;
        };
        h.messages += 1;
        if role == Some("user") && h.first_user.is_none() {
            h.first_user = Some(text.to_string());
        }
    }
    h.last_ts = root
        .get("timeline")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.get("timestamp").and_then(Value::as_str).and_then(parse_ts))
        .max();
    Some(h)
}

fn is_subagent(data: Option<&Value>) -> bool {
    data.and_then(|d| d.get("parentToolCallId"))
        .is_some_and(|v| !v.is_null())
}

/// Trimmed text of a message event's `content`, or None when it is empty or not a string.
fn message_content(data: Option<&Value>) -> Option<String> {
    data.and_then(|d| d.get("content"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn preview_events(path: &Path, max: usize) -> Vec<PreviewMsg> {
    let mut out = Vec::new();
    for_each_line(path, |line| {
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            return true;
        };
        let role = match rec.get("type").and_then(Value::as_str) {
            Some("user.message") => "user",
            Some("assistant.message") => "assistant",
            _ => return true,
        };
        let data = rec.get("data");
        if is_subagent(data) {
            return true;
        }
        let Some(text) = message_content(data) else {
            return true;
        };
        out.push(PreviewMsg {
            role: role.to_string(),
            text: truncate(&text, PREVIEW_TEXT_MAX),
            ts: rec
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_ts),
        });
        out.len() < max
    });
    out
}

fn preview_history(path: &Path, max: usize) -> Vec<PreviewMsg> {
    let Some(root) = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return Vec::new();
    };
    root.get("chatMessages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|msg| {
            let role = msg.get("role").and_then(Value::as_str)?;
            if !matches!(role, "user" | "assistant") {
                return None;
            }
            let text = message_content(Some(msg))?;
            Some(PreviewMsg {
                role: role.to_string(),
                text: truncate(&text, PREVIEW_TEXT_MAX),
                ts: None,
            })
        })
        .take(max)
        .collect()
}

/// Argument vectors of running Copilot CLI processes.
fn copilot_process_argvs() -> Vec<Vec<String>> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::Always),
    );
    sys.processes()
        .values()
        .filter_map(|p| {
            let argv: Vec<String> = p
                .cmd()
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let name = p.name().to_string_lossy().into_owned();
            is_copilot_process(&name, p.exe(), &argv).then_some(argv)
        })
        .collect()
}

/// True when a process is the Copilot CLI. The CLI ships as a Node package, so its entry script
/// sits under a path such as `.../copilot/index.js`. Every `/` segment of each command word is
/// therefore compared, not only the last one, as well as the process name and executable path
/// components. Comparison is case-insensitive and whole-token.
fn is_copilot_process(name: &str, exe: Option<&Path>, argv: &[String]) -> bool {
    let mut tokens: Vec<String> = vec![name.to_lowercase()];
    if let Some(exe) = exe {
        tokens.extend(
            exe.components()
                .filter_map(|c| c.as_os_str().to_str())
                .map(str::to_lowercase),
        );
    }
    for arg in argv {
        for word in arg.split_whitespace() {
            tokens.extend(word.split('/').map(str::to_lowercase));
        }
    }
    PROCESS_NAMES
        .iter()
        .any(|w| tokens.iter().any(|t| t == w))
}

/// Session ids named in the argument vectors of Copilot processes. A UUID counts as a whole token,
/// so `--resume=<id>` and `--resume <id>` both match. Processes that name no known session yield
/// an empty result.
fn live_ids_from(argvs: &[Vec<String>], known: &HashSet<String>) -> Vec<String> {
    let mut live = BTreeSet::new();
    for argv in argvs {
        for arg in argv {
            for tok in arg.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')) {
                if known.contains(tok) {
                    live.insert(tok.to_string());
                }
            }
        }
    }
    live.into_iter().collect()
}

/// Collapses whitespace and truncates a title. Returns None when nothing is left.
fn normalize_title(raw: &str) -> Option<String> {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then(|| truncate(&collapsed, TITLE_MAX))
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Calls `f` for each non-empty line of `path`, trimmed. Invalid UTF-8 is replaced. `f` returns
/// false to stop early.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn fixture_home() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/copilot")
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

        let mut sessions = CopilotAdapter::new(&home).scan().expect("scan");
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(sessions.len(), expected.len(), "orphan cache key must not be a session");

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
            assert_eq!(s.message_count, e["message_count"].as_u64(), "message_count of {id}");
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
            assert_eq!(s.parent_id.as_deref(), e["parent_id"].as_str(), "parent of {id}");

            let mut files: Vec<String> = s
                .artifacts
                .iter()
                .map(|a| rel(&home, a.primary_path()))
                .collect();
            files.sort();
            let mut want: Vec<String> = e["artifacts"]
                .as_array()
                .expect("artifacts")
                .iter()
                .map(|v| v.as_str().expect("path").to_string())
                .collect();
            want.sort();
            assert_eq!(files, want, "artifacts of {id}");
            assert_eq!(s.bytes, s.artifacts.iter().map(Artifact::own_bytes).sum::<u64>());
        }
    }

    #[test]
    fn preview_reads_main_thread_messages_in_order() {
        let home = fixture_home();
        let adapter = CopilotAdapter::new(&home);
        let sessions = adapter.scan().expect("scan");
        let s = sessions
            .iter()
            .find(|s| s.id == "7c1d2e01-aaaa-4b1c-9d00-000000000001")
            .expect("fixture session");

        let msgs = adapter.preview(s, 10).expect("preview");
        let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
        assert!(msgs[0].text.starts_with("The sync worker retries"));
        assert!(msgs.iter().all(|m| !m.text.contains("Subagent")));
        assert_eq!(adapter.preview(s, 1).expect("preview").len(), 1);
    }

    #[test]
    fn preview_falls_back_to_history_file() {
        let home = fixture_home();
        let adapter = CopilotAdapter::new(&home);
        let sessions = adapter.scan().expect("scan");
        let s = sessions
            .iter()
            .find(|s| s.id == "7c1d2e03-cccc-4b1c-9d00-000000000003")
            .expect("history-only session");
        let msgs = adapter.preview(s, 10).expect("preview");
        let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["user", "assistant"]);
    }

    #[test]
    fn malformed_lines_and_missing_workspace_are_tolerated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = "7c1d2e09-9999-4b1c-9d00-000000000009";
        let sdir = dir.path().join(".copilot/session-state").join(id);
        std::fs::create_dir_all(&sdir).expect("mkdir");
        let body = [
            "not json at all",
            r#"{"type":"user.message","data":{"content":"Hello there"},"timestamp":"2026-11-01T10:00:00.000Z"}"#,
            r#"{"type":"user.message","data":{"content":"unterminated"#,
            "",
            r#"{"type":"assistant.message","data":{"content":"Hi"},"timestamp":"2026-11-01T10:00:05.000Z"}"#,
        ]
        .join("\n");
        std::fs::write(sdir.join("events.jsonl"), body).expect("write");

        let adapter = CopilotAdapter::new(dir.path());
        let sessions = adapter.scan().expect("scan");
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.title.as_deref(), Some("Hello there"));
        assert_eq!(s.message_count, Some(2));
        assert_eq!(s.project, None);
        assert_eq!(s.last_active, Some(iso("2026-11-01T10:00:05Z")));
        assert_eq!(s.artifacts.len(), 1);
    }

    #[test]
    fn workspace_yaml_quoted_and_block_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspace.yaml");
        std::fs::write(
            &path,
            "id: x\ncwd: '/tmp/it''s here'\nsummary: \"Quoted \\\"title\\\"\"\nupdated_at: 2026-11-01T10:00:00.000Z\n",
        )
        .expect("write");
        let ws = read_workspace(&path);
        assert_eq!(ws.cwd.as_deref(), Some("/tmp/it's here"));
        assert_eq!(ws.summary.as_deref(), Some("Quoted \"title\""));
        assert_eq!(ws.updated, Some(iso("2026-11-01T10:00:00Z")));

        std::fs::write(&path, "summary: |\n  first line\n  second line\ncwd: /a\n").expect("write");
        let ws = read_workspace(&path);
        assert_eq!(ws.summary.as_deref(), Some("first line second line"));
        assert_eq!(ws.cwd.as_deref(), Some("/a"));
    }

    #[test]
    fn history_file_name_yields_session_id() {
        assert_eq!(
            history_file_session_id(Path::new(
                "session_7c1d2e03-cccc-4b1c-9d00-000000000003_1793000000000.json"
            ))
            .as_deref(),
            Some("7c1d2e03-cccc-4b1c-9d00-000000000003")
        );
        assert_eq!(history_file_session_id(Path::new("notes.json")), None);
    }

    #[test]
    fn copilot_process_detection_covers_node_entry_scripts() {
        let node_argv = vec![
            "node".to_string(),
            "/Users/x/.copilot/pkg/darwin-arm64/0.0.406/copilot/index.js".to_string(),
        ];
        assert!(is_copilot_process("node", None, &node_argv));
        assert!(is_copilot_process("copilot", None, &[]));
        let bash = vec!["bash".to_string(), "-l".to_string()];
        assert!(!is_copilot_process("bash", Some(Path::new("/bin/bash")), &bash));
    }

    #[test]
    fn live_ids_match_whole_uuid_tokens_only() {
        let id = "7c1d2e01-aaaa-4b1c-9d00-000000000001";
        let known: HashSet<String> = [id.to_string()].into_iter().collect();
        let argvs = vec![vec!["copilot".to_string(), format!("--resume={id}")]];
        assert_eq!(live_ids_from(&argvs, &known), vec![id.to_string()]);

        let partial = vec![vec!["copilot".to_string(), "--resume=7c1d2e01-aaaa".to_string()]];
        assert!(live_ids_from(&partial, &known).is_empty());
        assert!(live_ids_from(&[], &known).is_empty());
    }

    #[test]
    fn workspace_only_session_has_zero_messages() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sdir = dir.path().join(".copilot/session-state/7c1d2e08-8888-4b1c-9d00-000000000008");
        std::fs::create_dir_all(&sdir).expect("mkdir");
        std::fs::write(sdir.join("workspace.yaml"), "cwd: /Users/test/empty\ncreated_at: 2026-11-01T10:00:00.000Z\n").expect("write");
        let sessions = CopilotAdapter::new(dir.path()).scan().expect("scan");
        assert_eq!(sessions[0].message_count, Some(0));
        assert_eq!(sessions[0].project, Some(PathBuf::from("/Users/test/empty")));
        assert_eq!(sessions[0].title, None);
    }

    #[test]
    #[ignore]
    fn live_scan() {
        let home = dirs::home_dir().expect("home dir");
        let started = Instant::now();
        let sessions = CopilotAdapter::new(&home).scan().expect("scan");
        let elapsed = started.elapsed();
        let total: u64 = sessions.iter().map(|s| s.bytes).sum();
        println!(
            "copilot: {} sessions, {} bytes, scan {:?}",
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
