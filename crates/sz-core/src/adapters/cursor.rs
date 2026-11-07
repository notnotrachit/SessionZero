//! Cursor sessions come from two stores, which share no ids:
//! - Agent transcripts: `~/.cursor/projects/<slug>/agent-transcripts/<id>/<id>.jsonl`, plus
//!   subagent transcripts in `<id>/subagents/*.jsonl`.
//! - Composers: `cursorDiskKV` (`composerData:<id>`, `bubbleId:<id>:<bubble>`) and the
//!   `composerHeaders` table in `User/globalStorage/state.vscdb`.
//!
//! Layout notes: `docs/formats/cursor.md`.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::adapter::Adapter;
use crate::model::{AgentKind, Artifact, DbDelete, PreviewMsg, Session};
use crate::util::{mtime, truncate};

const TITLE_MAX: usize = 120;
const TEXT_MAX: usize = 2000;
/// Upper bound on absolute paths collected from one transcript when resolving its project.
const MAX_PATHS: usize = 200;
const KV_TABLE: &str = "cursorDiskKV";
const HEADERS_TABLE: &str = "composerHeaders";

pub struct CursorAdapter {
    home: PathBuf,
}

impl CursorAdapter {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }

    fn projects_dir(&self) -> PathBuf {
        self.home.join(".cursor/projects")
    }

    fn user_dir(&self) -> PathBuf {
        self.home.join("Library/Application Support/Cursor/User")
    }

    fn global_db(&self) -> PathBuf {
        self.user_dir().join("globalStorage/state.vscdb")
    }

    /// Workspace storage id (directory name) -> folder path, from each `workspace.json`.
    fn workspace_folders(&self) -> HashMap<String, PathBuf> {
        let mut out = HashMap::new();
        for dir in sorted_entries(&self.user_dir().join("workspaceStorage")) {
            let Some(folder) = read_json(&dir.join("workspace.json"))
                .and_then(|v| v.get("folder").and_then(Value::as_str).and_then(uri_to_path))
            else {
                continue;
            };
            out.insert(file_name(&dir), folder);
        }
        out
    }

    fn transcript_sessions(&self, folders: &HashMap<String, PathBuf>) -> Vec<Session> {
        let folder_by_slug: HashMap<String, PathBuf> = folders
            .values()
            .map(|p| (slug_of(p), p.clone()))
            .collect();
        let mut decode_cache: HashMap<String, Option<PathBuf>> = HashMap::new();
        let mut out = Vec::new();

        for proj in sorted_entries(&self.projects_dir()) {
            if !proj.is_dir() {
                continue;
            }
            let slug = file_name(&proj);
            for entry in sorted_entries(&proj.join("agent-transcripts")) {
                if entry.is_dir() {
                    let id = file_name(&entry);
                    let main = entry.join(format!("{id}.jsonl"));
                    if !main.is_file() {
                        continue;
                    }
                    let info = scan_transcript(&main);
                    let paths = info.as_ref().map(|i| i.abs_paths.clone()).unwrap_or_default();
                    let project =
                        resolve_project(&slug, &paths, &folder_by_slug, &mut decode_cache);
                    out.push(transcript_session(
                        id,
                        &main,
                        info.as_ref(),
                        project.clone(),
                        Artifact::Dir(entry.clone()),
                    ));

                    for sub in sorted_entries(&entry.join("subagents")) {
                        if !is_jsonl(&sub) {
                            continue;
                        }
                        let Some(sub_id) = stem(&sub) else { continue };
                        let sub_info = scan_transcript(&sub);
                        let sub_paths = sub_info
                            .as_ref()
                            .map(|i| i.abs_paths.clone())
                            .unwrap_or_default();
                        let sub_project =
                            resolve_project(&slug, &sub_paths, &folder_by_slug, &mut decode_cache)
                                .or_else(|| project.clone());
                        out.push(transcript_session(
                            sub_id,
                            &sub,
                            sub_info.as_ref(),
                            sub_project,
                            Artifact::File(sub.clone()),
                        ));
                    }
                } else if is_jsonl(&entry) {
                    // Loose transcript file directly under agent-transcripts.
                    let Some(id) = stem(&entry) else { continue };
                    let info = scan_transcript(&entry);
                    let paths = info.as_ref().map(|i| i.abs_paths.clone()).unwrap_or_default();
                    let project =
                        resolve_project(&slug, &paths, &folder_by_slug, &mut decode_cache);
                    out.push(transcript_session(
                        id,
                        &entry,
                        info.as_ref(),
                        project,
                        Artifact::File(entry.clone()),
                    ));
                }
            }
        }
        out
    }

    fn composer_sessions(&self, folders: &HashMap<String, PathBuf>) -> Vec<Session> {
        let db = self.global_db();
        if !db.is_file() {
            return Vec::new();
        }
        let Some(conn) = open_ro(&db) else {
            return Vec::new();
        };
        let headers = load_headers(&conn);
        let (composer_len, bubbles) = load_kv_sizes(&conn);

        let ids: BTreeSet<String> = composer_len
            .keys()
            .chain(headers.keys())
            .cloned()
            .collect();

        let mut out = Vec::new();
        for id in ids {
            let data = read_composer(&conn, &id);
            let header = headers.get(&id);

            let title = data
                .as_ref()
                .and_then(|d| d.get("name").and_then(Value::as_str))
                .and_then(first_line)
                .or_else(|| {
                    data.as_ref()
                        .and_then(|d| first_user_text(&conn, &id, d))
                        .and_then(|t| user_title(&t))
                });

            let workspace_id = data
                .as_ref()
                .and_then(|d| d.pointer("/workspaceIdentifier/id").and_then(Value::as_str))
                .or_else(|| header.and_then(|h| h.workspace.as_deref()));
            let project = workspace_id.and_then(|w| folders.get(w)).cloned();

            let created = data
                .as_ref()
                .and_then(|d| d.get("createdAt").and_then(Value::as_i64))
                .or_else(|| header.and_then(|h| h.created))
                .and_then(ms_to_utc);
            let last_active = data
                .as_ref()
                .and_then(|d| d.get("lastUpdatedAt").and_then(Value::as_i64))
                .or_else(|| header.and_then(|h| h.updated))
                .and_then(ms_to_utc);

            let bubble_count = bubbles.get(&id).map(|(count, _)| *count);
            let listed = data
                .as_ref()
                .and_then(|d| d.get("fullConversationHeadersOnly").and_then(Value::as_array))
                .map(|a| a.len() as u64);
            let message_count = listed.or(bubble_count);

            let mut artifacts = Vec::new();
            let mut deletes = Vec::new();
            let mut row_bytes = 0;
            if let Some(len) = composer_len.get(&id) {
                deletes.push(DbDelete {
                    table: KV_TABLE.into(),
                    where_col: "key".into(),
                    value: format!("composerData:{id}"),
                });
                row_bytes += len;
            }
            if let Some(h) = header {
                deletes.push(DbDelete {
                    table: HEADERS_TABLE.into(),
                    where_col: "composerId".into(),
                    value: id.clone(),
                });
                row_bytes += h.bytes;
            }
            artifacts.push(Artifact::DbRows {
                db: db.clone(),
                deletes,
                bytes: row_bytes,
            });
            if let Some((count, bytes)) = bubbles.get(&id) {
                if *count > 0 {
                    artifacts.push(Artifact::DbPrefix {
                        db: db.clone(),
                        table: KV_TABLE.into(),
                        col: "key".into(),
                        prefix: format!("bubbleId:{id}:"),
                        bytes: *bytes,
                    });
                }
            }

            out.push(make_session(
                id, title, project, created, last_active, message_count, artifacts,
            ));
        }
        out
    }
}

impl Adapter for CursorAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Cursor
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![
            self.home.join(".cursor"),
            self.home.join("Library/Application Support/Cursor/User"),
        ]
    }

    fn scan(&self) -> anyhow::Result<Vec<Session>> {
        let folders = self.workspace_folders();
        let mut sessions = self.transcript_sessions(&folders);
        sessions.extend(self.composer_sessions(&folders));
        sessions.sort_by(|a, b| {
            b.last_active
                .cmp(&a.last_active)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(sessions)
    }

    fn preview(&self, s: &Session, max: usize) -> anyhow::Result<Vec<PreviewMsg>> {
        let composer_db = s.artifacts.iter().find_map(|a| match a {
            Artifact::DbRows { db, .. } => Some(db.clone()),
            _ => None,
        });
        let messages: Vec<(String, String)> = match composer_db {
            Some(db) => match open_ro(&db) {
                Some(conn) => composer_messages(&conn, &s.id),
                None => Vec::new(),
            },
            None => match transcript_path(s) {
                Some(path) => transcript_messages(&path),
                None => Vec::new(),
            },
        };
        Ok(messages
            .into_iter()
            .take(max)
            .map(|(role, text)| PreviewMsg {
                role,
                text,
                ts: None,
            })
            .collect())
    }

    fn process_names(&self) -> &'static [&'static str] {
        &["Cursor", "cursor-agent"]
    }

    /// Cursor keeps no live marker that was observed, so nothing is reported as running.
    fn live_session_ids(&self) -> Vec<String> {
        Vec::new()
    }
}


// ---------------------------------------------------------------------------------------------
// Agent transcripts
// ---------------------------------------------------------------------------------------------

struct TranscriptInfo {
    messages: u64,
    title: Option<String>,
    abs_paths: Vec<PathBuf>,
}

fn transcript_session(
    id: String,
    main: &Path,
    info: Option<&TranscriptInfo>,
    project: Option<PathBuf>,
    artifact: Artifact,
) -> Session {
    // Transcripts carry no timestamps, so the file mtime is the only activity signal.
    make_session(
        id,
        info.and_then(|i| i.title.clone()),
        project,
        None,
        mtime(main),
        info.map(|i| i.messages),
        vec![artifact],
    )
}

/// One pass over a transcript: role record count, first user line, and absolute paths named in
/// tool inputs (used to resolve the project).
fn scan_transcript(path: &Path) -> Option<TranscriptInfo> {
    let bytes = fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut info = TranscriptInfo {
        messages: 0,
        title: None,
        abs_paths: Vec::new(),
    };
    for line in text.lines() {
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let role = match rec.get("role").and_then(Value::as_str) {
            Some(r @ ("user" | "assistant")) => r,
            _ => continue,
        };
        info.messages += 1;
        let blocks = rec
            .pointer("/message/content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") if role == "user" && info.title.is_none() => {
                    info.title = block.get("text").and_then(Value::as_str).and_then(user_title);
                }
                Some("tool_use") if info.abs_paths.len() < MAX_PATHS => {
                    if let Some(input) = block.get("input") {
                        collect_abs_paths(input, &mut info.abs_paths);
                    }
                }
                _ => {}
            }
        }
    }
    Some(info)
}

fn collect_abs_paths(v: &Value, out: &mut Vec<PathBuf>) {
    if out.len() >= MAX_PATHS {
        return;
    }
    match v {
        Value::String(s) if s.len() > 1 && s.starts_with('/') && !s.contains('\n') => {
            out.push(PathBuf::from(s));
        }
        Value::Array(items) => items.iter().for_each(|i| collect_abs_paths(i, out)),
        Value::Object(map) => map.values().for_each(|i| collect_abs_paths(i, out)),
        _ => {}
    }
}

/// Messages of a transcript as (role, text). Records with no text are skipped.
fn transcript_messages(path: &Path) -> Vec<(String, String)> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut out = Vec::new();
    for line in text.lines() {
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let role = match rec.get("role").and_then(Value::as_str) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };
        let body: String = rec
            .pointer("/message/content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if body.trim().is_empty() {
            continue;
        }
        out.push((role.to_owned(), truncate(&body, TEXT_MAX)));
    }
    out
}

/// The transcript file behind a transcript session: `<dir>/<id>.jsonl` for a directory artifact,
/// or the file itself for a file artifact.
fn transcript_path(s: &Session) -> Option<PathBuf> {
    s.artifacts.iter().find_map(|a| match a {
        Artifact::Dir(dir) => {
            let main = dir.join(format!("{}.jsonl", s.id));
            main.is_file().then_some(main)
        }
        Artifact::File(file) if is_jsonl(file) => Some(file.clone()),
        _ => None,
    })
}

/// Project for a transcript directory `slug`. Order: a workspace folder whose slug matches, then
/// an ancestor of an absolute path named in the transcript, then a decode of the slug against
/// directories that exist on disk. The slug alone is lossy (`-` is ambiguous), so an unresolved
/// slug returns `None`.
fn resolve_project(
    slug: &str,
    abs_paths: &[PathBuf],
    folder_by_slug: &HashMap<String, PathBuf>,
    cache: &mut HashMap<String, Option<PathBuf>>,
) -> Option<PathBuf> {
    if let Some(folder) = folder_by_slug.get(slug) {
        return Some(folder.clone());
    }
    for path in abs_paths {
        if let Some(anc) = path.ancestors().find(|a| slug_of(a) == slug) {
            return Some(anc.to_path_buf());
        }
    }
    cache
        .entry(slug.to_owned())
        .or_insert_with(|| {
            let tokens: Vec<&str> = slug.split('-').collect();
            decode_under(Path::new("/"), &tokens)
        })
        .clone()
}

/// Rebuilds a path from its `-` joined tokens, consuming only directories that exist. Longer
/// segments are tried first, so `GitHub-Personal` wins over `GitHub` then `Personal` when both
/// exist.
fn decode_under(base: &Path, tokens: &[&str]) -> Option<PathBuf> {
    if tokens.is_empty() {
        return Some(base.to_path_buf());
    }
    for end in (1..=tokens.len()).rev() {
        let candidate = base.join(tokens[..end].join("-"));
        if candidate.is_dir() {
            if let Some(found) = decode_under(&candidate, &tokens[end..]) {
                return Some(found);
            }
        }
    }
    None
}

/// Cursor's transcript directory name for a project path: `/` becomes `-`, leading slash dropped.
fn slug_of(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "-")
        .trim_start_matches('-')
        .to_owned()
}

fn is_jsonl(p: &Path) -> bool {
    p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("jsonl")
}

// ---------------------------------------------------------------------------------------------
// Composers (state.vscdb)
// ---------------------------------------------------------------------------------------------

struct Header {
    workspace: Option<String>,
    created: Option<i64>,
    updated: Option<i64>,
    /// `length(value)` of the header row.
    bytes: u64,
}

fn load_headers(conn: &Connection) -> HashMap<String, Header> {
    let mut out = HashMap::new();
    let sql = format!(
        "SELECT composerId, workspaceId, createdAt, lastUpdatedAt, length(value) FROM {HEADERS_TABLE}"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return out;
    };
    let rows = stmt.query_map([], |r| {
        Ok((
            text_at(r, 0)?,
            Header {
                workspace: text_at(r, 1)?,
                created: int_at(r, 2)?,
                updated: int_at(r, 3)?,
                bytes: int_at(r, 4)?.and_then(|n| u64::try_from(n).ok()).unwrap_or(0),
            },
        ))
    });
    if let Ok(rows) = rows {
        for (id, header) in rows.flatten() {
            if let Some(id) = id {
                out.insert(id, header);
            }
        }
    }
    out
}

/// One pass over the KV table for sizes. Returns composerData length per composer id, and the
/// bubble (count, bytes) per composer id.
fn load_kv_sizes(conn: &Connection) -> (HashMap<String, u64>, HashMap<String, (u64, u64)>) {
    let mut composer = HashMap::new();
    let mut bubbles: HashMap<String, (u64, u64)> = HashMap::new();
    let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT key, length(value) FROM {KV_TABLE} WHERE key LIKE 'composerData:%' OR key LIKE 'bubbleId:%'"
    )) else {
        return (composer, bubbles);
    };
    let rows = stmt.query_map([], |r| Ok((text_at(r, 0)?, int_at(r, 1)?)));
    let Ok(rows) = rows else {
        return (composer, bubbles);
    };
    for (key, len) in rows.flatten() {
        let (Some(key), Some(len)) = (key, len) else {
            continue;
        };
        let len = u64::try_from(len).unwrap_or(0);
        if let Some(id) = key.strip_prefix("composerData:") {
            composer.insert(id.to_owned(), len);
        } else if let Some(rest) = key.strip_prefix("bubbleId:") {
            if let Some((id, _bubble)) = rest.split_once(':') {
                let entry = bubbles.entry(id.to_owned()).or_insert((0, 0));
                entry.0 += 1;
                entry.1 += len;
            }
        }
    }
    (composer, bubbles)
}

fn read_kv(conn: &Connection, key: &str) -> Option<Value> {
    let bytes: Vec<u8> = conn
        .query_row(
            &format!("SELECT value FROM {KV_TABLE} WHERE key = ?1"),
            [key],
            |r| {
                Ok(match r.get_ref(0)? {
                    ValueRef::Text(b) | ValueRef::Blob(b) => b.to_vec(),
                    _ => Vec::new(),
                })
            },
        )
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn read_composer(conn: &Connection, id: &str) -> Option<Value> {
    read_kv(conn, &format!("composerData:{id}"))
}

/// Text of a bubble row: `bubbleId:<composer>:<bubble>`.
fn bubble_value(conn: &Connection, composer: &str, bubble: &str) -> Option<Value> {
    read_kv(conn, &format!("bubbleId:{composer}:{bubble}"))
}

/// Bubbles of a composer in conversation order, as (type, bubble id). Type 1 is user, 2 is
/// assistant.
fn ordered_bubbles(data: &Value) -> Vec<(u64, String)> {
    data.get("fullConversationHeadersOnly")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|h| {
                    let kind = h.get("type").and_then(Value::as_u64)?;
                    let bubble = h.get("bubbleId").and_then(Value::as_str)?;
                    Some((kind, bubble.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn first_user_text(conn: &Connection, composer: &str, data: &Value) -> Option<String> {
    ordered_bubbles(data)
        .into_iter()
        .filter(|(kind, _)| *kind == 1)
        .find_map(|(_, bubble)| {
            bubble_value(conn, composer, &bubble)?
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
}

fn composer_messages(conn: &Connection, composer: &str) -> Vec<(String, String)> {
    let Some(data) = read_composer(conn, composer) else {
        return Vec::new();
    };
    ordered_bubbles(&data)
        .into_iter()
        .filter_map(|(kind, bubble)| {
            let role = match kind {
                1 => "user",
                2 => "assistant",
                _ => return None,
            };
            let text = bubble_value(conn, composer, &bubble)?
                .get("text")
                .and_then(Value::as_str)?
                .to_owned();
            (!text.trim().is_empty()).then(|| (role.to_owned(), truncate(&text, TEXT_MAX)))
        })
        .collect()
}

fn ms_to_utc(ms: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms).single()
}

// ---------------------------------------------------------------------------------------------
// Shared helpers.
// ---------------------------------------------------------------------------------------------

fn make_session(
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
        agent: AgentKind::Cursor,
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

/// Title from the first user text. Cursor prepends wrapper lines such as `<timestamp>…</timestamp>`
/// or `<git_diff_…>` to user turns, so lines that are only a tag (`<…>` at both ends) are skipped.
fn user_title(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !(l.is_empty() || (l.starts_with('<') && l.ends_with('>'))))
        .map(|l| truncate(l, TITLE_MAX))
}

fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| truncate(l, TITLE_MAX))
}

/// `file:///abs/path` (percent-encoded) or a bare absolute path.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let raw = uri.strip_prefix("file://").unwrap_or(uri);
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

/// Opens a SQLite file read-only. Tries `mode=ro` first, then `immutable=1` when the first open
/// cannot read the file.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Instant, SystemTime};

    fn fixture_home() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/cursor")
    }

    fn expected() -> Value {
        read_json(&fixture_home().join("EXPECTED.json")).expect("EXPECTED.json")
    }

    fn iso(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("iso")
            .with_timezone(&Utc)
    }

    /// Copies the fixture HOME into a temp dir and sets transcript mtimes, because a checkout
    /// resets them and the expected last_active values depend on them. The repo copy stays
    /// untouched.
    fn staged_home() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        copy_tree(&fixture_home(), dir.path());
        let set = |rel: &str, when: &str| {
            let path = dir.path().join(rel);
            let t: SystemTime = iso(when).into();
            fs::File::open(&path)
                .and_then(|f| f.set_modified(t))
                .expect("set mtime");
        };
        let slug_a = ".cursor/projects/Users-fixture-work-alpha-app/agent-transcripts/aaaaaaaa-0001-4000-8000-000000000001";
        let slug_b = ".cursor/projects/Users-fixture-work-beta-tool/agent-transcripts/aaaaaaaa-0003-4000-8000-000000000003";
        set(
            &format!("{slug_a}/aaaaaaaa-0001-4000-8000-000000000001.jsonl"),
            "2026-02-01T10:20:00Z",
        );
        set(
            &format!("{slug_a}/subagents/aaaaaaaa-0002-4000-8000-000000000002.jsonl"),
            "2026-02-01T10:05:00Z",
        );
        set(
            &format!("{slug_b}/aaaaaaaa-0003-4000-8000-000000000003.jsonl"),
            "2026-02-03T09:12:00Z",
        );
        dir
    }

    fn copy_tree(src: &Path, dst: &Path) {
        for entry in walkdir::WalkDir::new(src).into_iter().filter_map(|e| e.ok()) {
            let rel = entry.path().strip_prefix(src).expect("prefix");
            let target = dst.join(rel);
            if entry.file_type().is_dir() {
                fs::create_dir_all(&target).expect("mkdir");
            } else {
                fs::copy(entry.path(), &target).expect("copy");
            }
        }
    }

    fn find<'a>(sessions: &'a [Session], id: &str) -> &'a Session {
        sessions
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("missing session {id}"))
    }

    #[test]
    fn scan_matches_expected_fixture() {
        let home = staged_home();
        let sessions = CursorAdapter::new(home.path()).scan().expect("scan");
        let exp = expected();
        let expected_sessions = exp["sessions"].as_array().expect("sessions");
        assert_eq!(sessions.len(), expected_sessions.len(), "{sessions:#?}");

        for e in expected_sessions {
            let id = e["id"].as_str().expect("id");
            let s = find(&sessions, id);
            assert_eq!(s.title.as_deref(), e["title"].as_str(), "title {id}");
            assert_eq!(
                s.project.as_ref().map(|p| p.to_string_lossy().into_owned()),
                e["project"].as_str().map(str::to_owned),
                "project {id}"
            );
            assert_eq!(s.message_count, e["message_count"].as_u64(), "count {id}");
            assert_eq!(s.created, e["created"].as_str().map(iso), "created {id}");
            assert_eq!(s.last_active, e["last_active"].as_str().map(iso), "last {id}");
            if let Some(dir_bytes) = e["dir_bytes"].as_u64() {
                assert_eq!(s.bytes, dir_bytes, "bytes {id}");
            }
        }

        // Composer: composerData (604) + composerHeaders row (382) + two bubbles (109 + 107).
        let composer = find(&sessions, "c0ffee00-1111-4222-8333-000000000011");
        assert_eq!(composer.bytes, 604 + 382 + 109 + 107);
    }

    #[test]
    fn composer_artifacts_use_exact_keys_and_prefix() {
        let home = staged_home();
        let sessions = CursorAdapter::new(home.path()).scan().expect("scan");
        let composer = find(&sessions, "c0ffee00-1111-4222-8333-000000000011");
        let deletes: Vec<(String, String, String)> = composer
            .artifacts
            .iter()
            .filter_map(|a| match a {
                Artifact::DbRows { deletes, .. } => Some(deletes.iter().map(|d| {
                    (d.table.clone(), d.where_col.clone(), d.value.clone())
                })),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(deletes.contains(&(
            "cursorDiskKV".into(),
            "key".into(),
            "composerData:c0ffee00-1111-4222-8333-000000000011".into()
        )));
        assert!(deletes.contains(&(
            "composerHeaders".into(),
            "composerId".into(),
            "c0ffee00-1111-4222-8333-000000000011".into()
        )));
        assert!(composer.artifacts.iter().any(|a| matches!(
            a,
            Artifact::DbPrefix { prefix, table, .. }
                if prefix == "bubbleId:c0ffee00-1111-4222-8333-000000000011:" && table == "cursorDiskKV"
        )));
    }

    #[test]
    fn parent_transcript_owns_subagent_directory() {
        let home = staged_home();
        let sessions = CursorAdapter::new(home.path()).scan().expect("scan");
        let parent = find(&sessions, "aaaaaaaa-0001-4000-8000-000000000001");
        assert!(matches!(parent.artifacts.as_slice(), [Artifact::Dir(_)]));
        let child = find(&sessions, "aaaaaaaa-0002-4000-8000-000000000002");
        assert!(matches!(child.artifacts.as_slice(), [Artifact::File(_)]));
    }

    #[test]
    fn preview_returns_transcript_and_composer_messages() {
        let home = staged_home();
        let adapter = CursorAdapter::new(home.path());
        let sessions = adapter.scan().expect("scan");

        let alpha = find(&sessions, "aaaaaaaa-0001-4000-8000-000000000001");
        let texts: Vec<String> = adapter
            .preview(alpha, 10)
            .expect("preview")
            .into_iter()
            .map(|m| format!("{}: {}", m.role, m.text))
            .collect();
        assert_eq!(
            texts,
            vec![
                "user: fixture request alpha",
                "assistant: fixture reply alpha",
                "user: fixture follow up",
                "assistant: fixture final reply",
            ]
        );
        assert_eq!(adapter.preview(alpha, 1).expect("preview").len(), 1);

        let composer = find(&sessions, "c0ffee00-1111-4222-8333-000000000011");
        let msgs = adapter.preview(composer, 10).expect("preview");
        let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant"]);
        assert_eq!(msgs[1].text, "fixture composer answer");
    }

    #[test]
    fn nothing_is_reported_live() {
        assert!(CursorAdapter::new(&fixture_home()).live_session_ids().is_empty());
    }

    #[test]
    fn slug_decodes_only_existing_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("proj-one/sub")).expect("mkdir");
        let found = decode_under(dir.path(), &["proj", "one", "sub"]);
        assert_eq!(found, Some(dir.path().join("proj-one/sub")));
        assert_eq!(decode_under(dir.path(), &["missing", "dir"]), None);
    }

    #[test]
    fn user_title_skips_wrapper_tag_lines() {
        let text = "<timestamp>Tuesday, 10:43 PM</timestamp>\n<user_query>\nFix the build\n</user_query>";
        assert_eq!(user_title(text).as_deref(), Some("Fix the build"));
        assert_eq!(user_title("<git_diff_x>\n</git_diff_x>"), None);
    }

    #[test]
    fn slug_of_matches_cursor_directory_names() {
        assert_eq!(
            slug_of(Path::new("/Users/fixture/work/alpha-app")),
            "Users-fixture-work-alpha-app"
        );
    }

    #[test]
    fn file_uri_decodes_percent_encoding() {
        assert_eq!(
            uri_to_path("file:///Users/a/My%20App"),
            Some(PathBuf::from("/Users/a/My App"))
        );
    }

    #[test]
    fn missing_global_db_yields_no_composers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = CursorAdapter::new(dir.path()).scan().expect("scan");
        assert!(sessions.is_empty());
    }

    #[test]
    #[ignore]
    fn live_scan() {
        let home = dirs::home_dir().expect("home dir");
        let started = Instant::now();
        let sessions = CursorAdapter::new(&home).scan().expect("scan");
        let elapsed = started.elapsed();
        let total: u64 = sessions.iter().map(|s| s.bytes).sum();
        println!(
            "cursor: {} sessions, {} bytes, scan {:?}",
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
