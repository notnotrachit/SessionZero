//! Safe, reversible deletion of session artifacts.
//!
//! `plan` decides what may be deleted. `Trash::execute` moves or rewrites each artifact and
//! records every change in the entry's `manifest.json`. `Trash::restore` replays those records
//! in reverse order. Every file this module writes goes through a temp file and a rename, so a
//! crash never leaves a half-written shared file behind.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration as StdDuration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use walkdir::WalkDir;

use crate::adapters::all_adapters;
use crate::model::{AgentKind, Artifact, Session};
use crate::paths;
use crate::util::{path_size, running_processes};

const MANIFEST: &str = "manifest.json";
const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// A session that `plan` refuses to delete.
#[derive(Debug, Clone)]
pub struct Blocked {
    pub key: String,
    pub reason: String,
}

/// Sessions cleared for deletion, with the bytes they would free.
#[derive(Debug, Clone)]
pub struct DeletePlan {
    pub sessions: Vec<Session>,
    pub bytes: u64,
    pub blocked: Vec<Blocked>,
}

/// Plans a delete. Unless `force`, a session of a running agent is blocked when it was active
/// recently, has unknown activity, or lives in a database. Paths that are relative, or that
/// contain the home directory, are always refused.
pub fn plan(home: &Path, sessions: Vec<Session>, force: bool) -> DeletePlan {
    let adapters = all_adapters(home);
    let mut running: HashMap<AgentKind, bool> = HashMap::new();
    let mut is_running = |kind: AgentKind| -> bool {
        *running.entry(kind).or_insert_with(|| {
            let names: Vec<&str> = adapters
                .iter()
                .filter(|a| a.kind() == kind)
                .flat_map(|a| a.process_names().iter().copied())
                .collect();
            running_processes(&names)
        })
    };
    let mut live_ids = |kind: AgentKind| -> Vec<String> {
        adapters
            .iter()
            .find(|a| a.kind() == kind)
            .map(|a| a.live_session_ids())
            .unwrap_or_default()
    };
    plan_with_live(home, sessions, force, &mut is_running, &mut live_ids)
}

/// `plan` with the running-agent check injected. Live session ids are treated as unknown.
pub fn plan_with(
    home: &Path,
    sessions: Vec<Session>,
    force: bool,
    is_running: &mut dyn FnMut(AgentKind) -> bool,
) -> DeletePlan {
    plan_with_live(home, sessions, force, is_running, &mut |_| Vec::new())
}

/// `plan` with both checks injected: the running-agent check and each agent's live session ids.
pub fn plan_with_live(
    home: &Path,
    sessions: Vec<Session>,
    force: bool,
    is_running: &mut dyn FnMut(AgentKind) -> bool,
    live_ids: &mut dyn FnMut(AgentKind) -> Vec<String>,
) -> DeletePlan {
    let now = Utc::now();
    let mut live: HashMap<AgentKind, HashSet<String>> = HashMap::new();
    let mut out = DeletePlan {
        sessions: Vec::new(),
        bytes: 0,
        blocked: Vec::new(),
    };
    for s in sessions {
        let mut reasons = unsafe_reasons(home, &s);
        if reasons.is_empty() && !force {
            let open = live
                .entry(s.agent)
                .or_insert_with(|| live_ids(s.agent).into_iter().collect());
            reasons = if open.contains(&s.id) {
                vec![format!("session is open in {}", s.agent.label())]
            } else {
                live_risk(&s, now, is_running)
            };
        }
        if reasons.is_empty() {
            out.bytes += s.bytes;
            out.sessions.push(s);
        } else {
            out.blocked.push(Blocked {
                key: s.key(),
                reason: reasons.join("; "),
            });
        }
    }
    out
}

/// Adds the children of every picked session, at any depth, taken from `all`. A child is a session
/// whose `parent_id` names a session of the same agent. The picked sessions come first in their
/// original order, then the added children in `all` order. No session appears twice.
///
/// Use it before `plan`, so a parent is never deleted while its children stay behind.
pub fn with_children(all: &[Session], picked: Vec<Session>) -> Vec<Session> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<Session> = picked
        .into_iter()
        .filter(|s| seen.insert(s.key()))
        .collect();
    let mut frontier = 0;
    while frontier < out.len() {
        let parent = out[frontier].clone();
        frontier += 1;
        for child in all {
            let is_child = child.agent == parent.agent
                && child.parent_id.as_deref() == Some(parent.id.as_str());
            if is_child && seen.insert(child.key()) {
                out.push(child.clone());
            }
        }
    }
    out
}

fn recent_window() -> Duration {
    Duration::hours(2)
}

fn unsafe_reasons(home: &Path, s: &Session) -> Vec<String> {
    let mut reasons = Vec::new();
    for art in &s.artifacts {
        let p = art.primary_path();
        if !p.is_absolute() {
            reasons.push(format!("refusing relative path {}", p.display()));
        } else if p.parent().is_none() || home.starts_with(p) {
            reasons.push(format!(
                "refusing to touch {}: it contains the home directory",
                p.display()
            ));
        }
    }
    reasons
}

fn live_risk(
    s: &Session,
    now: DateTime<Utc>,
    is_running: &mut dyn FnMut(AgentKind) -> bool,
) -> Vec<String> {
    let mut conditions = Vec::new();
    match s.last_active {
        None => conditions.push("has unknown last activity".to_string()),
        Some(t) if now - t <= recent_window() => conditions.push(format!(
            "was active {} min ago",
            (now - t).num_minutes().max(0)
        )),
        Some(_) => {}
    }
    let in_db = s
        .artifacts
        .iter()
        .any(|a| matches!(a, Artifact::DbRows { .. } | Artifact::DbPrefix { .. }));
    if in_db {
        conditions.push("is stored in a database the agent may write".to_string());
    }
    if conditions.is_empty() || !is_running(s.agent) {
        return Vec::new();
    }
    vec![format!(
        "{} is running; this session {}",
        s.agent.label(),
        conditions.join(", ")
    )]
}

/// One change made to disk, with enough detail to undo it. Paths named `stored` and `saved`
/// are relative to the entry directory.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TrashOp {
    /// A file or directory moved into the entry.
    Moved { original: PathBuf, stored: PathBuf },
    /// Rows deleted from a database; `saved` holds them as JSON.
    DbRows { db: PathBuf, saved: PathBuf },
    /// Lines removed from a shared JSONL file: (0-based line index, raw text with newline).
    JsonlLines {
        file: PathBuf,
        removed: Vec<(usize, String)>,
    },
    /// Elements removed from a JSON array: (original index, value).
    JsonArrayEntry {
        file: PathBuf,
        array_path: String,
        removed: Vec<(usize, Value)>,
    },
    /// One key removed from a JSON object, with its position.
    JsonObjectKey {
        file: PathBuf,
        object_path: String,
        index: usize,
        key: String,
        value: Value,
    },
}

/// One executed delete. Stored as `manifest.json` inside the entry directory.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TrashEntry {
    pub id: String,
    pub created: DateTime<Utc>,
    pub bytes: u64,
    pub sessions: Vec<Session>,
    pub ops: Vec<TrashOp>,
    /// Sessions that failed and were rolled back, with the reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// The trash store: one directory per entry under `root`.
#[derive(Debug, Clone)]
pub struct Trash {
    root: PathBuf,
}

impl Trash {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn default_location() -> Self {
        Self::new(paths::trash_dir())
    }

    /// Moves or rewrites every artifact of the planned sessions. A session that fails is rolled
    /// back and recorded in `errors`; the others continue. Returns `Err` only when no session
    /// succeeded.
    pub fn execute(&self, plan: &DeletePlan) -> Result<TrashEntry> {
        if plan.sessions.is_empty() {
            bail!("nothing to delete");
        }
        let (id, dir) = self.create_entry()?;
        let mut entry = TrashEntry {
            id,
            created: Utc::now(),
            bytes: 0,
            sessions: Vec::new(),
            ops: Vec::new(),
            errors: Vec::new(),
        };
        write_manifest(&dir, &entry)?;

        let mut seq = 0usize;
        let mut rollback_incomplete = false;
        for session in &plan.sessions {
            let mut session_ops = Vec::new();
            match apply_session(&dir, session, &mut seq, &mut session_ops) {
                Ok(()) => {
                    entry.bytes += session.bytes;
                    entry.sessions.push(session.clone());
                    entry.ops.extend(session_ops);
                }
                Err(err) => {
                    let mut msg = format!("{}: {err:#}", session.key());
                    for op in session_ops.iter().rev() {
                        if let Err(undo_err) = undo_op(&dir, op) {
                            rollback_incomplete = true;
                            msg.push_str(&format!(
                                "; rollback incomplete, data kept in {}: {undo_err:#}",
                                dir.display()
                            ));
                            break;
                        }
                    }
                    entry.errors.push(msg);
                }
            }
            write_manifest(&dir, &entry)?;
        }

        if entry.sessions.is_empty() && !rollback_incomplete {
            let _ = fs::remove_dir_all(&dir);
            bail!("every session failed: {}", entry.errors.join("; "));
        }
        Ok(entry)
    }

    /// Trash entries, newest first. Unreadable entries are skipped with a warning.
    pub fn list(&self) -> Result<Vec<TrashEntry>> {
        let mut out = Vec::new();
        for dir in self.entry_dirs()? {
            match read_manifest(&dir) {
                Ok(entry) => out.push(entry),
                Err(err) => eprintln!("sz: skipping trash entry {}: {err:#}", dir.display()),
            }
        }
        out.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| b.id.cmp(&a.id)));
        Ok(out)
    }

    /// Undoes every op of one entry in reverse order, then removes the entry. Checks all
    /// preconditions before it changes anything. Each undone op is dropped from the manifest,
    /// so a failed restore can be retried.
    pub fn restore(&self, id: &str) -> Result<()> {
        let dir = self.entry_dir(id)?;
        let mut entry = read_manifest(&dir).with_context(|| format!("reading trash entry {id}"))?;
        for op in &entry.ops {
            preflight(&dir, op).with_context(|| format!("cannot restore {id}"))?;
        }
        while let Some(op) = entry.ops.last().cloned() {
            undo_op(&dir, &op).with_context(|| format!("restoring {id}"))?;
            entry.ops.pop();
            write_manifest(&dir, &entry)?;
        }
        fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        Ok(())
    }

    /// Permanently deletes one entry, or every entry when `id` is `None`. Vacuums the databases
    /// the removed entries touched (lock errors are ignored). Returns the bytes freed.
    pub fn empty(&self, id: Option<&str>) -> Result<u64> {
        let dirs = match id {
            Some(id) => vec![self.entry_dir(id)?],
            None => self.entry_dirs()?,
        };
        let mut freed = 0u64;
        let mut dbs: BTreeSet<PathBuf> = BTreeSet::new();
        for dir in dirs {
            if let Ok(entry) = read_manifest(&dir) {
                for op in &entry.ops {
                    if let TrashOp::DbRows { db, .. } = op {
                        dbs.insert(db.clone());
                    }
                }
            }
            freed += path_size(&dir);
            fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        }
        for db in dbs {
            if db.is_file() {
                if let Ok(conn) = open_db(&db) {
                    let _ = conn.execute_batch("VACUUM");
                }
            }
        }
        Ok(freed)
    }

    fn create_entry(&self) -> Result<(String, PathBuf)> {
        fs::create_dir_all(&self.root)?;
        for _ in 0..16 {
            let id = format!(
                "{}-{}",
                Utc::now().format("%Y%m%d-%H%M%S"),
                rand_hex(4)
            );
            let dir = self.root.join(&id);
            match fs::create_dir(&dir) {
                Ok(()) => return Ok((id, dir)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        bail!("could not create a unique trash entry")
    }

    fn entry_dirs(&self) -> Result<Vec<PathBuf>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut dirs = Vec::new();
        for item in fs::read_dir(&self.root)? {
            let path = item?.path();
            let valid = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(valid_id);
            if valid && path.is_dir() {
                dirs.push(path);
            }
        }
        Ok(dirs)
    }

    fn entry_dir(&self, id: &str) -> Result<PathBuf> {
        if !valid_id(id) {
            bail!("invalid trash id '{id}'");
        }
        let dir = self.root.join(id);
        if !dir.join(MANIFEST).is_file() {
            bail!("no trash entry '{id}'");
        }
        Ok(dir)
    }
}

/// Entry ids are `YYYYMMDD-HHMMSS-hhhh`. Anything else is rejected so an id cannot escape the root.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn read_manifest(dir: &Path) -> Result<TrashEntry> {
    let bytes = fs::read(dir.join(MANIFEST))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_manifest(dir: &Path, entry: &TrashEntry) -> Result<()> {
    write_atomic(&dir.join(MANIFEST), &serde_json::to_vec_pretty(entry)?)
}

fn rand_hex(len: usize) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut out = String::new();
    while out.len() < len {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(nanos);
        hasher.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out.truncate(len);
    out
}

/// Writes `bytes` to a temp file beside `path`, then renames it over `path`. Keeps the
/// permissions of an existing file.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = dir.join(format!(".{name}.{}.tmp", rand_hex(8)));
    let result = write_then_rename(&tmp, path, bytes);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("writing {}", path.display()))
}

fn write_then_rename(tmp: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    {
        let mut file = fs::File::create(tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    if let Ok(meta) = fs::metadata(path) {
        fs::set_permissions(tmp, meta.permissions())?;
    }
    fs::rename(tmp, path)?;
    Ok(())
}

/// How long to wait for a `<file>.lock` that another process holds before giving up.
const SHARED_LOCK_WAIT: StdDuration = StdDuration::from_secs(5);
/// Reads allowed when a shared file changes between the first read and the rename.
const REREAD_ATTEMPTS: usize = 3;

/// Rewrites a file that other programs may also write, such as a JSONL index or a JSON store.
///
/// `edit` receives the current text. It returns the new text and a record of what it removed or
/// inserted, or `None` when nothing changes. When a sibling `<file>.lock` exists, the call holds
/// an exclusive advisory lock on it from the read through the rename. Writers that take the same
/// lock cannot interleave with it. The file is read again just before the rename. If it changed
/// in the meantime, `edit` runs again on the new text. A writer that ignores the lock cannot be
/// excluded, so a short race window stays.
fn rewrite_shared<T>(
    file: &Path,
    wait: StdDuration,
    mut edit: impl FnMut(&str) -> Result<Option<(String, T)>>,
) -> Result<Option<T>> {
    let _lock = lock_sibling(file, wait)?;
    for _ in 0..REREAD_ATTEMPTS {
        let Some(text) = read_optional(file)? else {
            return Ok(None);
        };
        let Some((next, record)) = edit(&text)? else {
            return Ok(None);
        };
        if read_optional(file)?.as_deref() != Some(text.as_str()) {
            continue;
        }
        write_atomic(file, next.as_bytes())?;
        return Ok(Some(record));
    }
    bail!(
        "{} kept changing while it was being edited; try again",
        file.display()
    )
}

/// Takes an exclusive lock on the sibling `<file>.lock` when it exists. The lock is released
/// when the returned handle is dropped. Returns `None` when there is no lock file.
fn lock_sibling(file: &Path, wait: StdDuration) -> Result<Option<fs::File>> {
    let Some(name) = file.file_name() else {
        return Ok(None);
    };
    let mut lock_name = name.to_os_string();
    lock_name.push(".lock");
    let lock_path = file.with_file_name(lock_name);
    if !lock_path.is_file() {
        return Ok(None);
    }
    let handle = fs::File::open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    let deadline = Instant::now() + wait;
    loop {
        match handle.try_lock() {
            Ok(()) => return Ok(Some(handle)),
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(StdDuration::from_millis(25));
            }
            Err(fs::TryLockError::WouldBlock) => {
                bail!("{} is locked by another process", lock_path.display())
            }
            Err(fs::TryLockError::Error(err)) => {
                return Err(err).with_context(|| format!("locking {}", lock_path.display()));
            }
        }
    }
}

/// `files/<original path without its root>`: where a moved path is stored inside an entry.
fn stored_rel(original: &Path) -> PathBuf {
    let mut rel = PathBuf::from("files");
    for component in original.components() {
        if let Component::Normal(part) = component {
            rel.push(part);
        }
    }
    rel
}

/// Refuses relative paths and filesystem roots. `plan` already checks against the home
/// directory; this guards hand-built plans too.
fn ensure_safe(p: &Path) -> Result<()> {
    if !p.is_absolute() || p.parent().is_none() {
        bail!("refusing to touch unsafe path {}", p.display());
    }
    Ok(())
}

/// Moves a path. Falls back to copy + remove when a rename is not possible (for example across
/// volumes). A failed copy is cleaned up and the source is left in place.
fn move_path(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    if let Err(err) = copy_path(src, dst) {
        let _ = remove_path(dst);
        return Err(err.context(format!("copying {}", src.display())));
    }
    remove_path(src).with_context(|| format!("removing {} after copy", src.display()))
}

fn copy_path(src: &Path, dst: &Path) -> Result<()> {
    let file_type = fs::symlink_metadata(src)?.file_type();
    if file_type.is_symlink() {
        return symlink_to(&fs::read_link(src)?, dst);
    }
    if file_type.is_dir() {
        fs::create_dir(dst)?;
        for entry in WalkDir::new(src).min_depth(1).follow_links(false) {
            let entry = entry?;
            let target = dst.join(entry.path().strip_prefix(src)?);
            let kind = entry.file_type();
            if kind.is_dir() {
                fs::create_dir(&target)?;
            } else if kind.is_symlink() {
                symlink_to(&fs::read_link(entry.path())?, &target)?;
            } else {
                fs::copy(entry.path(), &target)?;
            }
        }
        return Ok(());
    }
    fs::copy(src, dst)?;
    Ok(())
}

#[cfg(unix)]
fn symlink_to(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(not(unix))]
fn symlink_to(_target: &Path, _link: &Path) -> Result<()> {
    bail!("symbolic links are not supported on this platform")
}

fn remove_path(p: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(p)?;
    if meta.is_dir() {
        fs::remove_dir_all(p)?;
    } else {
        fs::remove_file(p)?;
    }
    Ok(())
}

fn require_file(p: &Path) -> Result<()> {
    if p.is_file() {
        Ok(())
    } else {
        bail!("{} is missing", p.display())
    }
}

fn require_present(p: &Path) -> Result<()> {
    if fs::symlink_metadata(p).is_ok() {
        Ok(())
    } else {
        bail!("{} is missing", p.display())
    }
}

/// Checks that `op` can be undone, without changing anything.
fn preflight(dir: &Path, op: &TrashOp) -> Result<()> {
    match op {
        TrashOp::Moved { original, stored } => {
            if fs::symlink_metadata(original).is_ok() {
                bail!("{} already exists; move it aside first", original.display());
            }
            require_present(&dir.join(stored))?;
        }
        TrashOp::DbRows { db, saved } => {
            require_file(db)?;
            require_file(&dir.join(saved))?;
        }
        TrashOp::JsonlLines { file, .. }
        | TrashOp::JsonArrayEntry { file, .. }
        | TrashOp::JsonObjectKey { file, .. } => require_file(file)?,
    }
    Ok(())
}

/// Reverses one recorded op.
fn undo_op(dir: &Path, op: &TrashOp) -> Result<()> {
    match op {
        TrashOp::Moved { original, stored } => {
            if fs::symlink_metadata(original).is_ok() {
                bail!("{} already exists", original.display());
            }
            move_path(&dir.join(stored), original)?;
        }
        TrashOp::DbRows { db, saved } => {
            let saved_rows: SavedRows = serde_json::from_slice(&fs::read(dir.join(saved))?)?;
            let mut conn = open_db(db)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            insert_rows(&tx, &saved_rows)?;
            tx.commit()?;
        }
        TrashOp::JsonlLines { file, removed } => {
            rewrite_shared(file, SHARED_LOCK_WAIT, |text| {
                let mut lines: Vec<String> =
                    text.split_inclusive('\n').map(str::to_string).collect();
                for (idx, raw) in removed {
                    if *idx > lines.len() {
                        bail!("line {idx} is out of range in {}", file.display());
                    }
                    lines.insert(*idx, raw.clone());
                }
                Ok(Some((lines.concat(), ())))
            })?;
        }
        TrashOp::JsonArrayEntry {
            file,
            array_path,
            removed,
        } => {
            rewrite_shared(file, SHARED_LOCK_WAIT, |text| {
                let mut doc: Value = serde_json::from_str(text)?;
                match navigate_mut(&mut doc, array_path) {
                    Some(Value::Array(items)) => {
                        for (idx, value) in removed {
                            let at = (*idx).min(items.len());
                            items.insert(at, value.clone());
                        }
                    }
                    _ => bail!("no array '{array_path}' in {}", file.display()),
                }
                Ok(Some((to_json_text(&doc, text)?, ())))
            })?;
        }
        TrashOp::JsonObjectKey {
            file,
            object_path,
            index,
            key,
            value,
        } => {
            rewrite_shared(file, SHARED_LOCK_WAIT, |text| {
                let mut doc: Value = serde_json::from_str(text)?;
                match navigate_mut(&mut doc, object_path) {
                    Some(Value::Object(map)) => {
                        if map.contains_key(key) {
                            bail!("key '{key}' already exists in {}", file.display());
                        }
                        map_insert_at(map, *index, key.clone(), value.clone());
                    }
                    _ => bail!("no object '{object_path}' in {}", file.display()),
                }
                Ok(Some((to_json_text(&doc, text)?, ())))
            })?;
        }
    }
    Ok(())
}

/// Removes every artifact of one session, recording an op per change.
fn apply_session(
    dir: &Path,
    session: &Session,
    seq: &mut usize,
    ops: &mut Vec<TrashOp>,
) -> Result<()> {
    for art in &session.artifacts {
        ensure_safe(art.primary_path())?;
        match art {
            Artifact::File(p) | Artifact::Dir(p) => {
                if fs::symlink_metadata(p).is_err() {
                    continue;
                }
                let stored = stored_rel(p);
                move_path(p, &dir.join(&stored))?;
                ops.push(TrashOp::Moved {
                    original: p.clone(),
                    stored,
                });
            }
            Artifact::DbRows { db, deletes, .. } => {
                let specs: Vec<DbSpec> = deletes
                    .iter()
                    .map(|d| DbSpec {
                        table: d.table.clone(),
                        cond: format!("{} = ?1", quote(&d.where_col)),
                        arg: d.value.clone(),
                    })
                    .collect();
                remove_db_rows(dir, db, &specs, seq, ops)?;
            }
            Artifact::DbPrefix {
                db,
                table,
                col,
                prefix,
                ..
            } => {
                let specs = [DbSpec {
                    table: table.clone(),
                    cond: format!("substr({}, 1, length(?1)) = ?1", quote(col)),
                    arg: prefix.clone(),
                }];
                remove_db_rows(dir, db, &specs, seq, ops)?;
            }
            Artifact::JsonlLines { file, field, value } => {
                remove_jsonl_lines(file, field, value, ops)?;
            }
            Artifact::JsonArrayEntry {
                file,
                array_path,
                field,
                value,
            } => {
                remove_json_array_entries(file, array_path, field, value, ops)?;
            }
            Artifact::JsonObjectKey {
                file,
                object_path,
                key,
            } => {
                remove_json_object_key(file, object_path, key, ops)?;
            }
        }
    }
    Ok(())
}

struct DbSpec {
    table: String,
    /// WHERE clause using `?1` as the only parameter.
    cond: String,
    arg: String,
}

/// Saves matching rows to the entry, then deletes them. One transaction covers all specs. The
/// ops are recorded only after the commit.
fn remove_db_rows(
    dir: &Path,
    db: &Path,
    specs: &[DbSpec],
    seq: &mut usize,
    ops: &mut Vec<TrashOp>,
) -> Result<()> {
    if !db.is_file() {
        return Ok(());
    }
    let mut conn = open_db(db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut staged = Vec::new();
    for spec in specs {
        let saved = fetch_rows(&tx, &spec.table, &spec.cond, &spec.arg)?;
        if saved.rows.is_empty() {
            continue;
        }
        let rel = PathBuf::from("db").join(format!("{}.json", *seq));
        *seq += 1;
        write_atomic(&dir.join(&rel), &serde_json::to_vec_pretty(&saved)?)?;
        tx.execute(
            &format!("DELETE FROM {} WHERE {}", quote(&spec.table), spec.cond),
            [&spec.arg],
        )?;
        staged.push(TrashOp::DbRows {
            db: db.to_path_buf(),
            saved: rel,
        });
    }
    tx.commit()?;
    ops.extend(staged);
    Ok(())
}

fn remove_jsonl_lines(
    file: &Path,
    field: &str,
    value: &str,
    ops: &mut Vec<TrashOp>,
) -> Result<()> {
    let removed = rewrite_shared(file, SHARED_LOCK_WAIT, |text| {
        let mut kept = String::with_capacity(text.len());
        let mut removed = Vec::new();
        for (idx, raw) in text.split_inclusive('\n').enumerate() {
            if jsonl_line_matches(raw, field, value) {
                removed.push((idx, raw.to_string()));
            } else {
                kept.push_str(raw);
            }
        }
        Ok((!removed.is_empty()).then_some((kept, removed)))
    })?;
    if let Some(removed) = removed {
        ops.push(TrashOp::JsonlLines {
            file: file.to_path_buf(),
            removed,
        });
    }
    Ok(())
}

fn jsonl_line_matches(raw: &str, field: &str, value: &str) -> bool {
    let line = raw.trim();
    if line.is_empty() {
        return false;
    }
    match serde_json::from_str::<Value>(line) {
        Ok(v) => dotted(&v, field).is_some_and(|x| as_text(x) == value),
        Err(_) => false,
    }
}

fn remove_json_array_entries(
    file: &Path,
    array_path: &str,
    field: &str,
    value: &str,
    ops: &mut Vec<TrashOp>,
) -> Result<()> {
    let removed = rewrite_shared(file, SHARED_LOCK_WAIT, |text| {
        let mut doc: Value = serde_json::from_str(text)
            .with_context(|| format!("parsing {}", file.display()))?;
        let mut removed: Vec<(usize, Value)> = Vec::new();
        if let Some(Value::Array(items)) = navigate_mut(&mut doc, array_path) {
            let hits: Vec<usize> = items
                .iter()
                .enumerate()
                .filter(|(_, item)| dotted(item, field).is_some_and(|x| as_text(x) == value))
                .map(|(i, _)| i)
                .collect();
            for &i in hits.iter().rev() {
                removed.push((i, items.remove(i)));
            }
            removed.reverse();
        }
        if removed.is_empty() {
            return Ok(None);
        }
        Ok(Some((to_json_text(&doc, text)?, removed)))
    })?;
    if let Some(removed) = removed {
        ops.push(TrashOp::JsonArrayEntry {
            file: file.to_path_buf(),
            array_path: array_path.to_string(),
            removed,
        });
    }
    Ok(())
}

fn remove_json_object_key(
    file: &Path,
    object_path: &str,
    key: &str,
    ops: &mut Vec<TrashOp>,
) -> Result<()> {
    let hit = rewrite_shared(file, SHARED_LOCK_WAIT, |text| {
        let mut doc: Value = serde_json::from_str(text)
            .with_context(|| format!("parsing {}", file.display()))?;
        let mut hit = None;
        if let Some(Value::Object(map)) = navigate_mut(&mut doc, object_path) {
            if map.contains_key(key) {
                hit = Some(map_remove(map, key));
            }
        }
        let Some((index, value)) = hit else {
            return Ok(None);
        };
        Ok(Some((to_json_text(&doc, text)?, (index, value))))
    })?;
    if let Some((index, value)) = hit {
        ops.push(TrashOp::JsonObjectKey {
            file: file.to_path_buf(),
            object_path: object_path.to_string(),
            index,
            key: key.to_string(),
            value,
        });
    }
    Ok(())
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(text))
}

fn navigate_mut<'a>(mut value: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    for seg in path.split('.').filter(|s| !s.is_empty()) {
        value = value.get_mut(seg)?;
    }
    Some(value)
}

fn dotted<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = value;
    for seg in path.split('.').filter(|s| !s.is_empty()) {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

/// A JSON scalar as text, for comparing against an artifact's `value`.
fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Removes `key` from `map` and returns its position and value. Rebuilding the map keeps the
/// order of the remaining keys.
fn map_remove(map: &mut Map<String, Value>, key: &str) -> (usize, Value) {
    let old = std::mem::take(map);
    let mut index = 0;
    let mut value = Value::Null;
    for (i, (k, v)) in old.into_iter().enumerate() {
        if k == key {
            index = i;
            value = v;
        } else {
            map.insert(k, v);
        }
    }
    (index, value)
}

fn map_insert_at(map: &mut Map<String, Value>, index: usize, key: String, value: Value) {
    let old = std::mem::take(map);
    let mut pending = Some((key, value));
    for (i, (k, v)) in old.into_iter().enumerate() {
        if i == index {
            if let Some((pk, pv)) = pending.take() {
                map.insert(pk, pv);
            }
        }
        map.insert(k, v);
    }
    if let Some((pk, pv)) = pending {
        map.insert(pk, pv);
    }
}

/// Serializes `doc` in the same style as `original`: pretty with the original indent, or
/// compact. A trailing newline is kept when the original had one.
fn to_json_text(doc: &Value, original: &str) -> Result<String> {
    use serde::Serialize as _;
    let mut out = if original.contains('\n') {
        let indent: String = original
            .lines()
            .find_map(|line| {
                let ws: String = line
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                (!ws.is_empty() && !line.trim().is_empty()).then_some(ws)
            })
            .unwrap_or_else(|| "  ".to_string());
        let mut buf = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
        let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
        doc.serialize(&mut ser)?;
        String::from_utf8(buf)?
    } else {
        serde_json::to_string(doc)?
    };
    if original.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// A row as saved in an entry. `rowid` is only present when the table has one.
#[derive(Serialize, Deserialize, Debug)]
struct SavedRows {
    table: String,
    rowid: bool,
    columns: Vec<String>,
    rows: Vec<SavedRow>,
}

#[derive(Serialize, Deserialize, Debug)]
struct SavedRow {
    rowid: i64,
    values: Vec<DbValue>,
}

/// SQLite value with its type kept. Text that is not valid UTF-8 is stored as bytes and
/// restored with a `CAST(... AS TEXT)`.
#[derive(Serialize, Deserialize, Debug)]
enum DbValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    TextBytes(String),
    Blob(String),
}

fn open_db(db: &Path) -> Result<Connection> {
    let conn = Connection::open(db).with_context(|| format!("opening {}", db.display()))?;
    conn.busy_timeout(StdDuration::from_secs(2))?;
    Ok(conn)
}

/// Quotes an identifier for SQL.
fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn to_db_value(v: ValueRef<'_>) -> DbValue {
    match v {
        ValueRef::Null => DbValue::Null,
        ValueRef::Integer(i) => DbValue::Integer(i),
        ValueRef::Real(f) => DbValue::Real(f),
        ValueRef::Text(t) => match std::str::from_utf8(t) {
            Ok(s) => DbValue::Text(s.to_string()),
            Err(_) => DbValue::TextBytes(BASE64.encode(t)),
        },
        ValueRef::Blob(b) => DbValue::Blob(BASE64.encode(b)),
    }
}

fn to_sql_value(v: &DbValue) -> Result<(SqlValue, bool)> {
    Ok(match v {
        DbValue::Null => (SqlValue::Null, false),
        DbValue::Integer(i) => (SqlValue::Integer(*i), false),
        DbValue::Real(f) => (SqlValue::Real(*f), false),
        DbValue::Text(s) => (SqlValue::Text(s.clone()), false),
        DbValue::TextBytes(b) => (SqlValue::Blob(BASE64.decode(b)?), true),
        DbValue::Blob(b) => (SqlValue::Blob(BASE64.decode(b)?), false),
    })
}

/// Reads the rows matching `cond` (one `?1` parameter bound to `arg`). Uses `rowid` when the
/// table has one.
fn fetch_rows(conn: &Connection, table: &str, cond: &str, arg: &str) -> Result<SavedRows> {
    let q = quote(table);
    let (mut stmt, has_rowid) = match conn.prepare(&format!(
        "SELECT rowid AS \"__sz_rowid\", * FROM {q} WHERE {cond}"
    )) {
        Ok(stmt) => (stmt, true),
        Err(_) => (
            conn.prepare(&format!("SELECT * FROM {q} WHERE {cond}"))?,
            false,
        ),
    };
    let start = usize::from(has_rowid);
    let columns: Vec<String> = stmt
        .column_names()
        .iter()
        .skip(start)
        .map(|s| s.to_string())
        .collect();
    let mut rows = Vec::new();
    let mut query = stmt.query([arg])?;
    while let Some(row) = query.next()? {
        let rowid = if has_rowid {
            row.get::<_, i64>(0)?
        } else {
            0
        };
        let mut values = Vec::with_capacity(columns.len());
        for i in 0..columns.len() {
            values.push(to_db_value(row.get_ref(start + i)?));
        }
        rows.push(SavedRow { rowid, values });
    }
    Ok(SavedRows {
        table: table.to_string(),
        rowid: has_rowid,
        columns,
        rows,
    })
}

/// Re-inserts saved rows. `INSERT OR IGNORE` leaves rows that already exist untouched.
fn insert_rows(conn: &Connection, saved: &SavedRows) -> Result<usize> {
    let mut cols: Vec<String> = Vec::new();
    if saved.rowid {
        cols.push(quote("rowid"));
    }
    cols.extend(saved.columns.iter().map(|c| quote(c)));
    let mut inserted = 0;
    for row in &saved.rows {
        let mut params: Vec<SqlValue> = Vec::new();
        let mut holes: Vec<&str> = Vec::new();
        if saved.rowid {
            params.push(SqlValue::Integer(row.rowid));
            holes.push("?");
        }
        for value in &row.values {
            let (sql_value, cast) = to_sql_value(value)?;
            params.push(sql_value);
            holes.push(if cast { "CAST(? AS TEXT)" } else { "?" });
        }
        let sql = format!(
            "INSERT OR IGNORE INTO {} ({}) VALUES ({})",
            quote(&saved.table),
            cols.join(", "),
            holes.join(", ")
        );
        inserted += conn.execute(&sql, rusqlite::params_from_iter(params.iter()))?;
    }
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentKind, DbDelete};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn session(
        agent: AgentKind,
        id: &str,
        artifacts: Vec<Artifact>,
        last_active: Option<DateTime<Utc>>,
    ) -> Session {
        let mut s = Session {
            parent_id: None,
            agent,
            id: id.into(),
            title: None,
            project: None,
            created: None,
            last_active,
            message_count: None,
            bytes: 0,
            artifacts,
        };
        s.recompute_bytes();
        s
    }

    fn recent() -> Option<DateTime<Utc>> {
        Some(Utc::now() - Duration::days(3))
    }

    fn write(p: &Path, bytes: &[u8]) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }

    /// Every regular file under `root` except SQLite databases (their page layout changes on
    /// restore, so they are checked by row contents instead).
    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        for e in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            let is_db = e.path().extension().is_some_and(|x| x == "db" || x == "vscdb");
            if e.file_type().is_file() && !is_db {
                out.insert(
                    e.path().strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(e.path()).unwrap(),
                );
            }
        }
        out
    }

    /// Every row as text, with each value tagged by its SQLite type.
    fn dump(db: &Path, sql: &str) -> Vec<String> {
        fn fmt_value(v: ValueRef<'_>) -> String {
            match v {
                ValueRef::Null => "NULL".to_string(),
                ValueRef::Integer(i) => format!("I:{i}"),
                ValueRef::Real(f) => format!("R:{f}"),
                ValueRef::Text(t) => format!("T:{}", String::from_utf8_lossy(t)),
                ValueRef::Blob(b) => format!("B:{b:?}"),
            }
        }
        let conn = Connection::open(db).unwrap();
        let mut stmt = conn.prepare(sql).unwrap();
        let n = stmt.column_count();
        let rows = stmt
            .query_map([], |row| {
                let mut parts = Vec::new();
                for i in 0..n {
                    parts.push(fmt_value(row.get_ref(i)?));
                }
                Ok(parts.join("|"))
            })
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    fn make_state_db(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id INTEGER PRIMARY KEY, sid TEXT, payload BLOB, score REAL, note, extra);
             INSERT INTO sessions VALUES (1, 'abc', X'00FF', 1.5, 'hi', NULL);
             INSERT INTO sessions VALUES (2, 'xyz', X'', 2.0, 'other', 7);
             INSERT INTO sessions VALUES (3, 'abc', X'ABCD', 3.0, 'bytes', 'text');
             INSERT INTO sessions VALUES (4, 'abc', X'00', 4.0, CAST(X'FFFE' AS TEXT), NULL);
             CREATE TABLE other (rid INTEGER PRIMARY KEY, sid TEXT);
             INSERT INTO other VALUES (1, 'abc');
             CREATE TABLE kv (k TEXT PRIMARY KEY, v) WITHOUT ROWID;
             INSERT INTO kv VALUES ('abc', 'v1');
             INSERT INTO kv VALUES ('keep', 'v2');",
        )
        .unwrap();
    }

    fn make_cursor_db(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
             INSERT INTO cursorDiskKV VALUES ('a_%:1', X'01');
             INSERT INTO cursorDiskKV VALUES ('a_%:2', X'02');
             INSERT INTO cursorDiskKV VALUES ('axb%:9', X'03');
             INSERT INTO cursorDiskKV VALUES ('composerData:C1', X'04');",
        )
        .unwrap();
    }

    fn trash_in(tmp: &tempfile::TempDir) -> Trash {
        Trash::new(tmp.path().join("trash"))
    }

    /// Plans with nothing running, executes, checks the sources are gone, restores, and checks
    /// the file tree is back to its exact bytes.
    fn round_trip(home: &Path, trash: &Trash, sessions: Vec<Session>) -> TrashEntry {
        let before = snapshot(home);
        let plan = plan_with(home, sessions, false, &mut |_| false);
        assert!(plan.blocked.is_empty(), "nothing should be blocked");
        let entry = trash.execute(&plan).expect("execute");
        assert!(entry.errors.is_empty(), "errors: {:?}", entry.errors);
        assert_ne!(snapshot(home), before, "execute changed something");

        trash.restore(&entry.id).expect("restore");
        assert_eq!(snapshot(home), before, "restore brings back identical files");
        assert!(!trash.root.join(&entry.id).exists(), "entry removed after restore");
        entry
    }

    #[test]
    fn round_trip_file_and_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let file = home.join("proj/sessions/a.jsonl");
        let dir = home.join("store/s2");
        write(&file, b"line one\nline two\n");
        write(&dir.join("one.bin"), &[0u8, 1, 2, 255]);
        write(&dir.join("nested/two.bin"), b"deep");
        let trash = trash_in(&tmp);

        let sessions = vec![
            session(AgentKind::Codex, "a", vec![Artifact::File(file.clone())], recent()),
            session(AgentKind::Claude, "b", vec![Artifact::Dir(dir.clone())], recent()),
        ];
        let before_plan = plan_with(&home, sessions.clone(), false, &mut |_| false);
        assert_eq!(before_plan.bytes, sessions.iter().map(|s| s.bytes).sum::<u64>());

        round_trip(&home, &trash, sessions);
    }

    #[test]
    fn execute_removes_file_and_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let file = home.join("f.txt");
        let dir = home.join("d");
        write(&file, b"x");
        write(&dir.join("inner.txt"), b"y");
        let trash = trash_in(&tmp);
        let plan = plan_with(
            &home,
            vec![session(
                AgentKind::Codex,
                "a",
                vec![Artifact::File(file.clone()), Artifact::Dir(dir.clone())],
                recent(),
            )],
            false,
            &mut |_| false,
        );
        let entry = trash.execute(&plan).unwrap();
        assert!(!file.exists());
        assert!(!dir.exists());
        assert_eq!(entry.bytes, 2);
        trash.restore(&entry.id).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"x");
        assert_eq!(fs::read(dir.join("inner.txt")).unwrap(), b"y");
    }

    #[test]
    fn round_trip_jsonl_lines_including_unterminated_last_line() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let history = home.join(".codex/history.jsonl");
        let body = concat!(
            r#"{"session_id":"abc","text":"first"}"#, "\n",
            r#"{"session_id":"xyz","text":"other"}"#, "\n",
            r#"{"payload":{"id":"abc"},"n":1}"#, "\n",
            "not json at all\n",
            r#"{"session_id":"abc","text":"last"}"#,
        );
        write(&history, body.as_bytes());
        let trash = trash_in(&tmp);

        let sessions = vec![
            session(
                AgentKind::Codex,
                "abc",
                vec![Artifact::JsonlLines {
                    file: history.clone(),
                    field: "session_id".into(),
                    value: "abc".into(),
                }],
                recent(),
            ),
            session(
                AgentKind::Codex,
                "abc-payload",
                vec![Artifact::JsonlLines {
                    file: history.clone(),
                    field: "payload.id".into(),
                    value: "abc".into(),
                }],
                recent(),
            ),
        ];
        let plan = plan_with(&home, sessions, false, &mut |_| false);
        let entry = trash.execute(&plan).unwrap();
        let left = fs::read_to_string(&history).unwrap();
        assert_eq!(
            left,
            "{\"session_id\":\"xyz\",\"text\":\"other\"}\nnot json at all\n",
            "only matching lines removed, unparsable line kept"
        );

        trash.restore(&entry.id).unwrap();
        assert_eq!(fs::read_to_string(&history).unwrap(), body);
    }

    #[test]
    fn round_trip_json_array_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let sessions_json = home.join(".claude/sessions.json");
        let doc = json!({
            "entries": [
                {"id": "abc", "n": 1},
                {"id": "xyz", "n": 2},
                {"id": "abc", "n": 3}
            ],
            "version": 2
        });
        write(&sessions_json, serde_json::to_string_pretty(&doc).unwrap().as_bytes());
        let trash = trash_in(&tmp);
        let sessions = vec![session(
            AgentKind::Claude,
            "abc",
            vec![Artifact::JsonArrayEntry {
                file: sessions_json.clone(),
                array_path: "entries".into(),
                field: "id".into(),
                value: "abc".into(),
            }],
            recent(),
        )];
        let plan = plan_with(&home, sessions, false, &mut |_| false);
        let entry = trash.execute(&plan).unwrap();
        let after: Value = serde_json::from_str(&fs::read_to_string(&sessions_json).unwrap()).unwrap();
        assert_eq!(after["entries"], json!([{"id": "xyz", "n": 2}]));
        assert_eq!(after["version"], json!(2));

        trash.restore(&entry.id).unwrap();
        assert_eq!(
            fs::read_to_string(&sessions_json).unwrap(),
            serde_json::to_string_pretty(&doc).unwrap()
        );
    }

    #[test]
    fn round_trip_json_object_key_nested_and_root() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let map_json = home.join(".gemini/map.json");
        let doc = json!({
            "z": 3,
            "projects": {"abc": {"x": 1}, "keep": 2},
            "a": "first"
        });
        let original = serde_json::to_string_pretty(&doc).unwrap() + "\n";
        write(&map_json, original.as_bytes());
        let trash = trash_in(&tmp);
        let sessions = vec![
            session(
                AgentKind::Gemini,
                "z",
                vec![Artifact::JsonObjectKey {
                    file: map_json.clone(),
                    object_path: "".into(),
                    key: "z".into(),
                }],
                recent(),
            ),
            session(
                AgentKind::Gemini,
                "abc",
                vec![Artifact::JsonObjectKey {
                    file: map_json.clone(),
                    object_path: "projects".into(),
                    key: "abc".into(),
                }],
                recent(),
            ),
        ];
        let plan = plan_with(&home, sessions, false, &mut |_| false);
        let entry = trash.execute(&plan).unwrap();
        let after: Value = serde_json::from_str(&fs::read_to_string(&map_json).unwrap()).unwrap();
        assert_eq!(after, json!({"projects": {"keep": 2}, "a": "first"}));

        trash.restore(&entry.id).unwrap();
        assert_eq!(fs::read_to_string(&map_json).unwrap(), original, "key order and trailing newline kept");
    }

    #[test]
    fn round_trip_sqlite_rows_keep_types_and_rowids() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let db = home.join(".kiro/state.db");
        make_state_db(&db);
        let sessions_before = dump(&db, "SELECT rowid, * FROM sessions ORDER BY rowid");
        let kv_before = dump(&db, "SELECT * FROM kv ORDER BY k");
        let other_before = dump(&db, "SELECT * FROM other");
        let trash = trash_in(&tmp);

        let sessions = vec![session(
            AgentKind::KiroCli,
            "abc",
            vec![Artifact::DbRows {
                db: db.clone(),
                deletes: vec![
                    DbDelete {
                        table: "sessions".into(),
                        where_col: "sid".into(),
                        value: "abc".into(),
                    },
                    DbDelete {
                        table: "kv".into(),
                        where_col: "k".into(),
                        value: "abc".into(),
                    },
                ],
                bytes: 0,
            }],
            recent(),
        )];
        let plan = plan_with(&home, sessions, false, &mut |_| false);
        let entry = trash.execute(&plan).unwrap();
        assert_eq!(dump(&db, "SELECT sid FROM sessions"), vec!["T:xyz".to_string()]);
        assert_eq!(dump(&db, "SELECT k FROM kv"), vec!["T:keep".to_string()]);
        assert_eq!(dump(&db, "SELECT * FROM other"), other_before, "unmatched table untouched");

        trash.restore(&entry.id).unwrap();
        assert_eq!(dump(&db, "SELECT rowid, * FROM sessions ORDER BY rowid"), sessions_before);
        assert_eq!(dump(&db, "SELECT * FROM kv ORDER BY k"), kv_before);
        assert_eq!(dump(&db, "SELECT * FROM other"), other_before);
    }

    #[test]
    fn round_trip_sqlite_prefix_rows_are_literal() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let db = home.join("cursor/globalStorage/state.vscdb");
        make_cursor_db(&db);
        let before = dump(&db, "SELECT * FROM cursorDiskKV ORDER BY key");
        let trash = trash_in(&tmp);

        let sessions = vec![session(
            AgentKind::Cursor,
            "C1",
            vec![
                Artifact::DbPrefix {
                    db: db.clone(),
                    table: "cursorDiskKV".into(),
                    col: "key".into(),
                    prefix: "a_%:".into(),
                    bytes: 0,
                },
            ],
            recent(),
        )];
        let plan = plan_with(&home, sessions, false, &mut |_| false);
        let entry = trash.execute(&plan).unwrap();
        // LIKE would treat `_` and `%` as wildcards and also remove 'axb%:9'.
        assert_eq!(
            dump(&db, "SELECT key FROM cursorDiskKV ORDER BY key"),
            vec!["T:axb%:9".to_string(), "T:composerData:C1".to_string()]
        );

        trash.restore(&entry.id).unwrap();
        assert_eq!(dump(&db, "SELECT * FROM cursorDiskKV ORDER BY key"), before);
    }

    #[test]
    fn round_trip_all_variants_across_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let file = home.join("x/a.json");
        let jsonl = home.join("x/h.jsonl");
        let array = home.join("x/arr.json");
        let state = home.join("x/state.db");
        write(&file, b"{}");
        write(&jsonl, b"{\"session_id\":\"s\"}\n{\"session_id\":\"t\"}\n");
        write(&array, serde_json::to_string(&json!([{"id": "s"}, {"id": "t"}])).unwrap().as_bytes());
        make_state_db(&state);
        let db_before = dump(&state, "SELECT rowid, * FROM sessions ORDER BY rowid");
        let trash = trash_in(&tmp);

        let sessions = vec![
            session(
                AgentKind::Codex,
                "s",
                vec![
                    Artifact::File(file.clone()),
                    Artifact::JsonlLines {
                        file: jsonl.clone(),
                        field: "session_id".into(),
                        value: "s".into(),
                    },
                    Artifact::JsonArrayEntry {
                        file: array.clone(),
                        array_path: "".into(),
                        field: "id".into(),
                        value: "s".into(),
                    },
                ],
                recent(),
            ),
            session(
                AgentKind::KiroCli,
                "abc",
                vec![Artifact::DbRows {
                    db: state.clone(),
                    deletes: vec![DbDelete {
                        table: "sessions".into(),
                        where_col: "sid".into(),
                        value: "abc".into(),
                    }],
                    bytes: 0,
                }],
                recent(),
            ),
        ];
        let plan = plan_with(&home, sessions, false, &mut |_| false);
        let entry = trash.execute(&plan).unwrap();
        assert_eq!(entry.sessions.len(), 2);
        assert!(!file.exists());

        trash.restore(&entry.id).unwrap();
        assert_eq!(dump(&state, "SELECT rowid, * FROM sessions ORDER BY rowid"), db_before);
        assert_eq!(fs::read_to_string(&jsonl).unwrap(), "{\"session_id\":\"s\"}\n{\"session_id\":\"t\"}\n");
        assert_eq!(fs::read(&file).unwrap(), b"{}");
    }

    #[test]
    fn plan_blocks_live_sessions_of_running_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let file = |name: &str| Artifact::File(home.join(name));
        let db = Artifact::DbRows {
            db: home.join("state.db"),
            deletes: vec![],
            bytes: 0,
        };
        let now = Utc::now();
        let old = Some(now - Duration::days(10));
        let just_now = Some(now - Duration::minutes(5));
        let mut sessions = vec![
            session(AgentKind::Codex, "a-live", vec![file("a")], just_now),
            session(AgentKind::Claude, "b-old", vec![file("b")], old),
            session(AgentKind::KiroCli, "c-unknown", vec![file("c")], None),
            session(AgentKind::Grok, "d-db", vec![db.clone()], old),
            session(AgentKind::Gemini, "e-recent-idle", vec![file("e")], just_now),
            session(AgentKind::Codex, "f-old", vec![file("f")], old),
            session(AgentKind::Codex, "g-old-db", vec![db], old),
        ];
        for s in &mut sessions {
            s.bytes = 100;
        }
        let running = |k: AgentKind| matches!(k, AgentKind::Codex | AgentKind::KiroCli | AgentKind::Grok);

        let p = plan_with(&home, sessions.clone(), false, &mut |k| running(k));
        let blocked: Vec<&str> = p.blocked.iter().map(|b| b.key.as_str()).collect();
        assert_eq!(blocked, vec!["codex:a-live", "kiro:c-unknown", "grok:d-db", "codex:g-old-db"]);
        let allowed: Vec<String> = p.sessions.iter().map(|s| s.key()).collect();
        assert_eq!(allowed, vec!["claude:b-old", "gemini:e-recent-idle", "codex:f-old"]);
        assert_eq!(p.bytes, 300);
        assert!(p.blocked[0].reason.contains("codex is running"));

        let forced = plan_with(&home, sessions, true, &mut |k| running(k));
        assert!(forced.blocked.is_empty(), "force skips the running check");
    }

    #[test]
    fn plan_blocks_sessions_the_adapter_reports_as_open() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let file = |name: &str| Artifact::File(home.join(name));
        // Old and not in a database, so only the live-id check can block them.
        let old = Some(Utc::now() - Duration::days(10));
        let sessions = vec![
            session(AgentKind::KiroCli, "open-1", vec![file("a")], old),
            session(AgentKind::KiroCli, "closed-1", vec![file("b")], old),
            session(AgentKind::Codex, "open-1", vec![file("c")], old),
        ];
        let mut live = |k: AgentKind| match k {
            AgentKind::KiroCli => vec!["open-1".to_string()],
            _ => Vec::new(),
        };
        let p = plan_with_live(&home, sessions.clone(), false, &mut |_| false, &mut live);
        let blocked: Vec<(&str, &str)> = p
            .blocked
            .iter()
            .map(|b| (b.key.as_str(), b.reason.as_str()))
            .collect();
        assert_eq!(blocked, vec![("kiro:open-1", "session is open in kiro")]);
        let allowed: Vec<String> = p.sessions.iter().map(|s| s.key()).collect();
        assert_eq!(allowed, vec!["kiro:closed-1", "codex:open-1"]);

        let forced = plan_with_live(&home, sessions, true, &mut |_| false, &mut live);
        assert!(forced.blocked.is_empty(), "force skips the open-session check");
    }

    #[test]
    fn with_children_adds_descendants_of_picked_parents_once() {
        let tmp = tempfile::tempdir().unwrap();
        let file = |name: &str| Artifact::File(tmp.path().join(name));
        let mk = |agent: AgentKind, id: &str, parent: Option<&str>| {
            let mut s = session(agent, id, vec![file(id)], None);
            s.parent_id = parent.map(str::to_string);
            s
        };
        let all = vec![
            mk(AgentKind::KiroCli, "root", None),
            mk(AgentKind::KiroCli, "child", Some("root")),
            mk(AgentKind::KiroCli, "grandchild", Some("child")),
            mk(AgentKind::KiroCli, "other", None),
            // Same id in another agent is not a child.
            mk(AgentKind::Codex, "impostor", Some("root")),
        ];
        let picked = vec![all[0].clone(), all[3].clone(), all[0].clone()];
        let keys: Vec<String> = with_children(&all, picked)
            .iter()
            .map(|s| s.key())
            .collect();
        assert_eq!(
            keys,
            vec![
                "kiro:root",
                "kiro:other",
                "kiro:child",
                "kiro:grandchild",
            ]
        );
    }

    #[test]
    fn shared_rewrite_rereads_when_the_file_changes_before_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("index.jsonl");
        write(&file, b"a\n");
        let mut calls = 0;
        let path = file.clone();
        let record = rewrite_shared(&file, SHARED_LOCK_WAIT, |text| {
            calls += 1;
            if calls == 1 {
                // Another writer that ignores the lock appends a line meanwhile.
                fs::write(&path, format!("{text}X\n")).unwrap();
            }
            Ok(Some((format!("{text}b\n"), calls)))
        })
        .unwrap();
        assert_eq!(calls, 2, "edit must run again on the changed text");
        assert_eq!(record, Some(2));
        assert_eq!(fs::read(&file).unwrap(), b"a\nX\nb\n");
    }

    #[test]
    fn shared_rewrite_waits_for_a_held_lock_then_proceeds() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("store.json");
        write(&file, b"{}");
        let lock_path = tmp.path().join("store.json.lock");
        write(&lock_path, b"");
        let holder = fs::File::open(&lock_path).unwrap();
        holder.lock().unwrap();
        let releaser = thread::spawn(move || {
            thread::sleep(StdDuration::from_millis(200));
            drop(holder);
        });

        let started = Instant::now();
        let done = rewrite_shared(&file, SHARED_LOCK_WAIT, |_| {
            Ok(Some(("{\"ok\":true}".to_string(), ())))
        })
        .unwrap();
        releaser.join().unwrap();
        assert!(done.is_some());
        assert!(started.elapsed() >= StdDuration::from_millis(150));
        assert_eq!(fs::read(&file).unwrap(), b"{\"ok\":true}");
    }

    #[test]
    fn shared_rewrite_refuses_when_the_lock_stays_held() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("store.jsonl");
        write(&file, b"keep\n");
        let lock_path = tmp.path().join("store.jsonl.lock");
        write(&lock_path, b"");
        let holder = fs::File::open(&lock_path).unwrap();
        holder.lock().unwrap();

        let err = rewrite_shared(&file, StdDuration::from_millis(100), |_| {
            Ok(Some(("gone\n".to_string(), ())))
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("locked by another process"));
        assert_eq!(fs::read(&file).unwrap(), b"keep\n", "file must be untouched");
        drop(holder);
    }

    #[test]
    fn execute_removes_jsonl_line_under_a_sibling_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let index = home.join("index.jsonl");
        write(&index, b"{\"id\":\"x\"}\n{\"id\":\"y\"}\n");
        write(&home.join("index.jsonl.lock"), b"");
        let s = session(
            AgentKind::Codex,
            "x",
            vec![Artifact::JsonlLines {
                file: index.clone(),
                field: "id".into(),
                value: "x".into(),
            }],
            recent(),
        );
        let trash = Trash::new(tmp.path().join("trash"));
        let entry = trash
            .execute(&plan_with(&home, vec![s], true, &mut |_| false))
            .unwrap();
        assert!(entry.errors.is_empty());
        assert_eq!(fs::read(&index).unwrap(), b"{\"id\":\"y\"}\n");
        trash.restore(&entry.id).unwrap();
        assert_eq!(
            fs::read(&index).unwrap(),
            b"{\"id\":\"x\"}\n{\"id\":\"y\"}\n"
        );
    }

    #[test]
    fn plan_refuses_unsafe_paths_even_when_forced() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        // Built by hand: `session()` would size the "/" directory, which walks the whole disk.
        let bare = |id: &str, artifact: Artifact| Session {
            parent_id: None,
            agent: AgentKind::Codex,
            id: id.into(),
            title: None,
            project: None,
            created: None,
            last_active: None,
            message_count: None,
            bytes: 0,
            artifacts: vec![artifact],
        };
        let sessions = vec![
            bare("home", Artifact::Dir(home.clone())),
            bare("root", Artifact::Dir(PathBuf::from("/"))),
            bare("rel", Artifact::File(PathBuf::from("relative/x"))),
        ];
        let p = plan_with(&home, sessions, true, &mut |_| false);
        assert!(p.sessions.is_empty());
        assert_eq!(p.blocked.len(), 3);
    }

    #[test]
    fn failed_session_is_rolled_back_and_others_continue() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let a = home.join("a.txt");
        let b = home.join("b.txt");
        let broken = home.join("broken.json");
        write(&a, b"A");
        write(&b, b"B");
        write(&broken, b"{ not json");
        let trash = trash_in(&tmp);
        let sessions = vec![
            session(AgentKind::Codex, "ok", vec![Artifact::File(a.clone())], None),
            session(
                AgentKind::Claude,
                "bad",
                vec![
                    Artifact::File(b.clone()),
                    Artifact::JsonArrayEntry {
                        file: broken.clone(),
                        array_path: "".into(),
                        field: "id".into(),
                        value: "x".into(),
                    },
                ],
                None,
            ),
        ];
        let entry = trash
            .execute(&plan_with(&home, sessions, false, &mut |_| false))
            .unwrap();
        assert_eq!(entry.sessions.len(), 1);
        assert_eq!(entry.sessions[0].id, "ok");
        assert_eq!(entry.errors.len(), 1);
        assert!(entry.errors[0].starts_with("claude:bad"));
        assert!(!a.exists());
        assert_eq!(fs::read(&b).unwrap(), b"B", "failed session's file was put back");

        trash.restore(&entry.id).unwrap();
        assert_eq!(fs::read(&a).unwrap(), b"A");
    }

    #[test]
    fn execute_with_no_sessions_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = trash_in(&tmp);
        let empty = DeletePlan {
            sessions: vec![],
            bytes: 0,
            blocked: vec![],
        };
        assert!(trash.execute(&empty).is_err());
        assert!(trash.list().unwrap().is_empty());
    }

    #[test]
    fn missing_paths_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let trash = trash_in(&tmp);
        let plan = plan_with(
            &home,
            vec![session(
                AgentKind::Codex,
                "gone",
                vec![
                    Artifact::File(home.join("nope.txt")),
                    Artifact::DbRows {
                        db: home.join("nope.db"),
                        deletes: vec![],
                        bytes: 0,
                    },
                    Artifact::JsonlLines {
                        file: home.join("nope.jsonl"),
                        field: "id".into(),
                        value: "gone".into(),
                    },
                ],
                recent(),
            )],
            false,
            &mut |_| false,
        );
        let entry = trash.execute(&plan).unwrap();
        assert!(entry.ops.is_empty());
        assert!(!home.join("nope.db").exists(), "a missing database is not created");
    }

    #[test]
    fn restore_refuses_when_destination_is_taken_and_keeps_the_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let a = home.join("a.txt");
        write(&a, b"A");
        let trash = trash_in(&tmp);
        let entry = trash
            .execute(&plan_with(
                &home,
                vec![session(AgentKind::Codex, "a", vec![Artifact::File(a.clone())], None)],
                false,
                &mut |_| false,
            ))
            .unwrap();

        write(&a, b"NEW");
        assert!(trash.restore(&entry.id).is_err());
        assert!(trash.root.join(&entry.id).exists(), "entry kept after refused restore");
        assert_eq!(fs::read(&a).unwrap(), b"NEW", "nothing overwritten");

        fs::remove_file(&a).unwrap();
        trash.restore(&entry.id).unwrap();
        assert_eq!(fs::read(&a).unwrap(), b"A");
    }

    #[test]
    fn restore_and_empty_reject_path_like_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = trash_in(&tmp);
        assert!(trash.restore("../etc").is_err());
        assert!(trash.restore("").is_err());
        assert!(trash.empty(Some("a/b")).is_err());
    }

    #[test]
    fn list_is_newest_first_and_empty_frees_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let x = home.join("x.txt");
        let y = home.join("y.txt");
        write(&x, b"12345");
        let trash = trash_in(&tmp);
        let first = trash
            .execute(&plan_with(
                &home,
                vec![session(AgentKind::Codex, "x", vec![Artifact::File(x.clone())], None)],
                false,
                &mut |_| false,
            ))
            .unwrap();
        write(&y, b"yy");
        let second = trash
            .execute(&plan_with(
                &home,
                vec![session(AgentKind::Codex, "y", vec![Artifact::File(y.clone())], None)],
                false,
                &mut |_| false,
            ))
            .unwrap();

        let listed: Vec<String> = trash.list().unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(listed, vec![second.id.clone(), first.id.clone()]);

        let freed = trash.empty(Some(&first.id)).unwrap();
        assert!(freed >= 5, "freed {freed} bytes");
        assert_eq!(trash.list().unwrap().len(), 1);

        let rest = trash.empty(None).unwrap();
        assert!(rest >= 2);
        assert!(trash.list().unwrap().is_empty());
        assert!(!x.exists() && !y.exists(), "emptied entries are not restorable");
    }

    #[test]
    fn copy_fallback_reproduces_a_tree_with_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        write(&src.join("a/b.txt"), b"hello");
        #[cfg(unix)]
        std::os::unix::fs::symlink("a/b.txt", src.join("link")).unwrap();
        let dst = tmp.path().join("dst");
        copy_path(&src, &dst).unwrap();
        assert_eq!(fs::read(dst.join("a/b.txt")).unwrap(), b"hello");
        #[cfg(unix)]
        assert_eq!(fs::read_link(dst.join("link")).unwrap(), PathBuf::from("a/b.txt"));
        remove_path(&src).unwrap();
        assert!(!src.exists());
    }

    #[test]
    fn stored_path_drops_root_and_refuses_parent_escape() {
        assert_eq!(stored_rel(Path::new("/a/b/c.txt")), PathBuf::from("files/a/b/c.txt"));
        assert_eq!(stored_rel(Path::new("/a/../b")), PathBuf::from("files/a/b"));
        assert!(ensure_safe(Path::new("/")).is_err());
        assert!(ensure_safe(Path::new("rel/x")).is_err());
        assert!(ensure_safe(Path::new("/a")).is_ok());
    }
}
