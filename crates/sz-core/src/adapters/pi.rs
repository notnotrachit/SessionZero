//! Pi coding agent adapter. Format notes: docs/formats/pi.md.
//!
//! Session files live under `~/.pi/agent/sessions/<cwd-dir>/`:
//! - `<ts>_<uuid>.jsonl` is a top-level session. Its header may name a `parentSession` (a branch).
//! - `<ts>_<uuid>/forks/<ts>_<uuid>.jsonl` is a fork. Its parent is the directory it sits under.
//! - `<ts>_<uuid>/<child-uuid>/run-0/session.jsonl` is a subagent run. Its parent is the directory
//!   two levels up.
//!
//! Per-session state lives in `~/.pi/agent/state/workspace-history/workspaces/<hash>/sessions/<id>/`.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
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
/// Lines longer than this are not parsed as JSON. Only their timestamp is read.
const MAX_PARSE_LINE: usize = 16 * 1024 * 1024;
const READ_BUF: usize = 256 * 1024;

pub struct PiAdapter {
    home: PathBuf,
}

impl PiAdapter {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }

    fn agent_dir(&self) -> PathBuf {
        self.home.join(".pi/agent")
    }

    fn sessions_dir(&self) -> PathBuf {
        self.agent_dir().join("sessions")
    }
}

impl Adapter for PiAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Pi
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![
            self.sessions_dir(),
            self.agent_dir()
                .join("state/workspace-history/workspaces"),
        ]
    }

    fn scan(&self) -> anyhow::Result<Vec<Session>> {
        let state = state_session_dirs(&self.agent_dir());
        let files = session_files(&self.sessions_dir());
        let cache = ParseCache::for_home(&self.home, "pi");
        // `par_iter().collect()` keeps the input order, so the first file that claims an id wins.
        let parsed: Vec<Session> = files
            .par_iter()
            .filter_map(|f| build_session(f, &state, &cache))
            .collect();
        if let Err(err) = cache.save() {
            eprintln!("sz: pi parse cache not saved: {err:#}");
        }
        let mut seen = HashSet::new();
        let mut sessions: Vec<Session> = parsed
            .into_iter()
            .filter(|s| seen.insert(s.id.clone()))
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
        if max == 0 {
            return Ok(out);
        }
        let Some(path) = s.artifacts.iter().find_map(|a| match a {
            Artifact::File(p) => Some(p),
            _ => None,
        }) else {
            return Ok(out);
        };
        let Ok(reader) = open(path) else {
            return Ok(out);
        };
        let _ = each_line(reader, |line| {
            if line.len() > MAX_PARSE_LINE || !line.contains("\"message\"") {
                return true;
            }
            let Ok(rec) = serde_json::from_str::<Record>(line) else {
                return true;
            };
            if rec.kind.as_deref() != Some("message") {
                return true;
            }
            let Some(msg) = rec.message else {
                return true;
            };
            let role = match msg.role.as_deref() {
                Some("user") => "user",
                Some("assistant") => "assistant",
                _ => return true,
            };
            let text = message_text(msg.content.as_ref());
            let text = text.trim();
            if text.is_empty() {
                return true;
            }
            out.push(PreviewMsg {
                role: role.to_string(),
                text: truncate(text, PREVIEW_TEXT_MAX),
                ts: rec.timestamp.as_deref().and_then(parse_ts),
            });
            out.len() < max
        });
        Ok(out)
    }

    fn process_names(&self) -> &'static [&'static str] {
        &["pi", "pi-coding-agent"]
    }
}

/// Where a session file sits in the tree. Decides its parent and its own artifacts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Layout {
    Top,
    Fork,
    Subagent,
}

struct SessionFile {
    path: PathBuf,
    layout: Layout,
    /// Parent id from the directory layout (forks and subagent runs). None for top-level files.
    dir_parent: Option<String>,
}

/// Fields read from one session file. Cached per file by path, size and mtime.
#[derive(Default, Clone, Serialize, Deserialize)]
struct Meta {
    id: Option<String>,
    project: Option<String>,
    created: Option<DateTime<Utc>>,
    last_active: Option<DateTime<Utc>>,
    /// `parentSession` from the header: the parent file's path.
    parent_session: Option<String>,
    /// Latest non-empty `session_info` name, normalized.
    name: Option<String>,
    /// Normalized text of the first user message.
    first_user: Option<String>,
    /// User and assistant message entries. Tool results and custom entries are not counted.
    messages: u64,
}

/// One JSONL line. Only the fields used here are read. Unknown fields are ignored.
#[derive(Deserialize)]
struct Record {
    #[serde(rename = "type")]
    kind: Option<String>,
    /// Session id, on the `session` header only.
    id: Option<String>,
    cwd: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "parentSession")]
    parent_session: Option<String>,
    /// Session name, on `session_info` entries only.
    name: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    role: Option<String>,
    content: Option<Value>,
}

/// Maps each session id that has a `workspace-history` state directory to that directory.
fn state_session_dirs(agent: &Path) -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();
    let Ok(workspaces) = std::fs::read_dir(agent.join("state/workspace-history/workspaces")) else {
        return map;
    };
    for ws in workspaces.flatten() {
        let Ok(sessions) = std::fs::read_dir(ws.path().join("sessions")) else {
            continue;
        };
        for entry in sessions.flatten() {
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_dir && find_uuid(&name) == Some(name.as_str()) {
                map.entry(name).or_insert_with(|| entry.path());
            }
        }
    }
    map
}

/// Every session file under `sessions/`, classified by its position. Sorted by path so results
/// are deterministic.
fn session_files(root: &Path) -> Vec<SessionFile> {
    let mut out = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .max_depth(5)
        .sort_by_file_name()
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let parts: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let classified = match parts.as_slice() {
            [_, file] if file.ends_with(".jsonl") => Some((Layout::Top, None)),
            [_, stem, forks, file] if forks == "forks" && file.ends_with(".jsonl") => {
                Some((Layout::Fork, find_uuid(stem).map(str::to_string)))
            }
            [_, stem, _, run, file]
                if run.starts_with("run-") && file == "session.jsonl" =>
            {
                Some((Layout::Subagent, find_uuid(stem).map(str::to_string)))
            }
            _ => None,
        };
        if let Some((layout, dir_parent)) = classified {
            out.push(SessionFile {
                path: entry.into_path(),
                layout,
                dir_parent,
            });
        }
    }
    out
}

fn build_session(
    file: &SessionFile,
    state: &HashMap<String, PathBuf>,
    cache: &ParseCache,
) -> Option<Session> {
    let meta = cache.get_or_parse(&file.path, || scan_session(&file.path))?;
    let id = meta.id.clone().or_else(|| {
        file.path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(find_uuid)
            .map(str::to_string)
    })?;

    let mut session = Session::new(AgentKind::Pi, id.clone());
    session.title = meta.name.clone().or_else(|| meta.first_user.clone());
    session.project = meta.project.clone().map(PathBuf::from);
    let fallback = mtime(&file.path);
    session.created = meta.created.or(fallback);
    session.last_active = meta.last_active.or(fallback);
    session.message_count = Some(meta.messages);
    session.parent_id = file
        .dir_parent
        .clone()
        .or_else(|| {
            meta.parent_session
                .as_deref()
                .and_then(file_name_uuid)
                .map(str::to_string)
        })
        .filter(|p| *p != id);

    session.artifacts.push(Artifact::File(file.path.clone()));
    if file.layout == Layout::Subagent {
        // `<parent>/<child-uuid>/run-0/session.jsonl`: the child directory is the run's own.
        if let Some(child_dir) = file.path.parent().and_then(Path::parent) {
            session
                .artifacts
                .push(Artifact::Dir(child_dir.to_path_buf()));
        }
    }
    if let Some(dir) = state.get(&id) {
        session.artifacts.push(Artifact::Dir(dir.clone()));
    }
    session.recompute_bytes();
    Some(session)
}

/// Reads one session file. Malformed lines are skipped. Returns None only when the file cannot be
/// opened.
fn scan_session(path: &Path) -> Option<Meta> {
    let reader = open(path).ok()?;
    let mut meta = Meta::default();
    let _ = each_line(reader, |line| {
        if line.len() > MAX_PARSE_LINE {
            if let Some(ts) = extract_timestamp(line) {
                bump(&mut meta.last_active, ts);
            }
            return true;
        }
        let Ok(rec) = serde_json::from_str::<Record>(line) else {
            return true;
        };
        if let Some(ts) = rec.timestamp.as_deref().and_then(parse_ts) {
            bump(&mut meta.last_active, ts);
        }
        match rec.kind.as_deref() {
            Some("session") => {
                if meta.id.is_none() {
                    meta.id = rec.id.filter(|s| !s.trim().is_empty());
                    meta.project = non_empty(rec.cwd);
                    meta.created = rec.timestamp.as_deref().and_then(parse_ts);
                    meta.parent_session = non_empty(rec.parent_session);
                }
            }
            Some("session_info") => {
                if let Some(name) = rec.name.as_deref().and_then(normalize_title) {
                    meta.name = Some(name);
                }
            }
            Some("message") => {
                let Some(msg) = rec.message else {
                    return true;
                };
                match msg.role.as_deref() {
                    Some("user") => {
                        meta.messages += 1;
                        if meta.first_user.is_none() {
                            meta.first_user = normalize_title(&message_text(msg.content.as_ref()));
                        }
                    }
                    Some("assistant") => meta.messages += 1,
                    _ => {}
                }
            }
            _ => {}
        }
        true
    });
    Some(meta)
}

/// Calls `f` for each non-empty trimmed line. Invalid UTF-8 is replaced. `f` returns false to stop
/// early. A read error ends the loop and is returned.
fn each_line(mut reader: impl BufRead, mut f: impl FnMut(&str) -> bool) -> io::Result<()> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&buf);
        let line = text.trim();
        if !line.is_empty() && !f(line) {
            break;
        }
    }
    Ok(())
}

fn open(path: &Path) -> io::Result<BufReader<File>> {
    Ok(BufReader::with_capacity(READ_BUF, File::open(path)?))
}

/// Joins the text blocks of a message. Plain string content is returned as it is.
fn message_text(content: Option<&Value>) -> String {
    match content {
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

/// Collapses whitespace and truncates a title. Returns None when nothing is left.
fn normalize_title(raw: &str) -> Option<String> {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then(|| truncate(&collapsed, TITLE_MAX))
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.trim().is_empty())
}

fn bump(slot: &mut Option<DateTime<Utc>>, ts: DateTime<Utc>) {
    *slot = Some(slot.map_or(ts, |cur| cur.max(ts)));
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Top-level `"timestamp":"..."` of a JSON line, read without a full parse.
fn extract_timestamp(line: &str) -> Option<DateTime<Utc>> {
    const KEY: &str = "\"timestamp\":\"";
    let start = line.find(KEY)? + KEY.len();
    let rest = &line[start..];
    parse_ts(&rest[..rest.find('"')?])
}

/// The file name of a session path, reduced to its session UUID.
fn file_name_uuid(path: &str) -> Option<&str> {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(find_uuid)
}

/// The first UUID-shaped run in `s`.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_home() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/pi")
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

        let mut sessions = PiAdapter::new(&home).scan().expect("scan");
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
            assert_eq!(
                s.parent_id.as_deref(),
                e["parent_id"].as_str(),
                "parent_id of {id}"
            );

            let mut files: Vec<String> = s
                .artifacts
                .iter()
                .map(|a| match a {
                    Artifact::File(p) | Artifact::Dir(p) => rel(&home, p),
                    other => panic!("unexpected artifact {other:?} for {id}"),
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
            assert_eq!(files, want, "artifacts of {id}");
        }
    }

    #[test]
    fn preview_keeps_user_and_assistant_text_only() {
        let home = fixture_home();
        let adapter = PiAdapter::new(&home);
        let sessions = adapter.scan().expect("scan");
        let alpha = sessions
            .iter()
            .find(|s| s.id == "0198aaaa-0000-7000-8000-000000000001")
            .expect("alpha");

        let msgs = adapter.preview(alpha, 10).expect("preview");
        let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
        assert_eq!(msgs[0].text, "Split the parser module into two files.");
        assert!(msgs[0].ts.is_some());

        assert_eq!(adapter.preview(alpha, 2).expect("preview").len(), 2);
        assert!(adapter.preview(alpha, 0).expect("preview").is_empty());
    }

    #[test]
    fn subagent_run_is_found_under_its_parent_directory() {
        let files = session_files(&fixture_home().join(".pi/agent/sessions"));
        let child = files
            .iter()
            .find(|f| f.layout == Layout::Subagent)
            .expect("subagent file");
        assert_eq!(
            child.dir_parent.as_deref(),
            Some("0198aaaa-0000-7000-8000-000000000001")
        );
        assert_eq!(files.iter().filter(|f| f.layout == Layout::Top).count(), 2);
    }

    #[test]
    fn parent_uuid_comes_from_the_parent_file_name() {
        assert_eq!(
            file_name_uuid(
                "/Users/x/.pi/agent/sessions/--a--/2026-09-12T08-22-38-567Z_01a094b6-1ee7-754a-b9a1-8b1fcf6a26fe.jsonl"
            ),
            Some("01a094b6-1ee7-754a-b9a1-8b1fcf6a26fe")
        );
        assert_eq!(find_uuid("no-uuid-here"), None);
    }

    #[test]
    fn normalize_title_collapses_and_truncates() {
        assert_eq!(normalize_title("  a \n b  ").as_deref(), Some("a b"));
        assert_eq!(normalize_title("   "), None);
        let long = "x".repeat(300);
        assert_eq!(normalize_title(&long).expect("title").chars().count(), TITLE_MAX);
    }

    /// Scans the real `~/.pi/agent` read-only. Run with `SZ_DATA_DIR` pointing at a scratch
    /// directory so the parse cache is not written under the real home:
    /// `SZ_DATA_DIR=$KIROCREW_SCRATCH/szdata cargo test -p sz-core live_scan -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_scan() {
        let home = dirs::home_dir().expect("home");
        let adapter = PiAdapter::new(&home);
        let started = std::time::Instant::now();
        let sessions = adapter.scan().expect("scan");
        let elapsed = started.elapsed();
        let bytes: u64 = sessions.iter().map(|s| s.bytes).sum();
        println!(
            "sessions={} bytes={} time={:?}",
            sessions.len(),
            bytes,
            elapsed
        );
        let with_parent = sessions.iter().filter(|s| s.parent_id.is_some()).count();
        println!("with parent_id={with_parent}");
        for s in sessions.iter().take(3) {
            println!(
                "sample id={} project={:?} messages={:?} bytes={} artifacts={}",
                s.id,
                s.project,
                s.message_count,
                s.bytes,
                s.artifacts.len()
            );
        }
    }
}
