//! Gemini CLI (`~/.gemini/tmp`) and Google Antigravity (`~/.gemini/antigravity-cli` for the
//! CLI, `~/.gemini/antigravity` for the GUI). Layout notes: `docs/formats/gemini.md`.
//!
//! Session identity:
//! - Gemini CLI: `sessionId` inside each `tmp/<key>/chats/session-*.json`. Chat files that share
//!   an id are merged into one session. Orphan `logs.json` entries become sessions too.
//! - Antigravity CLI: one session per `antigravity-cli/conversations/<id>.db`.
//! - Antigravity GUI: one session per `antigravity/conversations/<id>.pb|.db`. The GUI store is
//!   never attached to a CLI session. If a GUI id is also a CLI store id, the GUI session id is
//!   prefixed with `gui:` so the two keys stay distinct.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Utc};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::adapter::Adapter;
use crate::model::{AgentKind, Artifact, DbDelete, PreviewMsg, Session};
use crate::util::{mtime, truncate};

const TITLE_MAX: usize = 120;
const TEXT_MAX: usize = 2000;
const CLI_DIR: &str = "antigravity-cli";
const GUI_DIR: &str = "antigravity";
const SUMMARIES: &str = "conversation_summaries.db";
const SUMMARY_TABLE: &str = "conversation_summaries";

pub struct GeminiAdapter {
    home: PathBuf,
}

impl GeminiAdapter {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }

    fn root(&self) -> PathBuf {
        self.home.join(".gemini")
    }

}

impl Adapter for GeminiAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Gemini
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root()]
    }

    fn scan(&self) -> anyhow::Result<Vec<Session>> {
        let root = self.root();
        let ctx = Context::load(&root);

        let mut sessions = gemini_cli_sessions(&root, &ctx);
        let cli = antigravity_cli_sessions(&root, &ctx);
        let cli_ids: HashSet<String> = cli.iter().map(|s| s.id.clone()).collect();
        let gui = antigravity_gui_sessions(&root, &ctx, &cli_ids);
        sessions.extend(cli);
        sessions.extend(gui);

        sessions.sort_by(|a, b| {
            b.last_active
                .cmp(&a.last_active)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(sessions)
    }

    fn preview(&self, s: &Session, max: usize) -> anyhow::Result<Vec<PreviewMsg>> {
        let mut out = Vec::new();
        for art in &s.artifacts {
            let Artifact::File(path) = art else { continue };
            let in_chats = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                == Some("chats");
            if !in_chats {
                continue;
            }
            for (role, text, ts) in chat_messages(path) {
                if out.len() >= max {
                    return Ok(out);
                }
                out.push(PreviewMsg { role, text, ts });
            }
        }
        Ok(out)
    }

    fn process_names(&self) -> &'static [&'static str] {
        &["gemini", "Antigravity", "antigravity"]
    }

    /// Antigravity CLI conversation ids whose presence lock is held by another process.
    ///
    /// The lock files exist for idle conversations too, so a lock alone does not mean running.
    /// A session counts as live only when an exclusive `flock` on its lock file is contended.
    fn live_session_ids(&self) -> Vec<String> {
        let presence = self.root().join(CLI_DIR).join("presence");
        sorted_entries(&presence)
            .into_iter()
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("lock"))
            .filter(|p| lock_held(p))
            .filter_map(|p| stem(&p))
            .collect()
    }
}

/// True when another process holds an exclusive lock on the file. Opened read-only.
fn lock_held(path: &Path) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(fs::TryLockError::WouldBlock) => true,
        Err(fs::TryLockError::Error(_)) => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Shared lookup data, read once per scan.
// ---------------------------------------------------------------------------------------------

struct Context {
    /// `projects.json`: absolute path -> directory alias.
    aliases: Vec<(PathBuf, String)>,
    /// sha256(hex) of every known project path -> path.
    hashes: HashMap<String, PathBuf>,
    cli: HashMap<String, SummaryRow>,
    gui: HashMap<String, SummaryRow>,
    cli_db: PathBuf,
    gui_db: PathBuf,
}

struct SummaryRow {
    title: String,
    preview: String,
    step_count: Option<u64>,
    last_modified: Option<DateTime<Utc>>,
    workspace: Option<PathBuf>,
    /// Approximate bytes of the row: raw_summary plus the text columns.
    bytes: u64,
    /// `parent_conversation_id`, set for conversations nested under another one.
    parent: Option<String>,
}

impl Context {
    fn load(root: &Path) -> Self {
        let aliases = read_projects(root);
        let cli_db = root.join(CLI_DIR).join(SUMMARIES);
        let gui_db = root.join(GUI_DIR).join(SUMMARIES);
        let cli = load_summaries(&cli_db);
        let gui = load_summaries(&gui_db);

        let candidates = aliases
            .iter()
            .map(|(p, _)| p.clone())
            .chain(project_roots(root))
            .chain(
                cli.values()
                    .chain(gui.values())
                    .filter_map(|r| r.workspace.clone()),
            );
        let mut hashes = HashMap::new();
        for path in candidates {
            let digest = sha256_hex(path.to_string_lossy().as_bytes());
            hashes.entry(digest).or_insert(path);
        }

        Self {
            aliases,
            hashes,
            cli,
            gui,
            cli_db,
            gui_db,
        }
    }

    /// Project for a `tmp/<dir>` directory. The directory is either an alias from
    /// `projects.json` or a project hash. The chat body's `projectHash` is tried too.
    fn project_for(&self, dir_name: &str, body_hash: Option<&str>) -> Option<PathBuf> {
        if let Some((path, _)) = self.aliases.iter().find(|(_, alias)| alias == dir_name) {
            return Some(path.clone());
        }
        body_hash
            .and_then(|h| self.hashes.get(h))
            .or_else(|| self.hashes.get(dir_name))
            .cloned()
    }
}

fn read_projects(root: &Path) -> Vec<(PathBuf, String)> {
    let Some(body) = read_json(&root.join("projects.json")) else {
        return Vec::new();
    };
    body.get("projects")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(path, alias)| Some((PathBuf::from(path), alias.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

/// Absolute project paths recorded in `history/<name>/.project_root`. Gemini CLI writes these
/// next to its per-project history, and they name projects whose hash dirs have no alias.
fn project_roots(root: &Path) -> Vec<PathBuf> {
    sorted_entries(&root.join("history"))
        .into_iter()
        .filter_map(|dir| fs::read_to_string(dir.join(".project_root")).ok())
        .map(|text| PathBuf::from(text.trim()))
        .filter(|p| p.is_absolute())
        .collect()
}

fn load_summaries(db: &Path) -> HashMap<String, SummaryRow> {
    let mut rows = HashMap::new();
    if !db.is_file() {
        return rows;
    }
    let Some(conn) = open_ro(db) else {
        return rows;
    };
    let sql = format!(
        "SELECT conversation_id, title, preview, step_count, last_modified_time, \
         workspace_uris, length(title) + length(preview) + length(workspace_uris) \
         + COALESCE(length(raw_summary), 0), parent_conversation_id FROM {SUMMARY_TABLE}"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return rows;
    };
    let mapped = stmt.query_map([], |r| {
        let id = text_at(r, 0)?.unwrap_or_default();
        let row = SummaryRow {
            title: text_at(r, 1)?.unwrap_or_default(),
            preview: text_at(r, 2)?.unwrap_or_default(),
            step_count: int_at(r, 3)?.and_then(|n| u64::try_from(n).ok()),
            last_modified: text_at(r, 4)?.as_deref().and_then(parse_summary_time),
            workspace: text_at(r, 5)?.as_deref().and_then(uri_to_path),
            bytes: int_at(r, 6)?.and_then(|n| u64::try_from(n).ok()).unwrap_or(0),
            parent: text_at(r, 7)?.filter(|p| !p.trim().is_empty()),
        };
        Ok((id, row))
    });
    if let Ok(iter) = mapped {
        rows.extend(iter.flatten());
    }
    rows
}

// ---------------------------------------------------------------------------------------------
// Gemini CLI: tmp/<key>/chats/session-*.json and tmp/<key>/logs.json
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct LogGroup {
    count: u64,
    first_user: Option<String>,
    first_ts: Option<DateTime<Utc>>,
    last_ts: Option<DateTime<Utc>>,
}

#[derive(Default)]
struct ChatAcc {
    files: Vec<PathBuf>,
    logs: Vec<PathBuf>,
    project: Option<PathBuf>,
    title: Option<String>,
    created: Option<DateTime<Utc>>,
    last_active: Option<DateTime<Utc>>,
    message_count: u64,
}

#[derive(Default)]
struct OrphanAcc {
    logs: Vec<PathBuf>,
    project: Option<PathBuf>,
    title: Option<String>,
    created: Option<DateTime<Utc>>,
    last_active: Option<DateTime<Utc>>,
    count: u64,
}

fn gemini_cli_sessions(root: &Path, ctx: &Context) -> Vec<Session> {
    let tmp = root.join("tmp");
    let dirs: Vec<PathBuf> = sorted_entries(&tmp)
        .into_iter()
        .filter(|p| p.is_dir())
        .collect();

    // logs.json per directory: (path, dir name, sessionId -> group).
    let mut logs: Vec<(PathBuf, String, HashMap<String, LogGroup>)> = Vec::new();
    for dir in &dirs {
        let logs_path = dir.join("logs.json");
        if logs_path.is_file() {
            let groups = read_logs(&logs_path);
            logs.push((logs_path, file_name(dir), groups));
        }
    }

    // Chat files, in name order (the name encodes the start time, so this is chronological).
    let mut chats: BTreeMap<String, ChatAcc> = BTreeMap::new();
    for dir in &dirs {
        let dir_name = file_name(dir);
        for file in sorted_entries(&dir.join("chats")) {
            if !is_chat_file(&file) {
                continue;
            }
            let Some(body) = read_json(&file) else {
                continue;
            };
            let Some(id) = body.get("sessionId").and_then(Value::as_str) else {
                continue;
            };
            let body_hash = body.get("projectHash").and_then(Value::as_str);
            let msgs: &[Value] = body
                .get("messages")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);

            let acc = chats.entry(id.to_owned()).or_default();
            acc.files.push(file.clone());
            if acc.project.is_none() {
                acc.project = ctx.project_for(&dir_name, body_hash);
            }
            acc.message_count = acc.message_count.max(msgs.len() as u64);
            if acc.title.is_none() {
                acc.title = first_user_title(msgs);
            }
            acc.created = earlier(
                acc.created,
                body.get("startTime").and_then(Value::as_str).and_then(parse_iso),
            );
            let msg_ts_max = msgs
                .iter()
                .filter_map(|m| m.get("timestamp").and_then(Value::as_str))
                .filter_map(parse_iso)
                .max();
            let updated = body
                .get("lastUpdated")
                .and_then(Value::as_str)
                .and_then(parse_iso);
            acc.last_active = later(acc.last_active, later(updated, msg_ts_max));
        }
    }

    // Attach logs.json files to the chat sessions they hold entries for. Entries whose session
    // has no chat file become orphan sessions.
    let mut orphans: BTreeMap<String, OrphanAcc> = BTreeMap::new();
    for (logs_path, dir_name, groups) in &logs {
        for (id, group) in groups {
            if let Some(acc) = chats.get_mut(id) {
                acc.logs.push(logs_path.clone());
                continue;
            }
            let orphan = orphans.entry(id.clone()).or_default();
            orphan.logs.push(logs_path.clone());
            if orphan.project.is_none() {
                orphan.project = ctx.project_for(dir_name, None);
            }
            orphan.title = orphan.title.take().or_else(|| group.first_user.clone());
            orphan.created = earlier(orphan.created, group.first_ts);
            orphan.last_active = later(orphan.last_active, group.last_ts);
            orphan.count += group.count;
        }
    }

    let mut sessions = Vec::new();
    for (id, acc) in chats {
        let mut artifacts: Vec<Artifact> = acc.files.iter().cloned().map(Artifact::File).collect();
        artifacts.extend(acc.logs.into_iter().map(|file| Artifact::JsonArrayEntry {
            file,
            array_path: String::new(),
            field: "sessionId".into(),
            value: id.clone(),
        }));
        let last_active = acc.last_active.or_else(|| acc.files.first().and_then(|f| mtime(f)));
        sessions.push(make_session(
            AgentKind::Gemini,
            id,
            acc.title,
            acc.project,
            acc.created,
            last_active,
            Some(acc.message_count),
            artifacts,
        ));
    }
    for (id, orphan) in orphans {
        let artifacts: Vec<Artifact> = orphan
            .logs
            .into_iter()
            .map(|file| Artifact::JsonArrayEntry {
                file,
                array_path: String::new(),
                field: "sessionId".into(),
                value: id.clone(),
            })
            .collect();
        sessions.push(make_session(
            AgentKind::Gemini,
            id,
            orphan.title,
            orphan.project,
            orphan.created,
            orphan.last_active,
            Some(orphan.count),
            artifacts,
        ));
    }
    sessions
}

fn read_logs(path: &Path) -> HashMap<String, LogGroup> {
    let mut groups: HashMap<String, LogGroup> = HashMap::new();
    let Some(Value::Array(entries)) = read_json(path) else {
        return groups;
    };
    for entry in &entries {
        let Some(id) = entry.get("sessionId").and_then(Value::as_str) else {
            continue;
        };
        let group = groups.entry(id.to_owned()).or_default();
        group.count += 1;
        let ts = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso);
        group.first_ts = earlier(group.first_ts, ts);
        group.last_ts = later(group.last_ts, ts);
        if group.first_user.is_none() && entry.get("type").and_then(Value::as_str) == Some("user")
        {
            group.first_user = entry.get("message").and_then(first_line_of_content);
        }
    }
    groups
}

fn is_chat_file(path: &Path) -> bool {
    let name = file_name(path);
    name.starts_with("session-") && name.ends_with(".json")
}

fn first_user_title(msgs: &[Value]) -> Option<String> {
    msgs.iter()
        .find(|m| m.get("type").and_then(Value::as_str) == Some("user"))
        .and_then(|m| m.get("content"))
        .and_then(first_line_of_content)
}

fn first_line_of_content(v: &Value) -> Option<String> {
    first_line(&content_text(v))
}

/// Text of a message `content`, which is a string in the observed files. Arrays of parts are
/// joined by their `text` fields as a fallback.
fn content_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str).or_else(|| p.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn chat_messages(path: &Path) -> Vec<(String, String, Option<DateTime<Utc>>)> {
    let Some(body) = read_json(path) else {
        return Vec::new();
    };
    let Some(msgs) = body.get("messages").and_then(Value::as_array) else {
        return Vec::new();
    };
    msgs.iter()
        .filter_map(|m| {
            let role = match m.get("type").and_then(Value::as_str)? {
                "user" => "user",
                "gemini" => "assistant",
                _ => return None,
            };
            let text = content_text(m.get("content")?);
            if text.trim().is_empty() {
                return None;
            }
            let ts = m.get("timestamp").and_then(Value::as_str).and_then(parse_iso);
            Some((role.to_owned(), truncate(&text, TEXT_MAX), ts))
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Antigravity CLI: antigravity-cli/conversations/<id>.db
// ---------------------------------------------------------------------------------------------

fn antigravity_cli_sessions(root: &Path, ctx: &Context) -> Vec<Session> {
    let base = root.join(CLI_DIR);
    let mut out = Vec::new();
    for db in sorted_entries(&base.join("conversations")) {
        if !db.is_file() || db.extension().and_then(|e| e.to_str()) != Some("db") {
            continue;
        }
        let Some(id) = stem(&db) else {
            continue;
        };
        let mut artifacts = vec![Artifact::File(db.clone())];
        artifacts.extend(sidecars(&db));
        let lock = base.join("presence").join(format!("{id}.lock"));
        if lock.is_file() {
            artifacts.push(Artifact::File(lock));
        }
        let brain = base.join("brain").join(&id);
        if brain.is_dir() {
            artifacts.push(Artifact::Dir(brain));
        }
        let row = ctx.cli.get(&id);
        if let Some(row) = row {
            artifacts.push(summary_artifact(&ctx.cli_db, &id, row));
        }
        out.push(session_from_row(id, row, &db, &db, artifacts));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Antigravity GUI: antigravity/conversations/<id>.pb (and .db)
// ---------------------------------------------------------------------------------------------

fn antigravity_gui_sessions(root: &Path, ctx: &Context, cli_ids: &HashSet<String>) -> Vec<Session> {
    let base = root.join(GUI_DIR);
    let mut by_id: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for file in sorted_entries(&base.join("conversations")) {
        let is_store = matches!(
            file.extension().and_then(|e| e.to_str()),
            Some("pb") | Some("db")
        );
        if !file.is_file() || !is_store {
            continue;
        }
        if let Some(id) = stem(&file) {
            by_id.entry(id).or_default().push(file);
        }
    }

    let mut out = Vec::new();
    for (id, files) in by_id {
        let session_id = if cli_ids.contains(&id) {
            format!("gui:{id}")
        } else {
            id.clone()
        };
        let mut artifacts = Vec::new();
        for file in &files {
            artifacts.push(Artifact::File(file.clone()));
            artifacts.extend(sidecars(file));
        }
        let brain = base.join("brain").join(&id);
        if brain.is_dir() {
            artifacts.push(Artifact::Dir(brain));
        }
        let gui_row = ctx.gui.get(&id);
        let cli_row = ctx.cli.get(&id);
        if let Some(row) = gui_row {
            artifacts.push(summary_artifact(&ctx.gui_db, &id, row));
        }
        if let Some(row) = cli_row {
            artifacts.push(summary_artifact(&ctx.cli_db, &id, row));
        }
        let row = gui_row.or(cli_row);
        out.push(session_from_row(session_id, row, &files[0], &files[0], artifacts));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Helpers shared by the Antigravity sessions.
// ---------------------------------------------------------------------------------------------

fn summary_artifact(db: &Path, id: &str, row: &SummaryRow) -> Artifact {
    Artifact::DbRows {
        db: db.to_path_buf(),
        deletes: vec![DbDelete {
            table: SUMMARY_TABLE.into(),
            where_col: "conversation_id".into(),
            value: id.to_owned(),
        }],
        bytes: row.bytes,
    }
}

/// Builds an Antigravity session from its summary row, falling back to the store file for the
/// timestamp and to a step count query when no row exists.
fn session_from_row(
    id: String,
    row: Option<&SummaryRow>,
    store: &Path,
    time_fallback: &Path,
    artifacts: Vec<Artifact>,
) -> Session {
    let title = row.and_then(|r| {
        first_line(&r.title).or_else(|| first_line(&r.preview))
    });
    let project = row.and_then(|r| r.workspace.clone());
    let last_active = row
        .and_then(|r| r.last_modified)
        .or_else(|| mtime(time_fallback));
    let message_count = match row {
        Some(r) => r.step_count,
        None => count_steps(store),
    };
    let parent_id = row.and_then(|r| r.parent.clone());
    let mut session = make_session(
        AgentKind::Gemini,
        id,
        title,
        project,
        None,
        last_active,
        message_count,
        artifacts,
    );
    session.parent_id = parent_id;
    session
}

/// `-wal` and `-shm` files next to a SQLite store, when present.
fn sidecars(db: &Path) -> Vec<Artifact> {
    ["-wal", "-shm"]
        .iter()
        .map(|suffix| append_suffix(db, suffix))
        .filter(|p| p.is_file())
        .map(Artifact::File)
        .collect()
}

fn count_steps(db: &Path) -> Option<u64> {
    let conn = open_ro(db)?;
    let n: i64 = conn
        .query_row("SELECT count(*) FROM steps", [], |r| r.get(0))
        .ok()?;
    u64::try_from(n).ok()
}

#[allow(clippy::too_many_arguments)]
fn make_session(
    agent: AgentKind,
    id: String,
    title: Option<String>,
    project: Option<PathBuf>,
    created: Option<DateTime<Utc>>,
    last_active: Option<DateTime<Utc>>,
    message_count: Option<u64>,
    artifacts: Vec<Artifact>,
) -> Session {
    let mut s = Session {
        parent_id: None,
        agent,
        id,
        title,
        project,
        created,
        last_active,
        message_count,
        bytes: 0,
        artifacts,
    };
    s.recompute_bytes();
    s
}

fn earlier(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

fn later(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| truncate(l, TITLE_MAX))
}

fn parse_iso(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Summary times look like `2026-02-05 10:05:00+00:00`. Year 0001 is a sentinel for "unset".
fn parse_summary_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%:z")
        .ok()
        .map(|d| d.with_timezone(&Utc))
        .filter(|d| d.year() > 1)
}

/// `file:///abs/path` (percent-encoded) or a bare absolute path. The first token is used.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let first = uri.split_whitespace().next()?;
    let raw = first.strip_prefix("file://").unwrap_or(first);
    raw.starts_with('/')
        .then(|| PathBuf::from(percent_decode(raw)))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encodes a path for a `file:` URI. Only unreserved characters and `/` stay literal.
fn encode_uri_path(path: &Path) -> String {
    let mut out = String::new();
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Opens a SQLite file read-only. Tries `mode=ro` first and falls back to `immutable=1` when
/// the first open cannot read the file (for example a WAL database with no `-shm` file).
fn open_ro(path: &Path) -> Option<Connection> {
    let encoded = encode_uri_path(path);
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    for mode in ["mode=ro", "immutable=1"] {
        let Ok(conn) = Connection::open_with_flags(format!("file:{encoded}?{mode}"), flags) else {
            continue;
        };
        if conn
            .query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
            .is_ok()
        {
            return Some(conn);
        }
    }
    None
}

fn text_at(r: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<Option<String>> {
    Ok(match r.get_ref(i)? {
        ValueRef::Text(b) => Some(String::from_utf8_lossy(b).into_owned()),
        _ => None,
    })
}

fn int_at(r: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<Option<i64>> {
    Ok(match r.get_ref(i)? {
        ValueRef::Integer(n) => Some(n),
        _ => None,
    })
}

fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    entries.sort();
    entries
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn stem(p: &Path) -> Option<String> {
    p.file_stem()?.to_str().map(str::to_owned)
}

fn append_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Lowercase hex SHA-256. Gemini CLI names project directories with the hash of the absolute
/// project path. Implemented here so the crate needs no hashing dependency.
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(v);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn fixture_home() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/gemini")
    }

    fn expected() -> Value {
        read_json(&fixture_home().join("EXPECTED.json")).expect("EXPECTED.json")
    }

    fn expected_for<'a>(exp: &'a Value, id: &str) -> &'a Value {
        exp["sessions"]
            .as_array()
            .expect("sessions")
            .iter()
            .find(|s| s["id"] == id)
            .expect("expected session")
    }

    fn iso(s: &str) -> DateTime<Utc> {
        parse_iso(s).expect("iso")
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"/Users/fixture/work/alpha-app"),
            "047bb32c37fbea180267830113bb9775923f947dd5f26be13d0c34e325fb6020"
        );
    }

    #[test]
    fn scan_matches_expected_fixture() {
        let sessions = GeminiAdapter::new(&fixture_home()).scan().expect("scan");
        let exp = expected();
        assert_eq!(sessions.len(), 3, "{sessions:#?}");

        for s in &sessions {
            let e = expected_for(&exp, &s.id);
            assert_eq!(s.title.as_deref(), e["title"].as_str(), "title {}", s.id);
            assert_eq!(
                s.project.as_ref().map(|p| p.to_string_lossy().into_owned()),
                e["project"].as_str().map(str::to_owned),
                "project {}",
                s.id
            );
            assert_eq!(s.message_count, e["message_count"].as_u64(), "count {}", s.id);
            assert_eq!(s.last_active, e["last_active"].as_str().map(iso), "last {}", s.id);
            assert_eq!(s.created, e["created"].as_str().map(iso), "created {}", s.id);
            let file_bytes: u64 = s
                .artifacts
                .iter()
                .filter(|a| !a.is_shared())
                .map(Artifact::own_bytes)
                .sum();
            assert_eq!(Some(file_bytes), e["dir_bytes"].as_u64(), "bytes {}", s.id);
            assert_eq!(s.bytes, s.artifacts.iter().map(Artifact::own_bytes).sum::<u64>());
        }
    }

    #[test]
    fn chat_session_artifacts_include_logs_entries() {
        let sessions = GeminiAdapter::new(&fixture_home()).scan().expect("scan");
        let alpha = sessions
            .iter()
            .find(|s| s.id == "a1b2c3d4-1111-4aaa-8bbb-000000000001")
            .expect("alpha");
        assert!(alpha
            .artifacts
            .iter()
            .any(|a| matches!(a, Artifact::File(p) if p.ends_with("session-2026-02-01T10-00-a1b2c3d4.json"))));
        assert!(alpha.artifacts.iter().any(|a| matches!(
            a,
            Artifact::JsonArrayEntry { value, field, .. } if value == &alpha.id && field == "sessionId"
        )));
    }

    #[test]
    fn antigravity_session_has_db_rows_and_no_gui_store() {
        let sessions = GeminiAdapter::new(&fixture_home()).scan().expect("scan");
        let cli = sessions
            .iter()
            .find(|s| s.id == "5a1b2c3d-4444-4eee-8fff-000000000004")
            .expect("cli session");
        assert!(cli.artifacts.iter().any(|a| matches!(
            a,
            Artifact::DbRows { deletes, .. } if deletes[0].value == cli.id && deletes[0].table == "conversation_summaries"
        )));
        assert!(!cli
            .artifacts
            .iter()
            .any(|a| a.primary_path().to_string_lossy().contains("/antigravity/")));
    }

    #[test]
    fn preview_returns_chat_messages_and_empty_for_antigravity() {
        let adapter = GeminiAdapter::new(&fixture_home());
        let sessions = adapter.scan().expect("scan");
        let alpha = sessions
            .iter()
            .find(|s| s.id == "a1b2c3d4-1111-4aaa-8bbb-000000000001")
            .expect("alpha");
        let msgs = adapter.preview(alpha, 10).expect("preview");
        let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);
        assert_eq!(msgs[0].text, "fixture question one");
        assert_eq!(adapter.preview(alpha, 2).expect("preview").len(), 2);

        let cli = sessions
            .iter()
            .find(|s| s.id == "5a1b2c3d-4444-4eee-8fff-000000000004")
            .expect("cli");
        assert!(adapter.preview(cli, 10).expect("preview").is_empty());
    }

    #[test]
    fn fixture_presence_lock_is_not_live() {
        assert!(GeminiAdapter::new(&fixture_home())
            .live_session_ids()
            .is_empty());
    }

    #[test]
    fn held_presence_lock_is_live() {
        let dir = tempfile::tempdir().expect("tempdir");
        let presence = dir.path().join(".gemini/antigravity-cli/presence");
        fs::create_dir_all(&presence).expect("mkdir");
        let lock_path = presence.join("abc.lock");
        let held = fs::File::create(&lock_path).expect("create");
        held.lock().expect("lock");
        let idle = presence.join("idle.lock");
        fs::write(&idle, b"").expect("write");

        let live = GeminiAdapter::new(dir.path()).live_session_ids();
        assert_eq!(live, vec!["abc".to_string()]);
    }

    #[test]
    fn orphan_logs_entries_become_sessions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let proj = dir.path().join(".gemini/tmp/beta");
        fs::create_dir_all(&proj).expect("mkdir");
        let logs = r#"[{"sessionId":"orphan-1","messageId":0,"type":"user","message":"lost prompt","timestamp":"2026-03-01T08:00:00.000Z"}]"#;
        fs::write(proj.join("logs.json"), logs).expect("write");

        let sessions = GeminiAdapter::new(dir.path()).scan().expect("scan");
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, "orphan-1");
        assert_eq!(s.title.as_deref(), Some("lost prompt"));
        assert_eq!(s.message_count, Some(1));
        assert_eq!(s.bytes, 0);
        assert_eq!(s.created, Some(iso("2026-03-01T08:00:00Z")));
    }

    #[test]
    fn hash_dir_resolves_through_history_project_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gem = dir.path().join(".gemini");
        let project = "/Users/fixture/work/gamma";
        let hash = sha256_hex(project.as_bytes());
        let chats = gem.join("tmp").join(&hash).join("chats");
        fs::create_dir_all(&chats).expect("mkdir");
        fs::create_dir_all(gem.join("history/gamma")).expect("mkdir");
        fs::write(gem.join("history/gamma/.project_root"), format!("{project}\n")).expect("write");
        let body = format!(
            r#"{{"sessionId":"g-1","projectHash":"{hash}","startTime":"2026-04-01T00:00:00.000Z","lastUpdated":"2026-04-01T00:01:00.000Z","messages":[{{"type":"user","content":"hi there"}}]}}"#
        );
        fs::write(chats.join("session-2026-04-01T00-00-g1.json"), body).expect("write");

        let sessions = GeminiAdapter::new(dir.path()).scan().expect("scan");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].project, Some(PathBuf::from(project)));
        assert_eq!(sessions[0].title.as_deref(), Some("hi there"));
    }

    #[test]
    fn percent_decode_and_uri_paths() {
        assert_eq!(
            uri_to_path("file:///Users/a/My%20App").expect("path"),
            PathBuf::from("/Users/a/My App")
        );
        assert_eq!(uri_to_path("relative/path"), None);
    }

    #[test]
    #[ignore]
    fn live_scan() {
        let home = dirs::home_dir().expect("home dir");
        let started = Instant::now();
        let adapter = GeminiAdapter::new(&home);
        let sessions = adapter.scan().expect("scan");
        let elapsed = started.elapsed();
        let total: u64 = sessions.iter().map(|s| s.bytes).sum();
        let live = adapter.live_session_ids();
        println!(
            "gemini: {} sessions, {} bytes, scan {:?}, live {}",
            sessions.len(),
            total,
            elapsed,
            live.len()
        );
        for s in sessions.iter().take(3) {
            println!(
                "  {} | title={:?} | project={:?} | messages={:?} | bytes={}",
                s.id, s.title, s.project, s.message_count, s.bytes
            );
        }
    }
}
