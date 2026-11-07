//! Grok CLI adapter. Scans `~/.grok/sessions/<bucket>/<uuid>/` and reads the derived
//! FTS index and `active_sessions.json` read-only. See docs/formats/grok.md.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use sysinfo::{Pid, ProcessesToUpdate, System};

use crate::adapter::Adapter;
use crate::model::{AgentKind, Artifact, DbDelete, PreviewMsg, Session};
use crate::util::truncate;

const PREVIEW_TEXT_LIMIT: usize = 2000;
const PROMPT_HISTORY: &str = "prompt_history.jsonl";
const INDEX_FILE: &str = "session_search.sqlite";
const ACTIVE_FILE: &str = "active_sessions.json";

pub struct GrokAdapter {
    home: PathBuf,
}

impl GrokAdapter {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }

    fn grok_dir(&self) -> PathBuf {
        self.home.join(".grok")
    }

    fn sessions_dir(&self) -> PathBuf {
        self.grok_dir().join("sessions")
    }

    fn index_path(&self) -> PathBuf {
        self.sessions_dir().join(INDEX_FILE)
    }

    fn active_path(&self) -> PathBuf {
        self.grok_dir().join(ACTIVE_FILE)
    }

    /// Session ids that have a row in the derived FTS index. The index is rebuildable, so
    /// an unreadable index only means no index rows are scheduled for deletion.
    fn indexed_session_ids(&self) -> HashSet<String> {
        let path = self.index_path();
        if !path.is_file() {
            return HashSet::new();
        }
        let read = || -> anyhow::Result<HashSet<String>> {
            let conn = open_readonly(&path)?;
            let mut stmt = conn.prepare("SELECT session_id FROM session_docs")?;
            let ids = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<HashSet<_>>>()?;
            Ok(ids)
        };
        read().unwrap_or_default()
    }
}

impl Adapter for GrokAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Grok
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.grok_dir()]
    }

    fn scan(&self) -> anyhow::Result<Vec<Session>> {
        let sessions_dir = self.sessions_dir();
        if !sessions_dir.is_dir() {
            return Ok(vec![]);
        }
        let shared = Shared {
            indexed: self.indexed_session_ids(),
            active: read_active(&self.active_path())
                .into_iter()
                .map(|e| e.session_id)
                .collect(),
            active_path: self.active_path(),
            index_path: self.index_path(),
        };

        let mut out = Vec::new();
        for bucket in subdirs(&sessions_dir) {
            for session_dir in subdirs(&bucket) {
                if let Some(s) = parse_session(&session_dir, &bucket, &shared) {
                    out.push(s);
                }
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    fn preview(&self, s: &Session, max: usize) -> anyhow::Result<Vec<PreviewMsg>> {
        let Some(dir) = session_dir(s) else {
            return Ok(vec![]);
        };
        let Ok(file) = File::open(dir.join("chat_history.jsonl")) else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        for line in BufReader::new(file).lines() {
            if out.len() >= max {
                break;
            }
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let role = match v["type"].as_str() {
                Some(r @ ("user" | "assistant")) => r,
                _ => continue,
            };
            let Some(text) = v["content"].as_str().map(str::trim).filter(|t| !t.is_empty())
            else {
                continue;
            };
            out.push(PreviewMsg {
                role: role.to_owned(),
                text: truncate(text, PREVIEW_TEXT_LIMIT),
                ts: None,
            });
        }
        Ok(out)
    }

    fn process_names(&self) -> &'static [&'static str] {
        &["grok"]
    }

    /// Sessions whose registry entry names a live process. An entry is live when the pid
    /// exists and its start time matches `pid_start` (microseconds since epoch) within one
    /// second.
    fn live_session_ids(&self) -> Vec<String> {
        let mut sys = System::new();
        read_active(&self.active_path())
            .into_iter()
            .filter(|e| is_live(&mut sys, e))
            .map(|e| e.session_id)
            .collect()
    }
}

/// Lookups shared by every session parsed in one scan.
struct Shared {
    indexed: HashSet<String>,
    active: HashSet<String>,
    active_path: PathBuf,
    index_path: PathBuf,
}

/// Builds one session from `<bucket>/<uuid>/summary.json`. Returns None when the directory
/// has no readable summary. Subagent children are not sessions of their own: they sit
/// inside the parent directory, so deleting the parent covers them.
fn parse_session(dir: &Path, bucket: &Path, shared: &Shared) -> Option<Session> {
    let summary_text = fs::read_to_string(dir.join("summary.json")).ok()?;
    let summary: Value = serde_json::from_str(&summary_text).ok()?;
    let dir_name = dir.file_name()?.to_string_lossy().into_owned();
    let id = summary["info"]["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or(dir_name);

    let title = non_empty(&summary["generated_title"]).or_else(|| non_empty(&summary["session_summary"]));
    let project = summary["info"]["cwd"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| bucket_cwd(bucket));
    let created = parse_ts(&summary["created_at"]);
    let last_active =
        parse_ts(&summary["last_active_at"]).or_else(|| parse_ts(&summary["updated_at"]));
    let message_count = summary["num_chat_messages"].as_u64();

    let history = bucket.join(PROMPT_HISTORY);
    let mut artifacts = vec![Artifact::Dir(dir.to_path_buf())];
    if history.is_file() {
        artifacts.push(Artifact::JsonlLines {
            file: history,
            field: "session_id".into(),
            value: id.clone(),
        });
    }
    if shared.indexed.contains(&id) {
        artifacts.push(Artifact::DbRows {
            db: shared.index_path.clone(),
            deletes: vec![DbDelete {
                table: "session_docs".into(),
                where_col: "session_id".into(),
                value: id.clone(),
            }],
            // The index is derived and shared. Deleting a row does not shrink the file,
            // so no bytes are attributed to this session.
            bytes: 0,
        });
    }
    if shared.active.contains(&id) {
        artifacts.push(Artifact::JsonArrayEntry {
            file: shared.active_path.clone(),
            array_path: String::new(),
            field: "session_id".into(),
            value: id.clone(),
        });
    }

    let parent_id = non_empty(&summary["parent_session_id"]);
    let mut session = Session {
        parent_id,
        agent: AgentKind::Grok,
        id,
        title,
        project,
        created,
        last_active,
        message_count,
        bytes: 0,
        artifacts,
    };
    session.recompute_bytes();
    Some(session)
}

fn session_dir(s: &Session) -> Option<&Path> {
    s.artifacts.iter().find_map(|a| match a {
        Artifact::Dir(p) => Some(p.as_path()),
        _ => None,
    })
}

/// Immediate subdirectories, sorted. Symlinks are not followed.
fn subdirs(path: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(path) else {
        return vec![];
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

fn non_empty(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn parse_ts(v: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(v.as_str()?)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Working directory encoded in a bucket name (`%2FUsers%2Ffoo` -> `/Users/foo`).
fn bucket_cwd(bucket: &Path) -> Option<PathBuf> {
    let name = bucket.file_name()?.to_string_lossy();
    let decoded = percent_decode(&name);
    decoded.starts_with('/').then(|| PathBuf::from(decoded))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(v) = hex {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct ActiveEntry {
    session_id: String,
    pid: u32,
    pid_start_us: Option<u64>,
}

fn read_active(path: &Path) -> Vec<ActiveEntry> {
    let Ok(text) = fs::read_to_string(path) else {
        return vec![];
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&text) else {
        return vec![];
    };
    items
        .iter()
        .filter_map(|v| {
            Some(ActiveEntry {
                session_id: v["session_id"].as_str()?.to_owned(),
                pid: u32::try_from(v["pid"].as_u64()?).ok()?,
                pid_start_us: v["pid_start"].as_u64(),
            })
        })
        .collect()
}

fn is_live(sys: &mut System, e: &ActiveEntry) -> bool {
    let Some(start_us) = e.pid_start_us else {
        return false;
    };
    let pid = Pid::from_u32(e.pid);
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid)
        .is_some_and(|p| p.start_time().abs_diff(start_us / 1_000_000) <= 1)
}

/// Opens the index read-only. Falls back to `immutable=1` when the normal read-only open
/// fails, which happens while the index is live.
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

#[cfg(test)]
mod tests {
    use super::*;
    use sysinfo::Pid as SysPid;
    use walkdir::WalkDir;

    fn fixture_home() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/grok")
    }

    fn read_expected(home: &Path) -> Value {
        let text = fs::read_to_string(home.join("EXPECTED.json")).expect("EXPECTED.json");
        serde_json::from_str(&text).expect("valid EXPECTED.json")
    }

    fn parse_iso(v: &Value) -> Option<DateTime<Utc>> {
        parse_ts(v)
    }

    /// Relative paths (from `home`) of every file under a directory artifact.
    fn expand_dir(p: &Path, home: &Path) -> Vec<String> {
        let mut files: Vec<String> = WalkDir::new(p)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .map(|e| {
                e.path()
                    .strip_prefix(home)
                    .expect("under home")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn scan_matches_expected_json() {
        let home = fixture_home();
        let expected = read_expected(&home);
        let sessions = GrokAdapter::new(&home).scan().expect("scan");
        let exp_sessions = expected["sessions"].as_array().expect("sessions");
        assert_eq!(sessions.len(), exp_sessions.len());

        for exp in exp_sessions {
            let id = exp["id"].as_str().expect("id");
            let s = sessions
                .iter()
                .find(|s| s.id == id)
                .unwrap_or_else(|| panic!("missing session {id}"));
            assert_eq!(s.agent, AgentKind::Grok);
            assert_eq!(s.title.as_deref(), exp["title"].as_str(), "title of {id}");
            assert_eq!(s.project, exp["project"].as_str().map(PathBuf::from));
            assert_eq!(s.created, parse_iso(&exp["created"]), "created of {id}");
            assert_eq!(
                s.last_active,
                parse_iso(&exp["last_active"]),
                "last_active of {id}"
            );
            assert_eq!(s.message_count, exp["message_count"].as_u64());

            let dirs: Vec<&Path> = s
                .artifacts
                .iter()
                .filter_map(|a| match a {
                    Artifact::Dir(p) => Some(p.as_path()),
                    _ => None,
                })
                .collect();
            assert_eq!(dirs.len(), 1, "one session dir for {id}");
            let mut want: Vec<String> = exp["artifacts_rel_to_home"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            want.sort();
            assert_eq!(expand_dir(dirs[0], &home), want, "files of {id}");
            let dir_bytes = s
                .artifacts
                .iter()
                .find_map(|a| match a {
                    Artifact::Dir(_) => Some(a.own_bytes()),
                    _ => None,
                })
                .unwrap();
            assert_eq!(dir_bytes, exp["dir_bytes"].as_u64().unwrap(), "dir bytes of {id}");

            let has_index_row = s
                .artifacts
                .iter()
                .any(|a| matches!(a, Artifact::DbRows { .. }));
            let want_index = exp["index_rows"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty());
            assert_eq!(has_index_row, want_index, "index row of {id}");
            assert_eq!(s.bytes, dir_bytes, "total bytes of {id}");
        }
    }

    #[test]
    fn shared_files_reference_session_id() {
        let home = fixture_home();
        let sessions = GrokAdapter::new(&home).scan().expect("scan");
        let s1 = sessions
            .iter()
            .find(|s| s.id == "01a10000-0000-7000-8000-000000000001")
            .expect("s1");
        let history = s1.artifacts.iter().find_map(|a| match a {
            Artifact::JsonlLines { file, field, value } => Some((file, field, value)),
            _ => None,
        });
        let (file, field, value) = history.expect("prompt_history JsonlLines");
        assert!(file.ends_with(PROMPT_HISTORY));
        assert_eq!(field, "session_id");
        assert_eq!(value, &s1.id);
        assert!(!s1
            .artifacts
            .iter()
            .any(|a| matches!(a, Artifact::JsonArrayEntry { .. })));

        let s2 = sessions
            .iter()
            .find(|s| s.id == "01a10000-0000-7000-8000-000000000002")
            .expect("s2");
        assert!(s2.artifacts.iter().any(|a| matches!(
            a,
            Artifact::JsonArrayEntry { value, .. } if value == &s2.id
        )));
        for s in [s1, s2] {
            let index_row = s.artifacts.iter().find_map(|a| match a {
                Artifact::DbRows { deletes, .. } => deletes.first(),
                _ => None,
            });
            let d = index_row.expect("index row");
            assert_eq!((d.table.as_str(), d.where_col.as_str()), ("session_docs", "session_id"));
            assert_eq!(d.value, s.id);
        }
    }

    #[test]
    fn subagent_resume_session_is_listed_with_its_worktree_cwd() {
        let home = fixture_home();
        let sessions = GrokAdapter::new(&home).scan().expect("scan");
        let s3 = sessions
            .iter()
            .find(|s| s.id == "01a10000-0000-7000-8000-000000000003")
            .expect("s3");
        assert_eq!(
            s3.project,
            Some(PathBuf::from(
                "/Users/fixture/.grok/worktrees/alpha-app/subagent-01a10000-0000-7000-8000-000000000001"
            ))
        );
        assert!(!s3
            .artifacts
            .iter()
            .any(|a| matches!(a, Artifact::DbRows { .. })));
    }

    #[test]
    fn preview_returns_user_and_assistant_text_in_order() {
        let home = fixture_home();
        let adapter = GrokAdapter::new(&home);
        let s = adapter
            .scan()
            .expect("scan")
            .into_iter()
            .find(|s| s.id == "01a10000-0000-7000-8000-000000000001")
            .expect("session");
        let msgs = adapter.preview(&s, 10).expect("preview");
        let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);
        assert_eq!(msgs[0].text, "Refactor the config loader to read TOML.");
        assert_eq!(adapter.preview(&s, 1).expect("preview").len(), 1);
    }

    #[test]
    fn fixture_registry_pid_is_not_live() {
        assert!(GrokAdapter::new(&fixture_home())
            .live_session_ids()
            .is_empty());
    }

    #[test]
    fn current_process_registry_entry_is_live() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pid = std::process::id();
        let mut sys = System::new();
        sys.refresh_processes(ProcessesToUpdate::Some(&[SysPid::from_u32(pid)]), true);
        let start = sys
            .process(SysPid::from_u32(pid))
            .expect("self process")
            .start_time();
        let grok = tmp.path().join(".grok");
        fs::create_dir_all(&grok).expect("mkdir");
        let registry = format!(
            r#"[{{"session_id":"live-self","pid":{pid},"pid_start":{},"cwd":"/x","opened_at":"2026-01-01T00:00:00Z"}}]"#,
            start * 1_000_000
        );
        fs::write(grok.join(ACTIVE_FILE), registry).expect("write registry");
        assert_eq!(
            GrokAdapter::new(tmp.path()).live_session_ids(),
            vec!["live-self".to_owned()]
        );
    }

    #[test]
    fn index_connection_rejects_writes() {
        let index = fixture_home().join(".grok/sessions/session_search.sqlite");
        let conn = open_readonly(&index).expect("open read-only");
        assert!(conn.execute_batch("CREATE TABLE sz_probe(x)").is_err());
    }

    #[test]
    fn percent_decode_bucket_names() {
        assert_eq!(
            percent_decode("%2FUsers%2Ffixture%2Fwork%2Falpha-app"),
            "/Users/fixture/work/alpha-app"
        );
        assert_eq!(percent_decode("plain%"), "plain%");
    }

    #[test]
    #[ignore]
    fn live_scan() {
        let home = dirs::home_dir().expect("home");
        let started = std::time::Instant::now();
        let sessions = GrokAdapter::new(&home).scan().expect("live scan");
        let elapsed = started.elapsed();
        let total: u64 = sessions.iter().map(|s| s.bytes).sum();
        println!(
            "grok live: {} sessions, {} bytes, scan {:?}",
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
