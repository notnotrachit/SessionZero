//! Kiro CLI adapter. Format spec: `docs/formats/kiro_cli.md`.
//!
//! Two session stores under `~/.kiro/sessions/`:
//! - `cli/<uuid>.json` plus `cli/<uuid>.jsonl`: Kiro CLI sessions. Side files are
//!   `<uuid>.lock`, `<uuid>.history`, `sess_<uuid>.history`, `cli_<uuid>_*.history` and the
//!   `<uuid>/` directory.
//! - `<wshash>/sess_<uuid>/`: agent-mode sessions (`session.json`, `messages.jsonl`,
//!   `sub-executions/`). Each one is also registered in `session-index/<wshash>.jsonl`.
//!
//! Live rule (`live_session_ids`):
//! - CLI session: its `.lock` names a pid that is running, and that process started no later
//!   than the lock's `started_at` plus 120 s. A process that started after the lock was written
//!   holds a reused pid, so the lock is stale.
//! - Agent-mode session: `status == "in_progress"`, or `lastModifiedAt` within the last 5 minutes.
//!
//! Lock files of dead sessions are deletion artifacts. Lock files of live sessions are not.
//! The session index is a shared file, so it is listed as a `JsonlLines` artifact. Trash removes
//! only that session's lines.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::{DateTime, Duration, Utc};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sysinfo::{ProcessesToUpdate, System};

use crate::adapter::Adapter;
use crate::cache::ParseCache;
use crate::model::{AgentKind, Artifact, PreviewMsg, Session};
use crate::util::truncate;

const TITLE_MAX: usize = 120;
const TEXT_MAX: usize = 2000;
/// Allowed gap between a process start and the lock's `started_at`.
const LOCK_SLACK_SECS: i64 = 120;

pub struct KiroCliAdapter {
    home: PathBuf,
}

impl KiroCliAdapter {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }

    fn scan_all(&self, table: &OnceLock<ProcTable>) -> Vec<Session> {
        let cache = ParseCache::for_home(&self.home, "kiro-cli");
        let mut out = scan_cli_store(&self.home, table, &cache);
        out.extend(scan_agent_store(&self.home, &cache));
        if let Err(err) = cache.save() {
            eprintln!("sz: kiro-cli parse cache not saved: {err:#}");
        }
        out
    }
}

impl Adapter for KiroCliAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::KiroCli
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.home.join(".kiro/sessions")]
    }

    fn scan(&self) -> anyhow::Result<Vec<Session>> {
        let table = OnceLock::new();
        let mut sessions: Vec<Session> = self
            .scan_all(&table)
            .into_iter()
            .map(|mut session| {
                session.recompute_bytes();
                session
            })
            .collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(sessions)
    }

    fn preview(&self, s: &Session, max: usize) -> anyhow::Result<Vec<PreviewMsg>> {
        let mut out = Vec::new();
        if max == 0 {
            return Ok(out);
        }
        let agent_dir = s.artifacts.iter().find_map(|a| match a {
            Artifact::Dir(p) if is_agent_dir(p) => Some(p.clone()),
            _ => None,
        });
        let (path, agent) = match agent_dir {
            Some(dir) => (dir.join("messages.jsonl"), true),
            None => (
                self.home
                    .join(".kiro/sessions/cli")
                    .join(format!("{}.jsonl", s.id)),
                false,
            ),
        };
        if !path.is_file() {
            return Ok(out);
        }
        for_each_line(&path, |line| {
            if agent {
                preview_agent_line(line, &mut out);
            } else {
                preview_cli_line(line, &mut out);
            }
            out.len() < max
        })?;
        Ok(out)
    }

    fn process_names(&self) -> &'static [&'static str] {
        &["kiro-cli", "kiro", "Kiro"]
    }

    /// Ids of sessions open right now in either store. Reads only lock files and session
    /// metadata, never transcripts. The rule is in the module docs.
    fn live_session_ids(&self) -> Vec<String> {
        let now = Utc::now();
        let table = OnceLock::new();
        let mut ids = live_cli_ids(&self.home, &table);
        ids.extend(live_agent_ids(&self.home, now));
        ids.sort();
        ids.dedup();
        ids
    }
}


// ---------------------------------------------------------------------------
// Process table and locks
// ---------------------------------------------------------------------------

/// Pid to process start time (epoch seconds). Built at most once per scan.
struct ProcTable(HashMap<u32, i64>);

impl ProcTable {
    fn read() -> Self {
        let mut sys = System::new();
        sys.refresh_processes(ProcessesToUpdate::All, true);
        Self(
            sys.processes()
                .iter()
                .map(|(pid, p)| (pid.as_u32(), p.start_time() as i64))
                .collect(),
        )
    }
}

#[derive(Deserialize)]
struct Lock {
    pid: u32,
    started_at: String,
}

fn read_lock(path: &Path) -> Option<Lock> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// True when the lock's pid runs and started within the slack window of `started_at`.
/// An unparseable `started_at` keeps the lock, because the owner cannot be ruled out.
fn lock_is_live(table: &ProcTable, lock: &Lock) -> bool {
    let Some(&proc_start) = table.0.get(&lock.pid) else {
        return false;
    };
    match parse_ts(&lock.started_at) {
        Some(started) => proc_start <= started.timestamp() + LOCK_SLACK_SECS,
        None => true,
    }
}

// ---------------------------------------------------------------------------
// CLI store: ~/.kiro/sessions/cli/
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CliMeta {
    cwd: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
    title: Option<String>,
    /// Set only on subagent children.
    parent_session_id: Option<String>,
}

/// Only the top-level `kind` of a transcript line. The `data` body is skipped without parsing.
#[derive(Deserialize)]
struct Envelope {
    kind: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct CliSummary {
    messages: u64,
    first_prompt: Option<String>,
}

struct CliStore<'a> {
    dir: PathBuf,
    /// Session id to history side files.
    history: HashMap<String, Vec<PathBuf>>,
    cache: &'a ParseCache,
}

fn scan_cli_store(home: &Path, table: &OnceLock<ProcTable>, cache: &ParseCache) -> Vec<Session> {
    let dir = home.join(".kiro/sessions/cli");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut metas = Vec::new();
    let mut history: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            metas.push(path.clone());
        } else if let Some(owner) = history_owner(name) {
            history.entry(owner).or_default().push(path.clone());
        }
    }
    let store = CliStore {
        dir,
        history,
        cache,
    };
    metas
        .par_iter()
        .filter_map(|meta| store.session(meta, table))
        .collect()
}

/// Session id owned by a history file: `<uuid>.history`, `sess_<uuid>.history` or
/// `cli_<uuid>_<suffix>.history`.
fn history_owner(name: &str) -> Option<String> {
    let base = name.strip_suffix(".history")?;
    if let Some(rest) = base.strip_prefix("cli_") {
        return rest.split('_').next().map(str::to_string);
    }
    Some(base.strip_prefix("sess_").unwrap_or(base).to_string())
}

impl CliStore<'_> {
    fn session(&self, meta_path: &Path, table: &OnceLock<ProcTable>) -> Option<Session> {
        let id = meta_path.file_stem()?.to_str()?.to_string();
        let meta: CliMeta = serde_json::from_slice(&std::fs::read(meta_path).ok()?).ok()?;
        let transcript = self.dir.join(format!("{id}.jsonl"));
        let summary = summarize_cli(&transcript, self.cache);

        let fallback = summary.as_ref().and_then(|s| s.first_prompt.clone());
        let title = nonempty(meta.title)
            .or(fallback)
            .map(|t| truncate(&t, TITLE_MAX));

        let mut artifacts = vec![Artifact::File(meta_path.to_path_buf())];
        if transcript.is_file() {
            artifacts.push(Artifact::File(transcript));
        }

        // A lock of a live session is kept. A lock of a dead session is a deletable artifact.
        let lock_path = self.dir.join(format!("{id}.lock"));
        if let Some(lock) = read_lock(&lock_path) {
            if !lock_is_live(table.get_or_init(ProcTable::read), &lock) {
                artifacts.push(Artifact::File(lock_path));
            }
        }

        for path in self.history.get(&id).into_iter().flatten() {
            artifacts.push(Artifact::File(path.clone()));
        }
        let side_dir = self.dir.join(&id);
        if side_dir.is_dir() {
            artifacts.push(Artifact::Dir(side_dir));
        }

        let session = Session {
            parent_id: nonempty(meta.parent_session_id),
            agent: AgentKind::KiroCli,
            id,
            title,
            project: nonempty(meta.cwd).map(PathBuf::from),
            created: meta.created_at.as_deref().and_then(parse_ts),
            last_active: meta.updated_at.as_deref().and_then(parse_ts),
            message_count: summary.map(|s| s.messages),
            bytes: 0,
            artifacts,
        };
        Some(session)
    }
}

/// Counts Prompt and AssistantMessage records and takes the first prompt text. Cached per
/// transcript.
fn summarize_cli(path: &Path, cache: &ParseCache) -> Option<CliSummary> {
    if !path.is_file() {
        return None;
    }
    cache.get_or_parse(path, || parse_cli_transcript(path))
}

fn parse_cli_transcript(path: &Path) -> Option<CliSummary> {
    let mut sum = CliSummary {
        messages: 0,
        first_prompt: None,
    };
    let _ = for_each_raw_line(path, |line| {
        match cli_turn(line) {
            Turn::User => {
                sum.messages += 1;
                if sum.first_prompt.is_none() {
                    sum.first_prompt = prompt_text(line);
                }
            }
            Turn::Assistant => sum.messages += 1,
            Turn::Other => {}
        }
        true
    });
    Some(sum)
}

fn prompt_text(line: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(line).ok()?;
    nonempty(body_text(&v["data"]["content"]))
}

/// Role of a line in the CLI transcript. Read from the envelope's `kind`, without parsing the body.
fn cli_turn(line: &[u8]) -> Turn {
    if let Some(kind) = quoted_after(line, b"\"kind\":\"", Some(b"\"data\":")) {
        return match kind {
            b"Prompt" => Turn::User,
            b"AssistantMessage" => Turn::Assistant,
            _ => Turn::Other,
        };
    }
    match serde_json::from_slice::<Envelope>(line)
        .ok()
        .and_then(|e| e.kind)
        .as_deref()
    {
        Some("Prompt") => Turn::User,
        Some("AssistantMessage") => Turn::Assistant,
        _ => Turn::Other,
    }
}

/// Role of a line in the agent-mode transcript.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Turn {
    User,
    Assistant,
    Other,
}

/// The string value that starts right after `pat` in the first bytes of `line`. When `stop` is
/// given, `pat` must occur before the first `stop`. Returns None when the prefix does not settle
/// it, and the caller then parses the line.
fn quoted_after<'a>(line: &'a [u8], pat: &[u8], stop: Option<&[u8]>) -> Option<&'a [u8]> {
    let head = &line[..line.len().min(256)];
    let at = find(head, pat)?;
    if let Some(stop) = stop {
        let stop_at = find(head, stop)?;
        if stop_at < at {
            return None;
        }
    }
    let rest = &head[at + pat.len()..];
    let end = rest.iter().position(|&c| c == b'"')?;
    Some(&rest[..end])
}

/// Byte offset of the first `needle` in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn preview_cli_line(line: &str, out: &mut Vec<PreviewMsg>) {
    let Ok(env) = serde_json::from_str::<Envelope>(line) else {
        return;
    };
    let role = match env.kind.as_deref() {
        Some("Prompt") => "user",
        Some("AssistantMessage") => "assistant",
        _ => return,
    };
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let Some(text) = nonempty(body_text(&v["data"]["content"])) else {
        return;
    };
    let ts = if role == "user" {
        v["data"]["meta"]["timestamp"].as_str().and_then(parse_ts)
    } else {
        None
    };
    out.push(PreviewMsg {
        role: role.to_string(),
        text: truncate(&text, TEXT_MAX),
        ts,
    });
}

// ---------------------------------------------------------------------------
// Agent-mode store: ~/.kiro/sessions/<wshash>/sess_<uuid>/
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct AgentMeta {
    id: Option<String>,
    title: Option<String>,
    status: Option<String>,
    created_at: Option<String>,
    last_modified_at: Option<String>,
    workspace_paths: Option<Vec<String>>,
    root_paths: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct AgentEnv {
    payload: Option<AgentPayloadKind>,
}

#[derive(Deserialize)]
struct AgentPayloadKind {
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct IndexLine {
    #[serde(rename = "sessionPath")]
    session_path: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct AgentSummary {
    messages: u64,
    first_user: Option<String>,
}

/// Workspace-hash directories of the agent-mode store.
fn agent_workspaces(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(home.join(".kiro/sessions")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n != "cli" && !n.starts_with('.'))
        })
        .collect()
}

/// `sess_*` session directories inside one workspace directory.
fn agent_session_dirs(ws: &Path) -> Vec<PathBuf> {
    let Ok(dirs) = std::fs::read_dir(ws) else {
        return Vec::new();
    };
    dirs.flatten()
        .map(|e| e.path())
        .filter(|d| {
            d.is_dir()
                && d.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("sess_"))
        })
        .collect()
}

fn read_agent_meta(dir: &Path) -> Option<AgentMeta> {
    serde_json::from_slice(&std::fs::read(dir.join("session.json")).ok()?).ok()
}

/// Liveness of an agent-mode session. See the module docs for the rule.
fn agent_is_live(meta: &AgentMeta, now: DateTime<Utc>) -> bool {
    meta.status.as_deref() == Some("in_progress")
        || meta
            .last_modified_at
            .as_deref()
            .and_then(parse_ts)
            .is_some_and(|t| now.signed_duration_since(t) <= recent_window())
}

/// Ids with a live lock in the CLI store. Only `.lock` files are read.
fn live_cli_ids(home: &Path, table: &OnceLock<ProcTable>) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(home.join(".kiro/sessions/cli")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("lock") {
                return None;
            }
            let id = path.file_stem()?.to_str()?.to_string();
            let lock = read_lock(&path)?;
            lock_is_live(table.get_or_init(ProcTable::read), &lock).then_some(id)
        })
        .collect()
}

/// Ids of live agent-mode sessions. Reads only `session.json` files.
fn live_agent_ids(home: &Path, now: DateTime<Utc>) -> Vec<String> {
    agent_workspaces(home)
        .iter()
        .flat_map(|ws| agent_session_dirs(ws))
        .filter_map(|dir| {
            let meta = read_agent_meta(&dir)?;
            agent_is_live(&meta, now).then(|| agent_session_id(&meta, &dir))
        })
        .collect()
}

fn agent_session_id(meta: &AgentMeta, dir: &Path) -> String {
    nonempty(meta.id.clone()).unwrap_or_else(|| {
        dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string()
    })
}

fn scan_agent_store(home: &Path, cache: &ParseCache) -> Vec<Session> {
    let index_dir = home.join(".kiro/session-index");
    agent_workspaces(home)
        .par_iter()
        .flat_map_iter(|ws| {
            let Some(ws_name) = ws.file_name().and_then(|n| n.to_str()) else {
                return Vec::new();
            };
            let index_path = index_dir.join(format!("{ws_name}.jsonl"));
            let indexed = read_index(&index_path);
            agent_session_dirs(ws)
                .into_iter()
                .filter_map(|d| agent_session(ws_name, &d, &index_path, &indexed, cache))
                .collect::<Vec<Session>>()
        })
        .collect()
}

/// `sessionPath` values in one workspace's index file. The index is an append-only log, so any
/// path that appears is registered for the artifact check.
fn read_index(path: &Path) -> HashSet<String> {
    let mut set = HashSet::new();
    let _ = for_each_line(path, |line| {
        if let Ok(l) = serde_json::from_str::<IndexLine>(line) {
            if let Some(p) = l.session_path {
                set.insert(p);
            }
        }
        true
    });
    set
}

fn agent_session(
    ws: &str,
    dir: &Path,
    index_path: &Path,
    indexed: &HashSet<String>,
    cache: &ParseCache,
) -> Option<Session> {
    let meta = read_agent_meta(dir)?;
    let id = agent_session_id(&meta, dir);
    let rel = format!("{ws}/{}", dir.file_name()?.to_str()?);
    let transcript = summarize_agent(&dir.join("messages.jsonl"), cache);

    let fallback = transcript.as_ref().and_then(|t| t.first_user.clone());
    let title = nonempty(meta.title)
        .or(fallback)
        .map(|t| truncate(&t, TITLE_MAX));
    let project = first_nonempty(meta.workspace_paths.as_deref())
        .or_else(|| first_nonempty(meta.root_paths.as_deref()))
        .map(PathBuf::from);

    let mut artifacts = vec![Artifact::Dir(dir.to_path_buf())];
    if indexed.contains(&rel) {
        artifacts.push(Artifact::JsonlLines {
            file: index_path.to_path_buf(),
            field: "sessionPath".into(),
            value: rel,
        });
    }

    let session = Session {
        parent_id: None,
        agent: AgentKind::KiroCli,
        id,
        title,
        project,
        created: meta.created_at.as_deref().and_then(parse_ts),
        last_active: meta.last_modified_at.as_deref().and_then(parse_ts),
        message_count: transcript.as_ref().map(|t| t.messages),
        bytes: 0,
        artifacts,
    };
    Some(session)
}

fn summarize_agent(path: &Path, cache: &ParseCache) -> Option<AgentSummary> {
    if !path.is_file() {
        return None;
    }
    cache.get_or_parse(path, || parse_agent_transcript(path))
}

fn parse_agent_transcript(path: &Path) -> Option<AgentSummary> {
    let mut sum = AgentSummary {
        messages: 0,
        first_user: None,
    };
    let _ = for_each_raw_line(path, |line| {
        match agent_turn(line) {
            Turn::User => {
                sum.messages += 1;
                if sum.first_user.is_none() {
                    sum.first_user = agent_payload_text(line);
                }
            }
            Turn::Assistant => sum.messages += 1,
            Turn::Other => {}
        }
        true
    });
    Some(sum)
}

fn agent_payload_text(line: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(line).ok()?;
    nonempty(body_text(&v["payload"]["content"]))
}

/// Role of a line in an agent-mode transcript. Read from `payload.type` without parsing the body.
fn agent_turn(line: &[u8]) -> Turn {
    if let Some(kind) = quoted_after(line, b"\"payload\":{\"type\":\"", None) {
        return match kind {
            b"user" => Turn::User,
            b"assistant" => Turn::Assistant,
            _ => Turn::Other,
        };
    }
    match serde_json::from_slice::<AgentEnv>(line)
        .ok()
        .and_then(|e| e.payload)
        .and_then(|p| p.kind)
        .as_deref()
    {
        Some("user") => Turn::User,
        Some("assistant") => Turn::Assistant,
        _ => Turn::Other,
    }
}

fn preview_agent_line(line: &str, out: &mut Vec<PreviewMsg>) {
    let Ok(env) = serde_json::from_str::<AgentEnv>(line) else {
        return;
    };
    let role = match env.payload.and_then(|p| p.kind).as_deref() {
        Some("user") => "user",
        Some("assistant") => "assistant",
        _ => return,
    };
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let Some(text) = nonempty(body_text(&v["payload"]["content"])) else {
        return;
    };
    out.push(PreviewMsg {
        role: role.to_string(),
        text: truncate(&text, TEXT_MAX),
        ts: v["timestamp"].as_str().and_then(parse_ts),
    });
}

/// True for a `sess_*` directory, which marks an agent-mode session.
fn is_agent_dir(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("sess_"))
}

/// Agent-mode sessions are "recent" for this long after their last modification.
fn recent_window() -> Duration {
    Duration::seconds(300)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Calls `f` with each line, trimmed. Invalid UTF-8 is repaired lossily. `f` returns `false` to
/// stop early. Callers skip lines that fail to parse, so one malformed line does not end a file.
fn for_each_line(path: &Path, mut f: impl FnMut(&str) -> bool) -> std::io::Result<()> {
    for_each_raw_line(path, |line| f(&String::from_utf8_lossy(line)))
}

/// Calls `f` with each line as raw bytes, with surrounding ASCII whitespace trimmed. No UTF-8
/// check is made, so a line is only copied from the file. `f` returns `false` to stop early.
fn for_each_raw_line(path: &Path, mut f: impl FnMut(&[u8]) -> bool) -> std::io::Result<()> {
    let mut reader = BufReader::with_capacity(256 * 1024, File::open(path)?);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        if !f(buf.trim_ascii()) {
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

fn first_nonempty(list: Option<&[String]>) -> Option<String> {
    list?.iter().find(|s| !s.trim().is_empty()).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn fixture_home() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/kiro_cli")
    }

    fn expected() -> Vec<Value> {
        let text = std::fs::read_to_string(fixture_home().join("EXPECTED.json")).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        v["sessions"].as_array().unwrap().clone()
    }

    fn rel_paths(e: &Value) -> Vec<PathBuf> {
        ["artifacts_rel_to_home", "index_files_rel_to_home"]
            .iter()
            .flat_map(|k| e[*k].as_array().cloned().unwrap_or_default())
            .map(|v| fixture_home().join(v.as_str().unwrap()))
            .collect()
    }

    /// Every expected file lies under an artifact, and every artifact covers an expected file.
    fn assert_artifacts(s: &Session, expected: &[PathBuf]) {
        for e in expected {
            assert!(
                s.artifacts.iter().any(|a| e.starts_with(a.primary_path())),
                "{}: {} not covered by any artifact",
                s.id,
                e.display()
            );
        }
        for a in &s.artifacts {
            assert!(
                expected.iter().any(|e| e.starts_with(a.primary_path())),
                "{}: unexpected artifact {:?}",
                s.id,
                a
            );
        }
    }

    #[test]
    fn fixture_scan_matches_expected() {
        let adapter = KiroCliAdapter::new(&fixture_home());
        let sessions = adapter.scan().unwrap();
        let exp = expected();
        assert_eq!(sessions.len(), exp.len());
        for e in &exp {
            let id = e["id"].as_str().unwrap();
            let s = sessions
                .iter()
                .find(|s| s.id == id)
                .unwrap_or_else(|| panic!("missing session {id}"));
            assert_eq!(s.agent, AgentKind::KiroCli);
            // The fixture's `title_fallback` is the first prompt, used when the title is null.
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
            assert_artifacts(s, &rel_paths(e));
            let bytes: u64 = s.artifacts.iter().map(Artifact::own_bytes).sum();
            assert_eq!(s.bytes, bytes, "bytes of {id}");
        }
    }

    #[test]
    fn fixture_lock_dead_is_deletable_and_no_live_sessions() {
        let adapter = KiroCliAdapter::new(&fixture_home());
        let sessions = adapter.scan().unwrap();
        let s = sessions
            .iter()
            .find(|s| s.id == "0a1b2c3d-1111-4a2b-8c3d-000000000001")
            .unwrap();
        assert!(s.artifacts.iter().any(|a| matches!(a, Artifact::File(p) if p.to_string_lossy().ends_with(".lock"))));
        assert!(adapter.live_session_ids().is_empty());
    }

    #[test]
    fn fixture_preview_skips_tool_and_compaction_records() {
        let adapter = KiroCliAdapter::new(&fixture_home());
        let sessions = adapter.scan().unwrap();
        let cli = sessions
            .iter()
            .find(|s| s.id == "0a1b2c3d-1111-4a2b-8c3d-000000000001")
            .unwrap();
        let roles: Vec<String> = adapter
            .preview(cli, 10)
            .unwrap()
            .into_iter()
            .map(|m| m.role)
            .collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
        assert_eq!(adapter.preview(cli, 2).unwrap().len(), 2);

        let agent = sessions.iter().find(|s| s.id.starts_with("sess_")).unwrap();
        let msgs = adapter.preview(agent, 10).unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0].text, "Uploader fails on 503. Add retries with backoff.");
    }

    #[test]
    fn live_lock_of_running_process_is_live_and_not_deletable() {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join(".kiro/sessions/cli");
        std::fs::create_dir_all(&cli).unwrap();
        let id = "11111111-aaaa-4bbb-8ccc-000000000001";
        std::fs::write(cli.join(format!("{id}.json")), r#"{"session_id":"x","cwd":"/tmp/p"}"#).unwrap();
        let lock = format!(
            r#"{{"pid":{},"started_at":"{}"}}"#,
            std::process::id(),
            Utc::now().to_rfc3339()
        );
        std::fs::write(cli.join(format!("{id}.lock")), lock).unwrap();

        let adapter = KiroCliAdapter::new(dir.path());
        assert_eq!(adapter.live_session_ids(), vec![id.to_string()]);
        let s = &adapter.scan().unwrap()[0];
        assert!(!s.artifacts.iter().any(|a| matches!(a, Artifact::File(p) if p.to_string_lossy().ends_with(".lock"))));
    }

    #[test]
    fn reused_pid_lock_is_not_live() {
        // Our own process started long after 2000-01-01, so a lock claiming that start is stale.
        let table = ProcTable::read();
        let lock = Lock {
            pid: std::process::id(),
            started_at: "2000-01-01T00:00:00Z".into(),
        };
        assert!(!lock_is_live(&table, &lock));
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join(".kiro/sessions/cli");
        std::fs::create_dir_all(&cli).unwrap();
        let id = "22222222-aaaa-4bbb-8ccc-000000000002";
        std::fs::write(cli.join(format!("{id}.json")), r#"{"session_id":"x","title":null}"#).unwrap();
        let transcript = concat!(
            "{not json\n",
            r#"{"version":"v1","kind":"Prompt","data":{"message_id":"m","content":[{"kind":"text","data":"hello there"}],"meta":{}}}"#,
            "\n\n",
            r#"{"version":"v1","kind":"AssistantMessage","data":{"message_id":"n","content":[{"kind":"text","data":"hi"}]}}"#,
            "\n",
        );
        std::fs::write(cli.join(format!("{id}.jsonl")), transcript).unwrap();

        let sessions = KiroCliAdapter::new(dir.path()).scan().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].message_count, Some(2));
        assert_eq!(sessions[0].title.as_deref(), Some("hello there"));
    }

    #[test]
    fn subagent_child_records_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join(".kiro/sessions/cli");
        std::fs::create_dir_all(&cli).unwrap();
        let child = "33333333-aaaa-4bbb-8ccc-000000000003";
        let meta = r#"{"session_id":"33333333-aaaa-4bbb-8ccc-000000000003","cwd":"/","parent_session_id":"33333333-aaaa-4bbb-8ccc-000000000000"}"#;
        std::fs::write(cli.join(format!("{child}.json")), meta).unwrap();
        let sessions = KiroCliAdapter::new(dir.path()).scan().unwrap();
        assert_eq!(
            sessions[0].parent_id.as_deref(),
            Some("33333333-aaaa-4bbb-8ccc-000000000000")
        );
    }

    #[test]
    fn history_side_file_names_map_to_their_session() {
        assert_eq!(history_owner("abc.history").as_deref(), Some("abc"));
        assert_eq!(history_owner("sess_abc.history").as_deref(), Some("abc"));
        assert_eq!(history_owner("cli_abc_x1.history").as_deref(), Some("abc"));
        assert_eq!(history_owner("abc.json"), None);
    }

    /// Read-only scan of the real home. Run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_scan() {
        let home = dirs::home_dir().expect("home dir");
        let adapter = KiroCliAdapter::new(&home);
        let t = Instant::now();
        let sessions = adapter.scan().unwrap();
        let elapsed = t.elapsed();
        let total: u64 = sessions.iter().map(|s| s.bytes).sum();
        println!(
            "kiro_cli: count={} total_bytes={} scan_time={:?}",
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
