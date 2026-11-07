//! OpenCode adapter. Reads `~/.local/share/opencode/opencode.db` strictly read-only,
//! plus the legacy JSON tree under `storage/`. See docs/formats/opencode.md.
//!
//! Warm scans reuse two caches. Per-session DB byte sums are keyed by a fingerprint of each
//! session's `part`, `message` and `session_message` rows, so only changed sessions are summed
//! again. Legacy `storage/` directory sizes are keyed by directory mtime.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Context;
use chrono::{DateTime, Utc};
use rayon::prelude::*;
use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;

use crate::adapter::Adapter;
use crate::cache::KeyedCache;
use crate::model::{AgentKind, Artifact, DbDelete, PreviewMsg, Session};
use crate::util::{path_size, truncate};

const PREVIEW_TEXT_LIMIT: usize = 2000;
const TITLE_LIMIT: usize = 120;
const DEFAULT_TITLE_PREFIX: &str = "New session - ";

/// Tables holding one session's rows, as (table, where column). Children come before
/// parents. The order follows tests/fixtures/opencode/EXPECTED.json.
const SESSION_ROW_TABLES: &[(&str, &str)] = &[
    ("event", "aggregate_id"),
    ("event_sequence", "aggregate_id"),
    ("part", "session_id"),
    ("message", "session_id"),
    ("todo", "session_id"),
    ("session_message", "session_id"),
    ("session_share", "session_id"),
    ("session_inbox", "session_id"),
    ("session_pending", "session_id"),
    ("instruction_entry", "session_id"),
    ("instruction_state", "session_id"),
    ("session_v2", "id"),
    ("session", "id"),
];

/// Per-session row aggregates. One query per table, grouped by session.
const PART_AGG_SQL: &str = "SELECT session_id, COUNT(*), COALESCE(MAX(time_updated), 0), \
     COALESCE(SUM(time_updated), 0), 0 FROM part GROUP BY session_id";
const MESSAGE_AGG_SQL: &str = "SELECT session_id, COUNT(*), COALESCE(MAX(time_updated), 0), \
     COALESCE(SUM(time_updated), 0), 0 FROM message GROUP BY session_id";
const SESSION_MESSAGE_AGG_SQL: &str = "SELECT session_id, COUNT(*), COALESCE(MAX(time_updated), 0), \
     COALESCE(SUM(time_updated), 0), COALESCE(SUM(type IN ('user','assistant')), 0) \
     FROM session_message GROUP BY session_id";

/// Sessions per rayon task when summing DB bytes. Each task opens one connection.
const SUM_CHUNK: usize = 32;

/// Per-session DB byte sums, in the order `part`, `message`, `session_message`. These use the same
/// `length(CAST(data AS BLOB))` byte proxy as the uncached aggregate.
const SESSION_BYTES_SQL: [&str; 3] = [
    "SELECT COALESCE(SUM(length(CAST(data AS BLOB))), 0) FROM part WHERE session_id = ?1",
    "SELECT COALESCE(SUM(length(CAST(data AS BLOB))), 0) FROM message WHERE session_id = ?1",
    "SELECT COALESCE(SUM(length(CAST(data AS BLOB))), 0) FROM session_message WHERE session_id = ?1",
];

pub struct OpenCodeAdapter {
    home: PathBuf,
}

/// Caches a scan reads and writes. Tests pass in file-backed caches. `scan` uses the
/// persistent ones for the real home.
struct Caches<'a> {
    /// Session id -> DB bytes, valid for one row fingerprint.
    sessions: &'a KeyedCache<u64>,
    /// Legacy directory path -> recursive size, valid for one mtime.
    dirs: &'a KeyedCache<u64>,
}

impl OpenCodeAdapter {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.home.join(".local/share/opencode")
    }

    fn db_path(&self) -> PathBuf {
        self.data_dir().join("opencode.db")
    }

    fn storage_dir(&self) -> PathBuf {
        self.data_dir().join("storage")
    }

    /// Full scan with the caches for the real home. For any other home they stay in memory.
    fn scan_cached(&self) -> anyhow::Result<Vec<Session>> {
        let sessions = KeyedCache::for_home(&self.home, "opencode-sessions");
        let dirs = KeyedCache::for_home(&self.home, "opencode-dirs");
        let out = self.scan_inner(Some(Caches {
            sessions: &sessions,
            dirs: &dirs,
        }));
        if let Err(err) = sessions.save() {
            eprintln!("sz: opencode session cache not saved: {err:#}");
        }
        if let Err(err) = dirs.save() {
            eprintln!("sz: opencode directory cache not saved: {err:#}");
        }
        out
    }

    /// Scan that reads every session and directory from source, with no caches.
    #[cfg(test)]
    fn scan_uncached(&self) -> anyhow::Result<Vec<Session>> {
        self.scan_inner(None)
    }

    fn scan_inner(&self, caches: Option<Caches<'_>>) -> anyhow::Result<Vec<Session>> {
        let db = self.db_path();
        if !db.is_file() {
            return Ok(vec![]);
        }
        let conn = open_readonly(&db)?;
        // One read snapshot for the whole scan. The fingerprints and the byte sums then
        // describe the same data. The transaction is rolled back when this function returns.
        let _snapshot = conn.unchecked_transaction()?;
        let storage = self.storage_dir();

        let projects = load_projects(&conn)?;
        let v1 = load_v1_sessions(&conn)?;
        let v2 = load_v2_sessions(&conn)?;
        let parts = load_row_aggregates(&conn, PART_AGG_SQL)?;
        let messages = load_row_aggregates(&conn, MESSAGE_AGG_SQL)?;
        let session_messages = load_row_aggregates(&conn, SESSION_MESSAGE_AGG_SQL)?;
        let message_ids = load_message_ids(&conn)?;

        let mut ids: Vec<String> = v1.keys().chain(v2.keys()).cloned().collect();
        ids.sort();
        ids.dedup();

        let mut out: Vec<Session> = Vec::with_capacity(ids.len());
        // Index into `out` and fingerprint, for every session whose DB bytes must be summed.
        let mut misses: Vec<(usize, String)> = Vec::new();
        for id in ids {
            let r1 = v1.get(&id);
            let r2 = v2.get(&id);
            let project_id = r1
                .map(|r| r.project_id.as_str())
                .or(r2.map(|r| r.project_id.as_str()));
            let directory = r1
                .map(|r| r.directory.clone())
                .or(r2.map(|r| r.directory.clone()));
            let project = project_id
                .and_then(|p| projects.get(p))
                .cloned()
                .or(directory)
                .map(PathBuf::from);

            let created_ms = [r1.map(|r| r.created_ms), r2.map(|r| r.created_ms)]
                .into_iter()
                .flatten()
                .min();
            let updated_ms = [r1.map(|r| r.updated_ms), r2.map(|r| r.updated_ms)]
                .into_iter()
                .flatten()
                .max();

            let title = resolve_title(
                &conn,
                &id,
                r2.and_then(|r| r.title.as_deref()),
                r1.map(|r| r.title.as_str()),
            )?;

            let msgs_v1 = messages.get(&id).map_or(0, |a| a.rows);
            let msgs_v2 = session_messages.get(&id).map_or(0, |a| a.chat_rows);
            let message_count = msgs_v1.max(msgs_v2);

            let fp = row_fingerprint(&parts, &messages, &session_messages, &id);
            let cached = caches.as_ref().and_then(|c| c.sessions.get(&id, &fp));
            if cached.is_none() {
                misses.push((out.len(), fp));
            }

            let mut artifacts = vec![Artifact::DbRows {
                db: db.clone(),
                deletes: row_deletes(&id),
                bytes: cached.unwrap_or(0),
            }];
            let message_ids_for_session = message_ids.get(&id).map(Vec::as_slice).unwrap_or(&[]);
            artifacts.extend(legacy_artifacts(
                &storage,
                &id,
                project_id,
                message_ids_for_session,
            ));

            let parent_id = r2
                .and_then(|r| r.parent_id.clone())
                .or_else(|| r1.and_then(|r| r.parent_id.clone()))
                .filter(|p| !p.trim().is_empty());
            out.push(Session {
                parent_id,
                agent: AgentKind::OpenCode,
                id,
                title,
                project,
                created: created_ms.and_then(DateTime::<Utc>::from_timestamp_millis),
                last_active: updated_ms.and_then(DateTime::<Utc>::from_timestamp_millis),
                message_count: Some(message_count),
                bytes: 0,
                artifacts,
            });
        }

        // DB byte sums for sessions the cache could not answer, read in parallel.
        let miss_ids: Vec<&str> = misses.iter().map(|(i, _)| out[*i].id.as_str()).collect();
        let sums = session_db_bytes_parallel(&db, &miss_ids)?;
        for ((index, fp), bytes) in misses.iter().zip(sums) {
            let session = &mut out[*index];
            if let Some(c) = &caches {
                c.sessions.put(&session.id, fp, bytes);
            }
            if let Some(Artifact::DbRows { bytes: slot, .. }) = session.artifacts.first_mut() {
                *slot = bytes;
            }
        }

        // Legacy directory sizes are the slow part of a cold scan, so they run in parallel too.
        let dir_cache = caches.as_ref().map(|c| c.dirs);
        out.par_iter_mut().for_each(|session| {
            session.bytes = session
                .artifacts
                .iter()
                .map(|a| match a {
                    Artifact::Dir(p) => dir_bytes(p, dir_cache),
                    other => other.own_bytes(),
                })
                .sum();
        });
        Ok(out)
    }
}

/// DB bytes for each session in `ids`, in the same order. Chunks run on rayon threads, and each
/// chunk opens its own read-only connection, because a `Connection` cannot cross threads.
///
/// Each chunk reads its own snapshot. The byte sums can therefore be newer than the fingerprints
/// taken earlier in the scan. A newer sum is safe to cache: the next scan sees a fingerprint that
/// no longer matches and sums again.
fn session_db_bytes_parallel(db: &Path, ids: &[&str]) -> anyhow::Result<Vec<u64>> {
    let chunks: Vec<Vec<u64>> = ids
        .par_chunks(SUM_CHUNK)
        .map(|chunk| -> anyhow::Result<Vec<u64>> {
            let conn = open_readonly(db)?;
            chunk
                .iter()
                .map(|id| session_db_bytes(&conn, id))
                .collect()
        })
        .collect::<anyhow::Result<_>>()?;
    Ok(chunks.into_iter().flatten().collect())
}

impl Adapter for OpenCodeAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::OpenCode
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.data_dir()]
    }

    fn scan(&self) -> anyhow::Result<Vec<Session>> {
        self.scan_cached()
    }

    fn preview(&self, s: &Session, max: usize) -> anyhow::Result<Vec<PreviewMsg>> {
        let db = self.db_path();
        if max == 0 || !db.is_file() {
            return Ok(vec![]);
        }
        let conn = open_readonly(&db)?;
        let mut msgs = read_messages(&conn, &s.id, None, max)?;
        for m in &mut msgs {
            m.text = truncate(&m.text, PREVIEW_TEXT_LIMIT);
        }
        Ok(msgs)
    }

    fn process_names(&self) -> &'static [&'static str] {
        &["opencode"]
    }

    /// OpenCode writes no per-session liveness marker, so no session is reported live.
    fn live_session_ids(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Opens the database read-only. Falls back to `immutable=1` when the normal read-only
/// open fails, for example when the WAL index file is not usable.
fn open_readonly(path: &Path) -> anyhow::Result<Connection> {
    let base = uri_escape(path);
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    match probe(&format!("file:{base}?mode=ro"), flags) {
        Ok(conn) => Ok(conn),
        Err(first) => probe(&format!("file:{base}?immutable=1"), flags).with_context(|| {
            format!(
                "open {} read-only (mode=ro failed: {first})",
                path.display()
            )
        }),
    }
}

/// Opens a URI and runs a trivial query, because SQLite can defer the failure to first use.
fn probe(uri: &str, flags: OpenFlags) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(uri, flags)?;
    conn.busy_timeout(Duration::from_secs(2))?;
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))?;
    Ok(conn)
}

/// Percent-encodes a path for use inside a `file:` URI.
fn uri_escape(path: &Path) -> String {
    let mut out = String::new();
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

struct V1Session {
    project_id: String,
    directory: String,
    title: String,
    created_ms: i64,
    updated_ms: i64,
    parent_id: Option<String>,
}

struct V2Session {
    project_id: String,
    directory: String,
    title: Option<String>,
    created_ms: i64,
    updated_ms: i64,
    parent_id: Option<String>,
}

/// Aggregates of one table's rows for one session. `chat_rows` counts `user` and `assistant`
/// rows and is only filled for `session_message`.
#[derive(Clone, Copy, Default)]
struct RowAgg {
    rows: u64,
    max_updated: i64,
    sum_updated: i64,
    chat_rows: u64,
}

fn load_projects(conn: &Connection) -> anyhow::Result<HashMap<String, String>> {
    let mut stmt = conn.prepare("SELECT id, worktree FROM project")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn load_v1_sessions(conn: &Connection) -> anyhow::Result<HashMap<String, V1Session>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, directory, title, time_created, time_updated, parent_id FROM session",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            V1Session {
                project_id: r.get(1)?,
                directory: r.get(2)?,
                title: r.get(3)?,
                created_ms: r.get(4)?,
                updated_ms: r.get(5)?,
                parent_id: r.get(6)?,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn load_v2_sessions(conn: &Connection) -> anyhow::Result<HashMap<String, V2Session>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, directory, title, time_created, time_updated, parent_id FROM session_v2",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            V2Session {
                project_id: r.get(1)?,
                directory: r.get(2)?,
                title: r.get(3)?,
                created_ms: r.get(4)?,
                updated_ms: r.get(5)?,
                parent_id: r.get(6)?,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Runs one of the `*_AGG_SQL` queries: (session_id, rows, max time_updated, chat rows).
fn load_row_aggregates(conn: &Connection, sql: &str) -> anyhow::Result<HashMap<String, RowAgg>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            RowAgg {
                rows: u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
                max_updated: r.get(2)?,
                sum_updated: r.get(3)?,
                chat_rows: u64::try_from(r.get::<_, i64>(4)?).unwrap_or(0),
            },
        ))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (sid, agg) = row?;
        out.insert(sid, agg);
    }
    Ok(out)
}

/// Changes whenever a session's `part`, `message` or `session_message` rows change. OpenCode
/// bumps `time_updated` on every write. The row count catches inserts and deletes, the max
/// catches a newer write, and the sum catches a write to an older row.
///
/// A `data` change that leaves `time_updated` alone is not seen. The cached byte count for that
/// session then stays stale until its next write.
fn row_fingerprint(
    parts: &HashMap<String, RowAgg>,
    messages: &HashMap<String, RowAgg>,
    session_messages: &HashMap<String, RowAgg>,
    id: &str,
) -> String {
    let mut fp = String::new();
    for (tag, map) in [("p", parts), ("m", messages), ("s", session_messages)] {
        let agg = map.get(id).copied().unwrap_or_default();
        let _ = write!(
            fp,
            "{tag}{}:{}:{};",
            agg.rows, agg.max_updated, agg.sum_updated
        );
    }
    fp
}

/// DB bytes of one session, summed per table with an indexed `session_id` lookup.
fn session_db_bytes(conn: &Connection, id: &str) -> anyhow::Result<u64> {
    let mut total = 0u64;
    for sql in SESSION_BYTES_SQL {
        let mut stmt = conn.prepare_cached(sql)?;
        let n: i64 = stmt.query_row(params![id], |r| r.get(0))?;
        total += u64::try_from(n).unwrap_or(0);
    }
    Ok(total)
}

/// Recursive size of a legacy directory, cached by its mtime. A file rewritten in place does not
/// change its directory's mtime, so such a rewrite is seen only after the directory changes.
/// Legacy `storage/` is stale in current installs and is effectively append-only.
fn dir_bytes(path: &Path, cache: Option<&KeyedCache<u64>>) -> u64 {
    let Some(cache) = cache else {
        return path_size(path);
    };
    let Some(fp) = dir_mtime_fingerprint(path) else {
        return path_size(path);
    };
    let key = path.to_string_lossy();
    if let Some(hit) = cache.get(&key, &fp) {
        return hit;
    }
    let size = path_size(path);
    cache.put(&key, &fp, size);
    size
}

fn dir_mtime_fingerprint(path: &Path) -> Option<String> {
    let mtime = fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(mtime.to_string())
}

fn load_message_ids(conn: &Connection) -> anyhow::Result<HashMap<String, Vec<String>>> {
    let mut stmt = conn.prepare("SELECT session_id, id FROM message ORDER BY time_created, id")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows {
        let (sid, mid) = row?;
        out.entry(sid).or_default().push(mid);
    }
    Ok(out)
}

/// Every DB delete needed to remove one session, children first.
fn row_deletes(id: &str) -> Vec<DbDelete> {
    SESSION_ROW_TABLES
        .iter()
        .map(|(table, col)| DbDelete {
            table: (*table).to_owned(),
            where_col: (*col).to_owned(),
            value: id.to_owned(),
        })
        .collect()
}

/// Legacy JSON files and directories under `storage/` that belong to one session.
/// The storage tree is stale in current installs, so each path is included only when it
/// exists.
fn legacy_artifacts(
    storage: &Path,
    id: &str,
    project_id: Option<&str>,
    message_ids: &[String],
) -> Vec<Artifact> {
    if !is_safe_component(id) {
        return vec![];
    }
    let mut out = Vec::new();
    let mut push_file = |p: PathBuf| {
        if p.is_file() {
            out.push(Artifact::File(p));
        }
    };
    if let Some(pid) = project_id.filter(|p| is_safe_component(p)) {
        push_file(storage.join("session").join(pid).join(format!("{id}.json")));
    }
    push_file(storage.join("todo").join(format!("{id}.json")));
    push_file(storage.join("session_diff").join(format!("{id}.json")));

    let mut dirs = vec![storage.join("message").join(id)];
    dirs.extend(
        message_ids
            .iter()
            .filter(|m| is_safe_component(m))
            .map(|m| storage.join("part").join(m)),
    );
    for d in dirs {
        if d.is_dir() {
            out.push(Artifact::Dir(d));
        }
    }
    out
}

fn is_safe_component(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', '\0'])
}

/// Chooses a session title. A non-default stored title wins. A default
/// `New session - <date>` title is replaced by the first user message when one exists.
fn resolve_title(
    conn: &Connection,
    id: &str,
    v2_title: Option<&str>,
    v1_title: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let stored: Vec<&str> = [v2_title, v1_title]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if let Some(named) = stored.iter().find(|t| !t.starts_with(DEFAULT_TITLE_PREFIX)) {
        return Ok(Some((*named).to_owned()));
    }
    let first_user = read_messages(conn, id, Some("user"), 1)?
        .into_iter()
        .next()
        .and_then(|m| m.text.lines().next().map(|l| truncate(l.trim(), TITLE_LIMIT)))
        .filter(|t| !t.is_empty());
    Ok(first_user.or_else(|| stored.first().map(|t| (*t).to_owned())))
}

/// Reads up to `limit` user/assistant messages in order. Uses the v2 `session_message`
/// rows when the session has them, and the v1 `message` + `part` rows otherwise.
fn read_messages(
    conn: &Connection,
    id: &str,
    role: Option<&str>,
    limit: usize,
) -> anyhow::Result<Vec<PreviewMsg>> {
    let v2 = read_messages_v2(conn, id, role, limit)?;
    if !v2.is_empty() {
        return Ok(v2);
    }
    read_messages_v1(conn, id, role, limit)
}

fn read_messages_v2(
    conn: &Connection,
    id: &str,
    role: Option<&str>,
    limit: usize,
) -> anyhow::Result<Vec<PreviewMsg>> {
    let mut stmt = conn.prepare(
        "SELECT type, data, time_created FROM session_message \
         WHERE session_id = ?1 AND type IN ('user','assistant') \
           AND (?2 IS NULL OR type = ?2) ORDER BY seq",
    )?;
    let rows = stmt.query_map(params![id, role], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        if out.len() >= limit {
            break;
        }
        let (kind, data, ts) = row?;
        let text = serde_json::from_str::<Value>(&data)
            .ok()
            .and_then(|v| v["text"].as_str().map(str::to_owned))
            .unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }
        out.push(PreviewMsg {
            role: kind,
            text,
            ts: DateTime::<Utc>::from_timestamp_millis(ts),
        });
    }
    Ok(out)
}

fn read_messages_v1(
    conn: &Connection,
    id: &str,
    role: Option<&str>,
    limit: usize,
) -> anyhow::Result<Vec<PreviewMsg>> {
    let mut msg_stmt = conn.prepare(
        "SELECT id, data, time_created FROM message WHERE session_id = ?1 ORDER BY time_created, id",
    )?;
    let mut part_stmt = conn.prepare("SELECT data FROM part WHERE message_id = ?1 ORDER BY id")?;
    let rows = msg_stmt.query_map(params![id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        if out.len() >= limit {
            break;
        }
        let (mid, data, ts) = row?;
        let Some(msg_role) = serde_json::from_str::<Value>(&data)
            .ok()
            .and_then(|v| v["role"].as_str().map(str::to_owned))
        else {
            continue;
        };
        if !matches!(msg_role.as_str(), "user" | "assistant")
            || role.is_some_and(|want| want != msg_role)
        {
            continue;
        }
        let mut texts = Vec::new();
        for part in part_stmt.query_map(params![mid], |r| r.get::<_, String>(0))? {
            let part = serde_json::from_str::<Value>(&part?).unwrap_or(Value::Null);
            if part["type"].as_str() == Some("text") {
                if let Some(t) = part["text"].as_str().filter(|t| !t.trim().is_empty()) {
                    texts.push(t.to_owned());
                }
            }
        }
        if texts.is_empty() {
            continue;
        }
        out.push(PreviewMsg {
            role: msg_role,
            text: texts.join("\n"),
            ts: DateTime::<Utc>::from_timestamp_millis(ts),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use walkdir::WalkDir;

    fn fixture_home() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/opencode")
    }

    fn read_expected(home: &Path) -> Value {
        let text = std::fs::read_to_string(home.join("EXPECTED.json")).expect("EXPECTED.json");
        serde_json::from_str(&text).expect("valid EXPECTED.json")
    }

    fn parse_iso(v: &Value) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(v.as_str()?)
            .ok()
            .map(|d| d.with_timezone(&Utc))
    }

    /// Relative paths (from `home`) of every file an artifact covers.
    fn expand(art: &Artifact, home: &Path) -> Vec<String> {
        let rel = |p: &Path| {
            p.strip_prefix(home)
                .expect("under home")
                .to_string_lossy()
                .into_owned()
        };
        match art {
            Artifact::File(p) => vec![rel(p)],
            Artifact::Dir(p) => WalkDir::new(p)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_file())
                .map(|e| rel(e.path()))
                .collect(),
            _ => vec![],
        }
    }

    /// Copies the fixture database into a temp home that tests may write to.
    fn writable_home(tmp: &Path) -> PathBuf {
        let dir = tmp.join(".local/share/opencode");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::copy(
            fixture_home().join(".local/share/opencode/opencode.db"),
            dir.join("opencode.db"),
        )
        .expect("copy fixture db");
        tmp.to_path_buf()
    }

    #[test]
    fn scan_matches_expected_json() {
        let home = fixture_home();
        let expected = read_expected(&home);
        let sessions = OpenCodeAdapter::new(&home).scan().expect("scan");
        let exp_sessions = expected["sessions"].as_array().expect("sessions");
        assert_eq!(sessions.len(), exp_sessions.len());

        for exp in exp_sessions {
            let id = exp["id"].as_str().expect("id");
            let s = sessions
                .iter()
                .find(|s| s.id == id)
                .unwrap_or_else(|| panic!("missing session {id}"));
            assert_eq!(s.agent, AgentKind::OpenCode);
            assert_eq!(s.title.as_deref(), exp["title"].as_str(), "title of {id}");
            assert_eq!(
                s.project,
                Some(PathBuf::from(exp["project"]["worktree"].as_str().unwrap()))
            );
            assert_eq!(s.created, parse_iso(&exp["created"]), "created of {id}");
            assert_eq!(
                s.last_active,
                parse_iso(&exp["last_active"]),
                "last_active of {id}"
            );
            assert_eq!(s.message_count, exp["message_count"].as_u64());

            let (deletes, db_bytes) = s
                .artifacts
                .iter()
                .find_map(|a| match a {
                    Artifact::DbRows { deletes, bytes, .. } => Some((deletes, *bytes)),
                    _ => None,
                })
                .expect("DbRows artifact");
            assert_eq!(db_bytes, exp["db_bytes"]["total"].as_u64().unwrap(), "db bytes {id}");
            let got: Vec<[String; 3]> = deletes
                .iter()
                .map(|d| [d.table.clone(), d.where_col.clone(), d.value.clone()])
                .collect();
            let want: Vec<[String; 3]> = exp["db_rows_delete_order"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| {
                    let r = row.as_array().unwrap();
                    [
                        r[0].as_str().unwrap().to_owned(),
                        r[1].as_str().unwrap().to_owned(),
                        r[2].as_str().unwrap().to_owned(),
                    ]
                })
                .collect();
            assert_eq!(got, want, "delete order of {id}");

            let mut got_files: Vec<String> = s
                .artifacts
                .iter()
                .filter(|a| !matches!(a, Artifact::DbRows { .. }))
                .flat_map(|a| expand(a, &home))
                .collect();
            got_files.sort();
            let mut want_files: Vec<String> = exp["legacy_files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            want_files.sort();
            assert_eq!(got_files, want_files, "legacy files of {id}");

            let legacy_bytes = exp["legacy_file_bytes"].as_u64().unwrap();
            assert_eq!(s.bytes, db_bytes + legacy_bytes, "total bytes of {id}");
        }
    }

    #[test]
    fn default_title_falls_back_to_first_user_message() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join(".local/share/opencode");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let db = dir.join("opencode.db");
        std::fs::copy(fixture_home().join(".local/share/opencode/opencode.db"), &db)
            .expect("copy fixture db");
        {
            let conn = Connection::open(&db).expect("writable temp copy");
            conn.execute(
                "UPDATE session SET title = 'New session - 2026-01-08' WHERE id = 'ses_fixture0001'",
                [],
            )
            .expect("update v1");
            conn.execute(
                "UPDATE session_v2 SET title = 'New session - 2026-01-08' WHERE id = 'ses_fixture0001'",
                [],
            )
            .expect("update v2");
        }
        let sessions = OpenCodeAdapter::new(tmp.path()).scan().expect("scan");
        let s = sessions.iter().find(|s| s.id == "ses_fixture0001").expect("session");
        assert_eq!(
            s.title.as_deref(),
            Some("Login redirects back to /login after OAuth. Find out why.")
        );
    }

    #[test]
    fn preview_returns_user_and_assistant_text_in_order() {
        let home = fixture_home();
        let adapter = OpenCodeAdapter::new(&home);
        let s = adapter
            .scan()
            .expect("scan")
            .into_iter()
            .find(|s| s.id == "ses_fixture0001")
            .expect("session");
        let msgs = adapter.preview(&s, 10).expect("preview");
        let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);
        assert_eq!(
            msgs[0].text,
            "Login redirects back to /login after OAuth. Find out why."
        );
        assert_eq!(adapter.preview(&s, 2).expect("preview").len(), 2);
    }

    #[test]
    fn live_session_ids_is_empty() {
        assert!(OpenCodeAdapter::new(&fixture_home())
            .live_session_ids()
            .is_empty());
    }

    #[test]
    fn connection_rejects_writes() {
        let db = fixture_home().join(".local/share/opencode/opencode.db");
        let conn = open_readonly(&db).expect("open read-only");
        assert!(conn.execute_batch("CREATE TABLE sz_probe(x)").is_err());
    }

    /// Cached scans (cold, then warm from disk) must equal the uncached scan, field for field.
    /// Changing one session's parts must invalidate only that session's entry.
    #[test]
    fn cached_scan_matches_uncached_scan() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = writable_home(tmp.path());
        let adapter = OpenCodeAdapter::new(&home);
        let sess_file = tmp.path().join("sessions.json");
        let dir_file = tmp.path().join("dirs.json");

        let run_cached = || {
            let sessions = KeyedCache::load(Some(sess_file.clone()));
            let dirs = KeyedCache::load(Some(dir_file.clone()));
            let out = adapter
                .scan_inner(Some(Caches {
                    sessions: &sessions,
                    dirs: &dirs,
                }))
                .expect("cached scan");
            sessions.save().expect("save sessions");
            dirs.save().expect("save dirs");
            out
        };
        let as_json = |s: &[Session]| serde_json::to_value(s).expect("serialize");

        let uncached = as_json(&adapter.scan_uncached().expect("uncached"));
        let cold = as_json(&run_cached());
        assert!(sess_file.is_file() && dir_file.is_file(), "caches persisted");
        let warm = as_json(&run_cached());
        assert_eq!(cold, uncached, "cold cached scan differs");
        assert_eq!(warm, uncached, "warm cached scan differs");

        // Grow one session's part data and bump its time_updated, as OpenCode does on a write.
        {
            let conn = Connection::open(home.join(".local/share/opencode/opencode.db"))
                .expect("writable temp copy");
            conn.execute(
                "UPDATE part SET data = data || ' ', time_updated = time_updated + 1 \
                 WHERE id = (SELECT id FROM part WHERE session_id = 'ses_fixture0001' LIMIT 1)",
                [],
            )
            .expect("update part");
        }
        let uncached_after = as_json(&adapter.scan_uncached().expect("uncached"));
        let warm_after = as_json(&run_cached());
        assert_ne!(uncached_after, uncached, "the edit must change some byte count");
        assert_eq!(warm_after, uncached_after, "warm scan after an edit differs");
    }

    /// Read-only check on the real home: an in-memory cached scan equals the uncached scan.
    #[test]
    #[ignore]
    fn live_cached_matches_uncached() {
        let home = dirs::home_dir().expect("home");
        let adapter = OpenCodeAdapter::new(&home);
        let uncached = serde_json::to_value(adapter.scan_uncached().expect("uncached"))
            .expect("serialize");
        let sessions = KeyedCache::load(None);
        let dirs = KeyedCache::load(None);
        let cache_pass = |sessions: &KeyedCache<u64>, dirs: &KeyedCache<u64>| {
            serde_json::to_value(
                adapter
                    .scan_inner(Some(Caches { sessions, dirs }))
                    .expect("cached"),
            )
            .expect("serialize")
        };
        assert_eq!(cache_pass(&sessions, &dirs), uncached, "cold cached scan differs");
        assert_eq!(cache_pass(&sessions, &dirs), uncached, "warm cached scan differs");
    }

    #[test]
    #[ignore]
    fn live_scan() {
        let home = dirs::home_dir().expect("home");
        let started = std::time::Instant::now();
        let sessions = OpenCodeAdapter::new(&home).scan().expect("live scan");
        let elapsed = started.elapsed();
        let total: u64 = sessions.iter().map(|s| s.bytes).sum();
        println!(
            "opencode live: {} sessions, {} bytes, scan {:?}",
            sessions.len(),
            total,
            elapsed
        );
        for s in sessions.iter().take(3) {
            println!(
                "  {} | {:?} | {:?} | msgs={:?} | bytes={}",
                s.id, s.title, s.project, s.message_count, s.bytes
            );
        }
    }
}
