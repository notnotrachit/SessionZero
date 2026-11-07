# sz-core engine API (spec for scan / group / rules / cache / trash)

Implement EXACTLY these public APIs. The TUI/CLI code is written against them.

## paths.rs
- `pub fn data_dir() -> PathBuf` = `$SZ_DATA_DIR` or `~/.sessionzero` (create on demand)
- `pub fn trash_dir() -> PathBuf`, `pub fn cache_file() -> PathBuf`

## scan.rs
```rust
pub enum ScanEvent {
    AgentStarted(AgentKind),
    AgentDone { agent: AgentKind, sessions: Vec<Session>, error: Option<String>, millis: u128 },
    Finished,
}
pub fn scan_all(home: &Path, only: Option<&[AgentKind]>) -> Vec<Session>; // blocking; detected adapters in parallel (rayon); errors logged + skipped
pub fn scan_stream(home: PathBuf, only: Option<Vec<AgentKind>>) -> std::sync::mpsc::Receiver<ScanEvent>; // thread per adapter, events as each finishes
pub struct AgentInfo { pub kind: AgentKind, pub detected: bool, pub running: bool, pub roots: Vec<(PathBuf, bool /*exists*/)> }
pub fn agent_infos(home: &Path) -> Vec<AgentInfo>;
```

## group.rs
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum GroupBy { Project, Agent, Folder }
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum SortBy { Size, Recent, Oldest, Messages, Name }
pub struct Group {
    pub key: String, pub label: String, pub bytes: u64, pub count: usize,
    pub last_active: Option<DateTime<Utc>>,
    pub children: Vec<Group>,
    pub sessions: Vec<usize>, // indices into the input slice, sorted; only on leaf groups
}
pub fn group(sessions: &[Session], by: GroupBy, sort: SortBy) -> Vec<Group>;
pub fn sort_indices(sessions: &[Session], idx: &mut Vec<usize>, sort: SortBy);
pub fn filter(sessions: &[Session], pattern: &str) -> Vec<usize>; // case-insensitive regex over title, project, agent label, id; invalid regex -> literal substring
pub fn project_root(p: &Path) -> PathBuf; // nearest ancestor containing .git (memoized), else p
pub fn display_path(p: &Path) -> String; // home prefix -> ~
pub fn human_bytes(b: u64) -> String;    // "1.2 GB", 1024-based
```
Levels: Project -> Agent -> sessions; Agent -> Project -> sessions; Folder (parent dir of project root) -> Project -> sessions.
No project -> group key `(no project)`. Parents aggregate bytes / count / last_active. Groups and children sorted by `sort`.

## rules.rs
```rust
#[derive(Default, Clone, Debug)]
pub struct Rule {
    pub older_than: Option<chrono::Duration>, pub larger_than: Option<u64>,
    pub agents: Vec<AgentKind>, pub project: Option<String> /*regex*/, pub no_project: bool,
}
pub fn parse_duration(s: &str) -> anyhow::Result<chrono::Duration>; // 30d 12h 2w 6m(=30d months) 1y
pub fn parse_size(s: &str) -> anyhow::Result<u64>;                // 500KB 100MB 1.5GB 2G, plain bytes; 1KB = 1024
pub fn parse_agent(s: &str) -> Option<AgentKind>;                 // labels + aliases (kiro-cli, kirocrew, antigravity -> Gemini)
impl Rule { pub fn matches(&self, s: &Session, now: DateTime<Utc>) -> bool; pub fn is_empty(&self) -> bool; }
pub fn select(sessions: &[Session], rule: &Rule, now: DateTime<Utc>) -> Vec<usize>;
```
`older_than` uses last_active, falling back to created. Unknown dates never match `older_than`. All set fields must match (AND).

## cache.rs
- `pub fn load() -> Option<Vec<Session>>`, `pub fn load_if_fresh(max_age: chrono::Duration) -> Option<Vec<Session>>`
- `pub fn save(sessions: &[Session]) -> anyhow::Result<()>`: JSON `{saved_at, sessions}` at `paths::cache_file()`, atomic (tmp + rename).

## model.rs
- `Session` gained `#[serde(default)] pub parent_id: Option<String>`. It is set from the agent's own parent link: Kiro CLI `parent_session_id`, OpenCode `session.parent_id` (v1 and v2 rows), Grok `summary.json` `parent_session_id`, and Antigravity/Gemini `parent_conversation_id`. Codex, Claude, Cursor and Kiro Crew record no session-level parent, so it stays `None`.
- `Session::new(agent, id) -> Session`: every other field empty. Use it instead of a struct literal.

## adapter.rs
- `Adapter::live_session_ids(&self) -> Vec<String>` (default: empty). Overridden by kiro_cli, kiro_crew, opencode, grok, gemini and cursor. `plan` uses it.

## util.rs
- `running_processes(names)`: matches a process when one of `names` equals a whole token of its name, an executable path component, or a command-line word's last `/` segment. Case-insensitive. CPU and memory are not refreshed.

## cache.rs: ParseCache (per-file parse results)
- `ParseCache::load(file: Option<PathBuf>) -> ParseCache`: `None` keeps it in memory only.
- `ParseCache::for_home(home, agent_label) -> ParseCache`: persisted to `<data_dir>/parse-cache/<agent>.json` only when `home` is the real home directory; other homes stay in memory.
- `get_or_parse(path, parse) -> Option<T>`: returns the cached value when the file's size and mtime still match, else runs `parse` and caches a `Some` result. `None` is never cached. Takes the stamp before parsing.
- `get`, `save()`: `save` writes only entries seen in this run, so entries for deleted files drop out.
- Used by codex (per rollout), kiro_cli (per CLI and agent-mode transcript).

## cache.rs: KeyedCache (values keyed by a string, checked by a fingerprint)
- `KeyedCache::<V>::load(file: Option<PathBuf>)`: `None` keeps it in memory only. `V: Serialize + DeserializeOwned + Clone`.
- `KeyedCache::for_home(home, name)`: persisted to `<data_dir>/parse-cache/<name>.json` only when `home` is the real home directory.
- `get(key, fp) -> Option<V>`: returns the stored value only when its fingerprint equals `fp`. A hit is kept for the next save.
- `put(key, fp, value)`: stores a value under a fingerprint, replacing this run's earlier entry.
- `save()`: writes only entries put or hit during this run, atomically, so entries for deleted keys drop out.
- Used by opencode for per-session DB byte sums (`opencode-sessions`, keyed by session id; fingerprint = row count, max and sum of `time_updated` for `part`, `message` and `session_message`) and for legacy `storage/` directory sizes (`opencode-dirs`, keyed by path; fingerprint = directory mtime).

## Project placeholders (util.rs, scan.rs, group.rs)
- `util::is_placeholder_project(p, home)`: true for `/` and the home directory. `util::normalize_project(&mut Option<PathBuf>, home)` clears them.
- Every adapter's scan result is normalized in `scan_all` and `scan_stream`, so a Kiro CLI subagent with cwd `/` has `project: None`. `group` also ignores placeholder projects from older caches.

## trash.rs (must be safe and reversible)
```rust
pub struct Blocked { pub key: String, pub reason: String }
pub struct DeletePlan { pub sessions: Vec<Session>, pub bytes: u64, pub blocked: Vec<Blocked> }
pub fn plan(home: &Path, sessions: Vec<Session>, force: bool) -> DeletePlan;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TrashEntry { pub id: String, pub created: DateTime<Utc>, pub bytes: u64, pub sessions: Vec<Session>, pub ops: Vec<TrashOp> }
// TrashOp: your serde enum recording exactly what was done and how to undo it.

pub struct Trash { root: PathBuf }
impl Trash {
    pub fn new(root: PathBuf) -> Self;
    pub fn default_location() -> Self; // paths::trash_dir()
    pub fn execute(&self, plan: &DeletePlan) -> anyhow::Result<TrashEntry>;
    pub fn list(&self) -> anyhow::Result<Vec<TrashEntry>>; // newest first
    pub fn restore(&self, id: &str) -> anyhow::Result<()>;
    pub fn empty(&self, id: Option<&str>) -> anyhow::Result<u64>; // bytes freed; then VACUUM each touched db (ignore lock errors)
}
```

### plan()
Unless `force`, block a session when its id is in its adapter's `live_session_ids()` (reason `session is open in <agent>`). Otherwise block it when its agent is running (adapter `process_names` + `util::running_processes`) AND any of:
last_active within 2 hours, last_active unknown, or it has a `DbRows`/`DbPrefix` artifact (writing a live DB is unsafe).
Reasons are human-readable. `bytes` = sum over non-blocked sessions. Make the running check injectable for tests.
- `pub fn plan_with_live(home, sessions, force, is_running: &mut dyn FnMut(AgentKind) -> bool, live_ids: &mut dyn FnMut(AgentKind) -> Vec<String>) -> DeletePlan`: `plan` with both checks injected. `plan_with` is the same with live ids treated as unknown.

### with_children()
`pub fn with_children(all: &[Session], picked: Vec<Session>) -> Vec<Session>`: adds the children of every picked session, at any depth, found in `all`. A child is a session of the same agent whose `parent_id` equals the parent's id. Picked sessions come first, then children in `all` order. No session appears twice. Call it before `plan` so a parent is not deleted while its children stay behind.

### execute()
Entry dir `trash/<YYYYMMDD-HHMMSS>-<4 random hex>/` with `manifest.json`.
Shared files (`JsonlLines`, `JsonArrayEntry`, `JsonObjectKey`) are rewritten under the sibling `<file>.lock` when it exists: an exclusive advisory lock is taken (waits up to 5 s, then fails) and the file is re-read right before the rename. If it changed, the edit is redone. Restore takes the same lock.
- `File` / `Dir`: move to `entry/files/<absolute path without leading />` (`fs::rename`, fallback copy + remove across devices). Skip missing paths.
- `DbRows` / `DbPrefix`: open read-write, `BEGIN IMMEDIATE`, SELECT matching rows (all columns, blobs base64, keep types) to `entry/db/<n>.json`, DELETE, COMMIT. Prefix match: `substr(col,1,length(?1)) = ?1`.
- `JsonlLines`: atomically rewrite without lines whose dotted `field` equals `value` (compare as string). Save removed lines + line numbers.
- `JsonArrayEntry` / `JsonObjectKey`: parse, remove, save removed value (+ index), atomic write. Keep pretty vs compact style.
- If an op fails mid-session, roll back that session's finished ops and record the error. Continue with other sessions. Return Err only if all failed. Write the manifest after each session.

### restore()
Undo ops in reverse: move back (only if destination free), `INSERT OR IGNORE` rows, reinsert JSON lines/entries at saved positions. Then remove the entry dir.

## Tests (tempfile only, never the real home)
Rule parsing/matching; grouping; filter; trash round trip for EVERY Artifact variant (files, dir, jsonl, json array, json object, sqlite rows + prefix rows): execute -> verify gone -> restore -> files byte-identical, row sets equal; plan blocking with the injected running check.
