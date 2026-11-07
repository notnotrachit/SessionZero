//! VS Code built-in chat (Copilot Chat agent mode) sessions, from the `User` directory of VS Code,
//! VS Code Insiders and VSCodium. Layout and sources: `docs/formats/vscode.md`.
//!
//! Sessions are read from the session files only. The `chat.ChatSessionStore.index` entries in
//! `state.vscdb` are derived metadata, so this adapter does not open that database. Removing a
//! session never edits it.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use crate::adapter::Adapter;
use crate::model::{AgentKind, Artifact, PreviewMsg, Session};
use crate::util::{mtime, normalize_project, truncate};

const TITLE_MAX: usize = 120;
const TEXT_MAX: usize = 2000;

pub struct VscodeAdapter {
    home: PathBuf,
}

impl VscodeAdapter {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }

    /// Sessions of one `User` directory: each workspace storage folder, then the empty-window
    /// sessions that VS Code keeps under `globalStorage`.
    fn scan_user_dir(&self, user: &Path, out: &mut Vec<Session>) {
        for ws in sorted_entries(&user.join("workspaceStorage")) {
            if !ws.is_dir() {
                continue;
            }
            let project = read_json(&ws.join("workspace.json")).and_then(|v| {
                v.get("folder")
                    .and_then(Value::as_str)
                    .and_then(folder_uri_to_path)
            });
            scan_session_dir(
                &ws.join("chatSessions"),
                Some(&ws.join("chatEditingSessions")),
                project,
                &self.home,
                out,
            );
        }
        scan_session_dir(
            &user.join("globalStorage/emptyWindowChatSessions"),
            None,
            None,
            &self.home,
            out,
        );
    }
}

impl Adapter for VscodeAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Vscode
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![
            self.home.join("Library/Application Support/Code/User"),
            self.home
                .join("Library/Application Support/Code - Insiders/User"),
            self.home.join("Library/Application Support/VSCodium/User"),
            self.home.join(".config/Code/User"),
        ]
    }

    fn scan(&self) -> anyhow::Result<Vec<Session>> {
        let mut sessions = Vec::new();
        for user in self.roots() {
            if user.is_dir() {
                self.scan_user_dir(&user, &mut sessions);
            }
        }
        sessions.sort_by(|a, b| {
            b.last_active
                .cmp(&a.last_active)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(sessions)
    }

    fn preview(&self, s: &Session, max: usize) -> anyhow::Result<Vec<PreviewMsg>> {
        let files: Vec<PathBuf> = s
            .artifacts
            .iter()
            .filter_map(|a| match a {
                Artifact::File(p) => Some(p.clone()),
                _ => None,
            })
            .collect();
        let Some(doc) = load_preferred(&files) else {
            return Ok(Vec::new());
        };
        Ok(transcript(&doc)
            .into_iter()
            .take(max)
            .map(|(role, text, ts)| PreviewMsg {
                role,
                text: truncate(&text, TEXT_MAX),
                ts,
            })
            .collect())
    }

    fn process_names(&self) -> &'static [&'static str] {
        &["Code", "Electron", "code", "Code - Insiders", "VSCodium"]
    }
}

// ---------------------------------------------------------------------------------------------
// Session files
// ---------------------------------------------------------------------------------------------

/// Scans one `chatSessions` (or `emptyWindowChatSessions`) directory. A `<id>.json` and a
/// `<id>.jsonl` with the same stem are one session: both files are its artifacts.
fn scan_session_dir(
    dir: &Path,
    edits_root: Option<&Path>,
    project: Option<PathBuf>,
    home: &Path,
    out: &mut Vec<Session>,
) {
    let mut groups: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for path in sorted_entries(dir) {
        if !path.is_file() || !is_session_file(&path) {
            continue;
        }
        let Some(id) = stem(&path) else { continue };
        groups.entry(id).or_default().push(path);
    }
    for (id, files) in groups {
        let mut project = project.clone();
        normalize_project(&mut project, Some(home));
        let edits = edits_root.map(|root| root.join(&id)).filter(|p| p.is_dir());
        out.push(build_session(id, &files, edits, project));
    }
}

fn build_session(
    id: String,
    files: &[PathBuf],
    edits: Option<PathBuf>,
    project: Option<PathBuf>,
) -> Session {
    // A file that cannot be parsed still lists, so the user can remove it. It has no title or
    // counts, and its bytes still count.
    let doc = load_preferred(files).unwrap_or(Value::Null);
    let summary = summarize(&doc);

    let mut s = Session::new(AgentKind::Vscode, id);
    s.title = summary.title;
    s.project = project;
    s.created = summary.created;
    s.last_active = summary
        .last_active
        .or(summary.created)
        .or_else(|| files.first().and_then(|f| mtime(f)));
    s.message_count = summary.message_count;
    s.artifacts = files.iter().cloned().map(Artifact::File).collect();
    if let Some(dir) = edits {
        s.artifacts.push(Artifact::Dir(dir));
    }
    s.recompute_bytes();
    s
}

fn is_session_file(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()),
        Some("json" | "jsonl")
    )
}

/// Reads the session content from the `.jsonl` log when there is one, else from the `.json`.
/// The log is the newer format and replaces the `.json` for the same session.
fn load_preferred(files: &[PathBuf]) -> Option<Value> {
    let mut ordered: Vec<&PathBuf> = files.iter().collect();
    ordered.sort_by_key(|p| p.extension().and_then(|e| e.to_str()) != Some("jsonl"));
    ordered.into_iter().find_map(|p| load_file(p))
}

fn load_file(path: &Path) -> Option<Value> {
    if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
        read_jsonl(path)
    } else {
        read_json(path)
    }
}

/// Replays an operation log. Each line is `{"kind": 0|1|2|3, ...}`:
/// - 0 `{"v": state}`: full initial state, valid as the first entry.
/// - 1 `{"k": path, "v": value}`: set the value at `path`. A missing `v` deletes the key.
/// - 2 `{"k": path, "i"?: n, "v"?: [items]}`: truncate the array at `path` to `n` items when `i`
///   is set, then append `items`.
/// - 3 `{"k": path}`: delete the key at `path`.
///
/// Lines that do not parse are skipped. An unreadable file returns `None`.
fn read_jsonl(path: &Path) -> Option<Value> {
    let reader = BufReader::new(File::open(path).ok()?);
    let mut state = None;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        apply_entry(&mut state, &entry);
    }
    state
}

/// Applies one log entry. Returns `None` when the entry cannot be applied (no initial state yet,
/// a missing parent, or an unknown kind). Such an entry is skipped.
fn apply_entry(state: &mut Option<Value>, entry: &Value) -> Option<()> {
    let kind = entry.get("kind")?.as_u64()?;
    if kind == 0 {
        *state = Some(entry.get("v")?.clone());
        return Some(());
    }
    let root = state.as_mut()?;
    let path = entry.get("k")?.as_array()?;
    let (last, parent_path) = path.split_last()?;
    let parent = node_mut(root, parent_path)?;
    match kind {
        1 => match entry.get("v") {
            Some(v) => put(parent, last, v.clone()),
            None => remove(parent, last),
        },
        2 => {
            let is_array = matches!(child_mut(&mut *parent, last), Some(Value::Array(_)));
            if !is_array {
                put(parent, last, Value::Array(Vec::new()))?;
            }
            let Some(Value::Array(items)) = child_mut(parent, last) else {
                return None;
            };
            if let Some(i) = entry.get("i").and_then(Value::as_u64) {
                items.truncate(usize::try_from(i).ok()?);
            }
            if let Some(values) = entry.get("v").and_then(Value::as_array) {
                items.extend(values.iter().cloned());
            }
            Some(())
        }
        3 => remove(parent, last),
        _ => None,
    }
}

fn seg_key(seg: &Value) -> Option<String> {
    match seg {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn seg_index(seg: &Value) -> Option<usize> {
    match seg {
        Value::Number(n) => n.as_u64().and_then(|i| usize::try_from(i).ok()),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn child_mut<'a>(v: &'a mut Value, seg: &Value) -> Option<&'a mut Value> {
    match v {
        Value::Object(map) => map.get_mut(&seg_key(seg)?),
        Value::Array(items) => items.get_mut(seg_index(seg)?),
        _ => None,
    }
}

fn node_mut<'a>(root: &'a mut Value, path: &[Value]) -> Option<&'a mut Value> {
    let mut cur = root;
    for seg in path {
        cur = child_mut(cur, seg)?;
    }
    Some(cur)
}

fn put(parent: &mut Value, seg: &Value, value: Value) -> Option<()> {
    match parent {
        Value::Object(map) => {
            map.insert(seg_key(seg)?, value);
        }
        Value::Array(items) => {
            let i = seg_index(seg)?;
            if i >= items.len() {
                items.resize(i + 1, Value::Null);
            }
            items[i] = value;
        }
        _ => return None,
    }
    Some(())
}

/// Deletes an object key. An array slot becomes `null`, as a JavaScript `undefined` would
/// serialize inside an array.
fn remove(parent: &mut Value, seg: &Value) -> Option<()> {
    match parent {
        Value::Object(map) => {
            map.remove(&seg_key(seg)?);
        }
        Value::Array(items) => {
            let i = seg_index(seg)?;
            if let Some(slot) = items.get_mut(i) {
                *slot = Value::Null;
            }
        }
        _ => return None,
    }
    Some(())
}

// ---------------------------------------------------------------------------------------------
// Session content
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Summary {
    title: Option<String>,
    created: Option<DateTime<Utc>>,
    last_active: Option<DateTime<Utc>>,
    message_count: Option<u64>,
}

fn summarize(doc: &Value) -> Summary {
    let requests = requests_of(doc);
    let created = doc
        .get("creationDate")
        .and_then(Value::as_i64)
        .and_then(ms_to_utc)
        .or_else(|| {
            requests
                .first()
                .and_then(|r| r.get("timestamp"))
                .and_then(Value::as_i64)
                .and_then(ms_to_utc)
        });
    let last_active = requests
        .last()
        .and_then(|r| {
            r.get("responseTimestamp")
                .and_then(Value::as_i64)
                .or_else(|| r.get("timestamp").and_then(Value::as_i64))
        })
        .and_then(ms_to_utc);
    let title = doc
        .get("customTitle")
        .and_then(Value::as_str)
        .and_then(first_line)
        .or_else(|| {
            requests
                .iter()
                .find_map(|r| request_text(r))
                .and_then(first_line)
        });
    // One request is one user message plus one assistant response.
    let message_count = doc.is_object().then(|| requests.len() as u64 * 2);
    Summary {
        title,
        created,
        last_active,
        message_count,
    }
}

fn requests_of(doc: &Value) -> &[Value] {
    match doc.get("requests").and_then(Value::as_array) {
        Some(items) => items.as_slice(),
        None => &[],
    }
}

fn request_text(request: &Value) -> Option<&str> {
    request
        .pointer("/message/text")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
}

/// Markdown in a response part. A serialized `MarkdownString` is an object with `value`.
fn markdown_text(v: &Value) -> Option<&str> {
    v.as_str()
        .or_else(|| v.get("value").and_then(Value::as_str))
}

/// Text of one response part. Parts with a `kind` other than markdown (tool invocations,
/// progress, edits) carry no reply text and return `None`.
fn response_text(part: &Value) -> Option<&str> {
    match part.get("kind").and_then(Value::as_str) {
        None => markdown_text(part),
        Some("markdownContent") => part.get("content").and_then(markdown_text),
        Some(_) => None,
    }
}

/// (role, text, timestamp) per message, in conversation order. Each request yields its user
/// text and then one assistant message that joins its markdown parts.
fn transcript(doc: &Value) -> Vec<(String, String, Option<DateTime<Utc>>)> {
    let mut out = Vec::new();
    for request in requests_of(doc) {
        let request_ts = request
            .get("timestamp")
            .and_then(Value::as_i64)
            .and_then(ms_to_utc);
        if let Some(text) = request_text(request) {
            out.push(("user".to_owned(), text.to_owned(), request_ts));
        }
        let reply: Vec<&str> = request
            .get("response")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(response_text)
            .filter(|t| !t.trim().is_empty())
            .collect();
        if !reply.is_empty() {
            let reply_ts = request
                .get("responseTimestamp")
                .and_then(Value::as_i64)
                .and_then(ms_to_utc)
                .or(request_ts);
            out.push(("assistant".to_owned(), reply.join("\n"), reply_ts));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// `file:///abs/path` (percent-encoded) to a path. Any other scheme or an authority such as a
/// remote host returns `None`, because the path would not be on this machine.
fn folder_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    if !rest.starts_with('/') {
        return None;
    }
    let decoded = percent_decode(rest);
    // Windows drive letter: "/c:/Users/x" becomes "c:/Users/x".
    let bytes = decoded.as_bytes();
    let path = if bytes.len() >= 3 && bytes[2] == b':' && bytes[1].is_ascii_alphabetic() {
        &decoded[1..]
    } else {
        decoded.as_str()
    };
    Some(PathBuf::from(path))
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

/// First non-empty line, trimmed and truncated to the title limit.
fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| truncate(l, TITLE_MAX))
}

fn ms_to_utc(ms: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms).single()
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

fn stem(p: &Path) -> Option<String> {
    p.file_stem()?.to_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn fixture_home() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/vscode")
    }

    fn expected() -> Value {
        read_json(&fixture_home().join("EXPECTED.json")).expect("EXPECTED.json")
    }

    fn iso(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("iso")
            .with_timezone(&Utc)
    }

    fn find<'a>(sessions: &'a [Session], id: &str) -> &'a Session {
        sessions
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("missing session {id}"))
    }

    #[test]
    fn scan_matches_expected_fixture() {
        let sessions = VscodeAdapter::new(&fixture_home()).scan().expect("scan");
        let exp = expected();
        let expected_sessions = exp["sessions"].as_array().expect("sessions");
        assert_eq!(sessions.len(), expected_sessions.len(), "{sessions:#?}");

        for e in expected_sessions {
            let id = e["id"].as_str().expect("id");
            let s = find(&sessions, id);
            assert_eq!(s.agent, AgentKind::Vscode);
            assert_eq!(s.title.as_deref(), e["title"].as_str(), "title {id}");
            assert_eq!(
                s.project.as_ref().map(|p| p.to_string_lossy().into_owned()),
                e["project"].as_str().map(str::to_owned),
                "project {id}"
            );
            assert_eq!(s.message_count, e["message_count"].as_u64(), "count {id}");
            assert_eq!(s.created, e["created"].as_str().map(iso), "created {id}");
            assert_eq!(
                s.last_active,
                e["last_active"].as_str().map(iso),
                "last {id}"
            );
            assert_eq!(s.bytes, e["bytes"].as_u64().expect("bytes"), "bytes {id}");
        }
    }

    #[test]
    fn workspace_session_owns_edit_directory() {
        let sessions = VscodeAdapter::new(&fixture_home()).scan().expect("scan");
        let s = find(&sessions, "11111111-aaaa-4bbb-8ccc-000000000001");
        assert!(
            s.artifacts
                .iter()
                .any(|a| matches!(a, Artifact::Dir(d) if d.ends_with("chatEditingSessions/11111111-aaaa-4bbb-8ccc-000000000001"))),
            "{:?}",
            s.artifacts
        );
    }

    #[test]
    fn preview_joins_reply_parts_and_skips_tool_invocations() {
        let sessions = VscodeAdapter::new(&fixture_home()).scan().expect("scan");
        let s = find(&sessions, "11111111-aaaa-4bbb-8ccc-000000000001");
        let msgs = VscodeAdapter::new(&fixture_home())
            .preview(s, 10)
            .expect("preview");
        let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);
        assert_eq!(msgs[1].text, "The race is in the mock setup.");
        assert!(msgs[0].ts.is_some());

        let capped = VscodeAdapter::new(&fixture_home())
            .preview(s, 1)
            .expect("preview");
        assert_eq!(capped.len(), 1);
    }

    #[test]
    fn jsonl_log_replays_set_push_truncate_and_delete() {
        let sessions = VscodeAdapter::new(&fixture_home()).scan().expect("scan");
        let s = find(&sessions, "22222222-bbbb-4ccc-8ddd-000000000002");
        let msgs = VscodeAdapter::new(&fixture_home())
            .preview(s, 10)
            .expect("preview");
        // The truncate entry replaces the draft reply; the delete removes responderUsername.
        assert_eq!(msgs[1].text, "The CLI takes --json and --dry-run.");
        assert_eq!(msgs[3].text, "Done, --quiet added.");
        assert_eq!(s.title.as_deref(), Some("Beta: CLI flags"));
    }

    #[test]
    fn log_entries_apply_in_order() {
        let mut state = None;
        assert!(apply_entry(
            &mut state,
            &serde_json::json!({"kind": 1, "k": ["a"], "v": 1})
        )
        .is_none());
        apply_entry(
            &mut state,
            &serde_json::json!({"kind": 0, "v": {"list": [1, 2, 3]}}),
        );
        apply_entry(
            &mut state,
            &serde_json::json!({"kind": 2, "k": ["list"], "i": 1, "v": [9]}),
        );
        apply_entry(
            &mut state,
            &serde_json::json!({"kind": 1, "k": ["new", "deep"], "v": "x"}),
        );
        apply_entry(
            &mut state,
            &serde_json::json!({"kind": 3, "k": ["list", 0]}),
        );
        let v = state.expect("state");
        assert_eq!(v["list"], serde_json::json!([null, 9]));
        assert_eq!(v["new"], serde_json::json!(null));
    }

    #[test]
    fn push_creates_missing_array() {
        let mut state = Some(serde_json::json!({}));
        apply_entry(
            &mut state,
            &serde_json::json!({"kind": 2, "k": ["requests"], "v": [{"x": 1}]}),
        );
        assert_eq!(
            state.expect("state")["requests"],
            serde_json::json!([{"x": 1}])
        );
    }

    #[test]
    fn folder_uri_decodes_file_uris_only() {
        assert_eq!(
            folder_uri_to_path("file:///Users/fixture/work/beta%20tool"),
            Some(PathBuf::from("/Users/fixture/work/beta tool"))
        );
        assert_eq!(
            folder_uri_to_path("file:///c%3A/Users/x"),
            Some(PathBuf::from("c:/Users/x"))
        );
        assert_eq!(
            folder_uri_to_path("vscode-remote://ssh-remote+box/home/x"),
            None
        );
        assert_eq!(folder_uri_to_path("file://server/share/x"), None);
        assert_eq!(folder_uri_to_path("/plain/path"), None);
    }

    #[test]
    fn remote_workspace_has_no_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let user = dir.path().join(".config/Code/User");
        let ws = user.join("workspaceStorage/abc");
        fs::create_dir_all(ws.join("chatSessions")).expect("mkdir");
        fs::write(
            ws.join("workspace.json"),
            r#"{"folder":"vscode-remote://ssh-remote+box/home/x"}"#,
        )
        .expect("write");
        fs::write(
            ws.join("chatSessions/s1.json"),
            r#"{"version":3,"sessionId":"s1","requests":[]}"#,
        )
        .expect("write");
        let sessions = VscodeAdapter::new(dir.path()).scan().expect("scan");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].project, None);
        assert_eq!(sessions[0].message_count, Some(0));
    }

    #[test]
    fn json_and_jsonl_with_same_stem_are_one_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions_dir = dir
            .path()
            .join(".config/Code/User/globalStorage/emptyWindowChatSessions");
        fs::create_dir_all(&sessions_dir).expect("mkdir");
        fs::write(
            sessions_dir.join("s1.json"),
            r#"{"version":3,"sessionId":"s1","customTitle":"stale","requests":[]}"#,
        )
        .expect("write");
        fs::write(
            sessions_dir.join("s1.jsonl"),
            "{\"kind\":0,\"v\":{\"version\":3,\"sessionId\":\"s1\",\"customTitle\":\"fresh\",\"requests\":[]}}\n",
        )
        .expect("write");
        let sessions = VscodeAdapter::new(dir.path()).scan().expect("scan");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title.as_deref(), Some("fresh"));
        assert_eq!(sessions[0].artifacts.len(), 2);
    }

    #[test]
    fn unparseable_file_still_lists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions_dir = dir
            .path()
            .join(".config/Code/User/globalStorage/emptyWindowChatSessions");
        fs::create_dir_all(&sessions_dir).expect("mkdir");
        fs::write(sessions_dir.join("broken.json"), "{not json").expect("write");
        let sessions = VscodeAdapter::new(dir.path()).scan().expect("scan");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, None);
        assert_eq!(sessions[0].message_count, None);
        assert_eq!(sessions[0].bytes, 9);
    }

    #[test]
    fn missing_roots_scan_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(VscodeAdapter::new(dir.path())
            .scan()
            .expect("scan")
            .is_empty());
    }

    #[test]
    fn title_is_first_request_line_when_no_custom_title() {
        let doc = serde_json::json!({
            "requests": [
                {"message": {"text": "   "}},
                {"message": {"text": "\n  Explain the borrow error\nmore"}}
            ]
        });
        assert_eq!(
            summarize(&doc).title.as_deref(),
            Some("Explain the borrow error")
        );
    }

    #[test]
    fn long_titles_are_truncated() {
        let long = "x".repeat(300);
        let doc = serde_json::json!({"customTitle": long});
        let title = summarize(&doc).title.expect("title");
        assert_eq!(title.chars().count(), TITLE_MAX);
        assert!(title.ends_with('…'));
    }

    #[test]
    #[ignore]
    fn live_scan() {
        let home = dirs::home_dir().expect("home dir");
        let started = Instant::now();
        let sessions = VscodeAdapter::new(&home).scan().expect("scan");
        let elapsed = started.elapsed();
        let total: u64 = sessions.iter().map(|s| s.bytes).sum();
        println!(
            "vscode: {} sessions, {} bytes, scan {:?}",
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
