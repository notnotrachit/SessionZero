//! Interactive TUI application state, key handling, background jobs and the event loop.
//!
//! All state transitions happen in `App` methods that do not touch the terminal, so they can be
//! tested directly. Side effects (scans, trash IO, previews, opening folders) are queued as
//! `Command`s and run by `dispatch_pending` on worker threads; results come back as `JobResult`s.

use std::collections::{HashMap, HashSet};
use std::io;
use std::panic;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute, terminal};
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::Terminal;
use sz_core::adapters::all_adapters;
use sz_core::cache;
use sz_core::config::{self, Config};
use sz_core::group::{self, Group, GroupBy, SortBy};
use sz_core::model::{AgentKind, PreviewMsg, Session};
use sz_core::paths;
use sz_core::scan::{self, AgentInfo, ScanEvent};
use sz_core::trash::{self, DeletePlan, PurgeReport, Trash, TrashEntry};

use crate::ui;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Menu,
    List,
    Analyze,
    Trash,
}

pub struct Options {
    pub home: PathBuf,
    pub force: bool,
    pub agents: Option<Vec<AgentKind>>,
}

/// Screens the app can show. `Mode` is the entry point; `Agents` is only reachable from the menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Menu,
    List,
    Analyze,
    Trash,
    Agents,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuItem {
    Analyze,
    List,
    Trash,
    Agents,
    Quit,
}

pub const MENU_ITEMS: [MenuItem; 5] = [
    MenuItem::Analyze,
    MenuItem::List,
    MenuItem::Trash,
    MenuItem::Agents,
    MenuItem::Quit,
];

pub enum Modal {
    Help,
    Delete(DeleteModal),
    Trash(TrashModal),
}

pub enum DeleteModal {
    /// The plan is being computed on a worker thread. `seq` ties the reply to this request.
    Checking { seq: u64, count: usize },
    /// `children` of the sessions in `plan` were added because their parent was picked.
    Confirm { plan: DeletePlan, children: usize },
    Running { count: usize },
}

pub enum TrashModal {
    Restore(String),
    EmptyOne(String),
    /// Typed confirmation buffer; must read `yes` before Enter empties the whole trash.
    EmptyAll(String),
}

/// Side effects requested by key handling. Run by `App::dispatch_pending`.
pub enum Command {
    Rescan,
    /// Adds child sessions, then computes the delete plan. Runs off the UI thread.
    Plan { seq: u64, picked: Vec<Session> },
    Delete(DeletePlan),
    Restore(String),
    Empty(Option<String>),
    OpenFolder(PathBuf),
    Preview { key: String, session: Session },
}

/// Results of background work, delivered over a channel and applied on the UI thread.
pub enum JobResult {
    AgentInfos(Vec<AgentInfo>),
    Planned {
        seq: u64,
        plan: DeletePlan,
        children: usize,
    },
    Deleted(Result<TrashEntry, String>),
    /// `bytes` is the size of the entry being restored, read before the restore started.
    Restored {
        id: String,
        bytes: u64,
        result: Result<(), String>,
    },
    /// `count` is how many entries the empty removed, counted before it ran.
    Emptied {
        count: usize,
        result: Result<u64, String>,
    },
    Preview { key: String, result: Result<Vec<PreviewMsg>, String> },
    /// The startup auto-delete pass removed entries. Only sent when it removed something.
    AutoPurged { report: PurgeReport, days: u32 },
    /// The startup auto-delete pass failed.
    PurgeFailed(String),
    Info(String),
}

/// Trash contents at exit, shown in the session summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashNow {
    pub entries: usize,
    pub bytes: u64,
    /// 0 keeps entries forever.
    pub retention_days: u32,
    /// Whole days until the oldest entry is auto-deleted. `None` when the trash is empty.
    pub days_until_next: Option<i64>,
}

/// What this TUI session did to disk. Filled in as results arrive on the UI thread, then printed
/// after the terminal is restored on exit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionStats {
    pub moved_sessions: usize,
    pub moved_bytes: u64,
    pub moved_entries: usize,
    pub restored_entries: usize,
    pub restored_bytes: u64,
    pub emptied_entries: usize,
    pub emptied_bytes: u64,
    pub purged_entries: usize,
    pub purged_bytes: u64,
    pub error_count: usize,
    /// The first error message, untruncated. Truncated when rendered.
    pub first_error: Option<String>,
    /// Trash contents at exit. Read from disk only when something changed.
    pub trash_now: Option<TrashNow>,
}

const SUMMARY_ERROR_CHARS: usize = 120;
const TRASH_NOTE: &str = "Space is freed when you empty the trash.";

impl SessionStats {
    /// True when at least one entry was moved, restored, emptied or auto-deleted.
    pub fn has_disk_change(&self) -> bool {
        self.moved_entries > 0
            || self.restored_entries > 0
            || self.emptied_entries > 0
            || self.purged_entries > 0
    }

    pub fn record_error(&mut self, msg: impl Into<String>) {
        self.error_count += 1;
        if self.first_error.is_none() {
            self.first_error = Some(msg.into());
        }
    }

    /// Summary lines for stdout. Empty when this session changed nothing on disk.
    pub fn render_lines(&self) -> Vec<String> {
        if !self.has_disk_change() {
            return Vec::new();
        }
        let mut lines = Vec::new();
        if self.moved_entries > 0 {
            let not_freed = if self.moved_bytes > 0 { " (not freed yet)" } else { "" };
            lines.push(format!(
                "Moved to trash this session: {} ({}) in {}{not_freed}",
                count_noun(self.moved_sessions, "session", "sessions"),
                group::human_bytes(self.moved_bytes),
                count_noun(self.moved_entries, "trash entry", "trash entries"),
            ));
        }
        if self.restored_entries > 0 {
            lines.push(format!(
                "Restored from trash this session: {} ({})",
                count_noun(self.restored_entries, "entry", "entries"),
                group::human_bytes(self.restored_bytes),
            ));
        }
        if self.emptied_entries > 0 {
            lines.push(format!(
                "Permanently deleted from trash this session (x / X): {}, {} freed on disk.",
                count_noun(self.emptied_entries, "entry", "entries"),
                group::human_bytes(self.emptied_bytes),
            ));
        }
        if self.purged_entries > 0 {
            lines.push(format!(
                "Auto-deleted by the startup trash purge this session: {}, {} freed on disk.",
                count_noun(self.purged_entries, "entry", "entries"),
                group::human_bytes(self.purged_bytes),
            ));
        }
        if let Some(trash) = &self.trash_now {
            lines.push(format!("Trash now: {}", trash.describe()));
            if self.moved_bytes > 0 {
                lines.push(TRASH_NOTE.to_string());
            }
        }
        if let Some(first) = &self.first_error {
            lines.push(format!(
                "Errors this session: {} (first: {})",
                self.error_count,
                truncate_chars(&first.replace('\n', " "), SUMMARY_ERROR_CHARS),
            ));
        }
        lines
    }
}

impl TrashNow {
    fn describe(&self) -> String {
        let base = format!(
            "{}, {}",
            count_noun(self.entries, "entry", "entries"),
            group::human_bytes(self.bytes)
        );
        if self.entries == 0 {
            base
        } else if self.retention_days == 0 {
            format!("{base}; kept forever")
        } else {
            match self.days_until_next {
                Some(0) => format!("{base}; next auto-delete today"),
                Some(1) => format!("{base}; next auto-delete in 1 day"),
                Some(n) => format!("{base}; next auto-delete in {n} days"),
                None => base,
            }
        }
    }
}

fn count_noun(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Cuts `s` to at most `max` characters, ending with `...` when it was cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 3).collect();
    out.push_str("...");
    out
}

/// One line of the List or Analyze table. Group rows carry every session beneath them.
#[derive(Clone, Debug)]
pub struct Row {
    pub key: String,
    pub label: String,
    pub bytes: u64,
    pub count: usize,
    pub last: Option<DateTime<Utc>>,
    /// Global indices into `App::sessions`.
    pub sessions: Vec<usize>,
    pub is_session: bool,
    pub agent: Option<AgentKind>,
    pub project: Option<PathBuf>,
    pub msgs: Option<u64>,
    pub live: bool,
}

const NO_PROJECT: &str = "(no project)";
const TOAST_TTL: Duration = Duration::from_secs(8);
const PREVIEW_MSGS: usize = 20;
const LIVE_WINDOW_HOURS: i64 = 2;
const PAGE: isize = 10;

pub struct App {
    pub home: PathBuf,
    pub force: bool,
    pub agent_filter: Option<Vec<AgentKind>>,
    pub trash_root: PathBuf,

    pub screen: Screen,
    pub menu_cursor: usize,
    pub list_cursor: usize,
    pub analyze_cursor: usize,
    pub trash_cursor: usize,
    pub agents_cursor: usize,

    pub sessions: Vec<Session>,
    /// True while showing sessions loaded from the cache and no scan has finished yet.
    pub cached: bool,
    pub scanning: bool,
    pub scan_total: usize,
    pub scan_done: usize,
    scan_reported: HashSet<AgentKind>,
    pub scan_errors: HashMap<AgentKind, String>,
    pub agent_infos: Vec<AgentInfo>,
    pub running: HashSet<AgentKind>,

    pub sort: SortBy,
    pub group_by: GroupBy,
    pub filter: Option<String>,
    pub filter_input: Option<String>,
    /// Indices into `sessions` that pass the filter, in sort order.
    pub visible: Vec<usize>,
    /// Analyze tree built from `visible`. Its session indices point into `visible`.
    pub tree: Vec<Group>,
    pub analyze_path: Vec<String>,
    pub selected: HashSet<String>,
    pub range_anchor: Option<usize>,

    pub preview_on: bool,
    pub previews: HashMap<String, Result<Vec<PreviewMsg>, String>>,
    preview_pending: HashSet<String>,

    pub modal: Option<Modal>,
    toast: Option<(String, Instant)>,
    pub busy: Option<String>,
    pub trash_entries: Vec<TrashEntry>,
    /// Days before trash entries are auto-deleted. 0 keeps them forever. Shown on the trash screen.
    pub trash_retention_days: u32,
    /// Last delete-plan request. Replies carrying an older `seq` are dropped.
    plan_seq: u64,
    /// Disk changes made in this session, printed on exit.
    pub stats: SessionStats,

    pending: Vec<Command>,
    scan_rx: Option<Receiver<ScanEvent>>,
    jobs_tx: Sender<JobResult>,
    jobs_rx: Receiver<JobResult>,
    pub tick: usize,
    pub quit: bool,
    /// Write the scan cache when a scan finishes. Off in tests and snapshots.
    pub persist: bool,
}

impl App {
    pub fn new(
        opts: Options,
        trash_root: PathBuf,
        mode: Mode,
        sessions: Vec<Session>,
        cached: bool,
    ) -> Self {
        let (jobs_tx, jobs_rx) = mpsc::channel();
        let screen = match mode {
            Mode::Menu => Screen::Menu,
            Mode::List => Screen::List,
            Mode::Analyze => Screen::Analyze,
            Mode::Trash => Screen::Trash,
        };
        let mut app = App {
            home: opts.home,
            force: opts.force,
            agent_filter: opts.agents,
            trash_root,
            screen,
            menu_cursor: 0,
            list_cursor: 0,
            analyze_cursor: 0,
            trash_cursor: 0,
            agents_cursor: 0,
            sessions,
            cached,
            scanning: false,
            scan_total: 0,
            scan_done: 0,
            scan_reported: HashSet::new(),
            scan_errors: HashMap::new(),
            agent_infos: Vec::new(),
            running: HashSet::new(),
            sort: SortBy::Size,
            group_by: GroupBy::Project,
            filter: None,
            filter_input: None,
            visible: Vec::new(),
            tree: Vec::new(),
            analyze_path: Vec::new(),
            selected: HashSet::new(),
            range_anchor: None,
            preview_on: false,
            previews: HashMap::new(),
            preview_pending: HashSet::new(),
            modal: None,
            toast: None,
            busy: None,
            trash_entries: Vec::new(),
            trash_retention_days: config::DEFAULT_TRASH_RETENTION_DAYS,
            plan_seq: 0,
            stats: SessionStats::default(),
            pending: Vec::new(),
            scan_rx: None,
            jobs_tx,
            jobs_rx,
            tick: 0,
            quit: false,
            persist: false,
        };
        app.refresh();
        if screen == Screen::Trash {
            app.load_trash();
        }
        app
    }

    // ----- scan and background results -------------------------------------------------

    /// Starts a streaming scan and the agent-info probe. Called by `run` and by `Rescan`.
    pub fn start_scan(&mut self) {
        self.scanning = true;
        self.scan_total = 0;
        self.scan_done = 0;
        self.scan_reported.clear();
        self.scan_errors.clear();
        self.scan_rx = Some(scan::scan_stream(self.home.clone(), self.agent_filter.clone()));
        let tx = self.jobs_tx.clone();
        let home = self.home.clone();
        thread::spawn(move || {
            let _ = tx.send(JobResult::AgentInfos(scan::agent_infos(&home)));
        });
    }

    /// Merges one scan event into the session list.
    pub fn apply_scan_event(&mut self, ev: ScanEvent) {
        match ev {
            ScanEvent::AgentStarted(_) => self.scan_total += 1,
            ScanEvent::AgentDone {
                agent,
                sessions,
                error,
                ..
            } => {
                self.scan_done += 1;
                self.scan_reported.insert(agent);
                self.sessions.retain(|s| s.agent != agent);
                self.sessions.extend(sessions);
                match error {
                    Some(e) => {
                        self.scan_errors.insert(agent, e);
                    }
                    None => {
                        self.scan_errors.remove(&agent);
                    }
                }
                self.refresh();
            }
            ScanEvent::Finished => {
                self.finish_scan();
            }
        }
    }

    /// Drops cached rows for agents that did not report in this scan, then saves the cache on a
    /// worker thread so the UI never waits on disk.
    fn finish_scan(&mut self) {
        self.scanning = false;
        self.cached = false;
        let reported = &self.scan_reported;
        self.sessions.retain(|s| reported.contains(&s.agent));
        self.refresh();
        if self.persist {
            let sessions = self.sessions.clone();
            let tx = self.jobs_tx.clone();
            thread::spawn(move || {
                if let Err(e) = cache::save(&sessions) {
                    let _ = tx.send(JobResult::Info(format!("could not save scan cache: {e:#}")));
                }
            });
        }
    }

    /// Drains scan events and job results without blocking. Call once per frame.
    pub fn poll_background(&mut self) {
        if let Some(rx) = self.scan_rx.take() {
            let mut keep = true;
            loop {
                match rx.try_recv() {
                    Ok(ev) => {
                        let finished = matches!(ev, ScanEvent::Finished);
                        self.apply_scan_event(ev);
                        if finished {
                            keep = false;
                            break;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        // The scan thread died without sending Finished.
                        if self.scanning {
                            self.finish_scan();
                        }
                        keep = false;
                        break;
                    }
                }
            }
            if keep {
                self.scan_rx = Some(rx);
            }
        }
        while let Ok(job) = self.jobs_rx.try_recv() {
            self.apply_job(job);
        }
    }

    fn apply_job(&mut self, job: JobResult) {
        match job {
            JobResult::AgentInfos(infos) => {
                self.running = infos.iter().filter(|i| i.running).map(|i| i.kind).collect();
                self.agent_infos = infos;
            }
            JobResult::Planned { seq, plan, children } => {
                let current = matches!(
                    self.modal,
                    Some(Modal::Delete(DeleteModal::Checking { seq: s, .. })) if s == seq
                );
                if current {
                    self.modal = Some(Modal::Delete(DeleteModal::Confirm { plan, children }));
                }
            }
            JobResult::Deleted(Ok(entry)) => {
                self.busy = None;
                self.modal = None;
                let moved: HashSet<String> = entry.sessions.iter().map(|s| s.key()).collect();
                let n = entry.sessions.len();
                self.stats.moved_sessions += n;
                self.stats.moved_bytes += entry.bytes;
                if n > 0 {
                    self.stats.moved_entries += 1;
                }
                for err in &entry.errors {
                    self.stats.record_error(err.clone());
                }
                self.sessions.retain(|s| !moved.contains(&s.key()));
                self.selected.retain(|k| !moved.contains(k));
                self.refresh();
                let mut msg = format!(
                    "Moved {n} sessions ({}) to trash · t to view",
                    group::human_bytes(entry.bytes)
                );
                if let Some(first) = entry.errors.first() {
                    msg.push_str(&format!(" · {} failed: {first}", entry.errors.len()));
                }
                self.set_toast(msg);
            }
            JobResult::Deleted(Err(e)) => {
                self.busy = None;
                self.modal = None;
                self.stats.record_error(e.clone());
                self.set_toast(format!("Delete failed: {e}"));
            }
            JobResult::Restored {
                id,
                bytes,
                result: Ok(()),
            } => {
                self.busy = None;
                self.stats.restored_entries += 1;
                self.stats.restored_bytes += bytes;
                self.set_toast(format!("Restored {id}. Rescanning…"));
                if !self.scanning {
                    self.pending.push(Command::Rescan);
                }
                self.load_trash();
            }
            JobResult::Restored {
                id,
                result: Err(e),
                ..
            } => {
                self.busy = None;
                self.stats.record_error(e.clone());
                self.set_toast(format!("Restore {id} failed: {e}"));
            }
            JobResult::Emptied {
                count,
                result: Ok(bytes),
            } => {
                self.busy = None;
                self.stats.emptied_entries += count;
                self.stats.emptied_bytes += bytes;
                self.set_toast(format!("Trash emptied, freed {}", group::human_bytes(bytes)));
                self.load_trash();
            }
            JobResult::Emptied {
                result: Err(e), ..
            } => {
                self.busy = None;
                self.stats.record_error(e.clone());
                self.set_toast(format!("Empty failed: {e}"));
            }
            JobResult::Preview { key, result } => {
                self.preview_pending.remove(&key);
                self.previews.insert(key, result);
            }
            JobResult::AutoPurged { report, days } => {
                self.stats.purged_entries += report.entries;
                self.stats.purged_bytes += report.bytes;
                let unit = if days == 1 { "day" } else { "days" };
                self.set_toast(format!(
                    "Auto-deleted {} ({}) older than {days} {unit}",
                    if report.entries == 1 {
                        "1 trash entry".to_string()
                    } else {
                        format!("{} trash entries", report.entries)
                    },
                    group::human_bytes(report.bytes)
                ));
                self.load_trash();
            }
            JobResult::PurgeFailed(msg) => {
                self.stats.record_error(msg.clone());
                self.set_toast(msg);
            }
            JobResult::Info(msg) => self.set_toast(msg),
        }
    }

    /// Trash contents right now, for the exit summary. `None` when the trash cannot be read.
    pub fn trash_now(&self) -> Option<TrashNow> {
        let entries = Trash::new(self.trash_root.clone()).list().ok()?;
        let config = Config {
            trash_retention_days: self.trash_retention_days,
        };
        let now = Utc::now();
        let oldest = entries.iter().map(|e| e.created).min();
        Some(TrashNow {
            entries: entries.len(),
            bytes: entries.iter().map(|e| e.bytes).sum(),
            retention_days: self.trash_retention_days,
            days_until_next: oldest.and_then(|t| config.days_until_purge(t, now)),
        })
    }

    /// Runs the automatic trash deletion on a worker thread so startup never waits for it. The
    /// result arrives as a toast, and only when entries were removed.
    pub fn start_auto_purge(&self, config: Config) {
        let tx = self.jobs_tx.clone();
        let root = self.trash_root.clone();
        thread::spawn(move || {
            let trash = Trash::new(root);
            match config::auto_purge(&trash, &config, Utc::now()) {
                Ok(report) if report.entries > 0 => {
                    let _ = tx.send(JobResult::AutoPurged {
                        report,
                        days: config.trash_retention_days,
                    });
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = tx.send(JobResult::PurgeFailed(format!(
                        "Automatic trash deletion failed: {e:#}"
                    )));
                }
            }
        });
    }

    /// Shows a message in the footer toast.
    pub fn notify(&mut self, msg: impl Into<String>) {
        self.set_toast(msg);
    }

    /// Runs queued commands on worker threads. Call once per frame after input is handled.
    pub fn dispatch_pending(&mut self) {
        for cmd in std::mem::take(&mut self.pending) {
            self.dispatch(cmd);
        }
    }

    fn dispatch(&mut self, cmd: Command) {
        let tx = self.jobs_tx.clone();
        match cmd {
            Command::Rescan => self.start_scan(),
            Command::Plan { seq, picked } => {
                let all = self.sessions.clone();
                let home = self.home.clone();
                let force = self.force;
                thread::spawn(move || {
                    let picked_n = picked.len();
                    let full = trash::with_children(&all, picked);
                    let children = full.len().saturating_sub(picked_n);
                    let plan = trash::plan(&home, full, force);
                    let _ = tx.send(JobResult::Planned {
                        seq,
                        plan,
                        children,
                    });
                });
            }
            Command::Delete(plan) => {
                let root = self.trash_root.clone();
                thread::spawn(move || {
                    let res = Trash::new(root).execute(&plan).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(JobResult::Deleted(res));
                });
            }
            Command::Restore(id) => {
                let root = self.trash_root.clone();
                let bytes = self
                    .trash_entries
                    .iter()
                    .find(|e| e.id == id)
                    .map_or(0, |e| e.bytes);
                thread::spawn(move || {
                    let result = Trash::new(root).restore(&id).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(JobResult::Restored { id, bytes, result });
                });
            }
            Command::Empty(id) => {
                let root = self.trash_root.clone();
                thread::spawn(move || {
                    let trash = Trash::new(root);
                    // Counted before the empty runs, since the entries are gone afterwards.
                    let count = match id {
                        Some(_) => 1,
                        None => trash.list().map_or(0, |entries| entries.len()),
                    };
                    let result = trash
                        .empty(id.as_deref())
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(JobResult::Emptied { count, result });
                });
            }
            Command::OpenFolder(path) => {
                thread::spawn(move || {
                    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
                    let msg = match std::process::Command::new(opener).arg(&path).status() {
                        Ok(s) if s.success() => format!("Opened {}", path.display()),
                        Ok(s) => format!("{opener} exited with {s}"),
                        Err(e) => format!("could not run {opener}: {e}"),
                    };
                    let _ = tx.send(JobResult::Info(msg));
                });
            }
            Command::Preview { key, session } => {
                let home = self.home.clone();
                thread::spawn(move || {
                    let result = all_adapters(&home)
                        .into_iter()
                        .find(|a| a.kind() == session.agent)
                        .ok_or_else(|| "no adapter for this agent".to_string())
                        .and_then(|a| {
                            a.preview(&session, PREVIEW_MSGS)
                                .map_err(|e| format!("{e:#}"))
                        });
                    let _ = tx.send(JobResult::Preview { key, result });
                });
            }
        }
    }

    // ----- derived view ----------------------------------------------------------------

    /// Recomputes the filtered, sorted list and the Analyze tree. Call after any change to
    /// sessions, filter, sort or grouping.
    fn refresh(&mut self) {
        let mut idx: Vec<usize> = match &self.filter {
            Some(p) => group::filter(&self.sessions, p),
            None => (0..self.sessions.len()).collect(),
        };
        group::sort_indices(&self.sessions, &mut idx, self.sort);
        let subset: Vec<Session> = idx.iter().map(|&i| self.sessions[i].clone()).collect();
        self.tree = group::group(&subset, self.group_by, self.sort);
        self.visible = idx;
        let depth = self.walk().0.len();
        self.analyze_path.truncate(depth);
        self.clamp_cursors();
    }

    fn clamp_cursors(&mut self) {
        self.list_cursor = clamp_idx(self.list_cursor, self.rows_for(Screen::List).len());
        self.analyze_cursor = clamp_idx(self.analyze_cursor, self.rows_for(Screen::Analyze).len());
        self.trash_cursor = clamp_idx(self.trash_cursor, self.trash_entries.len());
        self.agents_cursor = clamp_idx(self.agents_cursor, self.agent_infos.len());
    }

    /// The Analyze groups along the drill-down path, and the level currently shown.
    fn walk(&self) -> (Vec<&Group>, &[Group]) {
        let mut nodes = Vec::new();
        let mut level: &[Group] = &self.tree;
        for key in &self.analyze_path {
            match level.iter().find(|g| &g.key == key) {
                Some(g) => {
                    nodes.push(g);
                    level = &g.children;
                }
                None => break,
            }
        }
        (nodes, level)
    }

    /// Breadcrumb labels for Analyze, starting at the root.
    pub fn breadcrumb(&self) -> Vec<String> {
        self.walk().0.iter().map(|g| g.label.clone()).collect()
    }

    /// Rows for the table on the current screen.
    pub fn rows(&self) -> Vec<Row> {
        self.rows_for(self.screen)
    }

    pub fn rows_for(&self, screen: Screen) -> Vec<Row> {
        match screen {
            Screen::List => self.visible.iter().map(|&i| self.session_row(i)).collect(),
            Screen::Analyze => {
                let (nodes, level) = self.walk();
                if let Some(last) = nodes.last() {
                    if last.children.is_empty() {
                        return last
                            .sessions
                            .iter()
                            .map(|&s| self.session_row(self.visible[s]))
                            .collect();
                    }
                }
                let mut rows: Vec<Row> = level.iter().map(|g| self.group_row(g)).collect();
                // "(no project)" always sorts last, whatever the chosen sort.
                rows.sort_by_key(|r| r.label == NO_PROJECT);
                rows
            }
            _ => Vec::new(),
        }
    }

    fn session_row(&self, i: usize) -> Row {
        let s = &self.sessions[i];
        let title = s.title.clone().unwrap_or_else(|| "(untitled)".to_string());
        // Child sessions (spawned by another session) carry a marker; deleting the parent
        // deletes them too.
        let label = if s.parent_id.is_some() {
            format!("↳ {title}")
        } else {
            title
        };
        Row {
            key: s.key(),
            label,
            bytes: s.bytes,
            count: 1,
            last: s.last_active.or(s.created),
            sessions: vec![i],
            is_session: true,
            agent: Some(s.agent),
            project: s.project.clone(),
            msgs: s.message_count,
            live: self.is_live(s),
        }
    }

    fn group_row(&self, g: &Group) -> Row {
        let mut sessions = Vec::new();
        self.leaf_sessions(g, &mut sessions);
        Row {
            key: g.key.clone(),
            label: g.label.clone(),
            bytes: g.bytes,
            count: g.count,
            last: g.last_active,
            sessions,
            is_session: false,
            agent: None,
            project: None,
            msgs: None,
            live: false,
        }
    }

    /// Global session indices under a group, in tree order.
    fn leaf_sessions(&self, g: &Group, out: &mut Vec<usize>) {
        if g.children.is_empty() {
            out.extend(g.sessions.iter().map(|&s| self.visible[s]));
        } else {
            for child in &g.children {
                self.leaf_sessions(child, out);
            }
        }
    }

    /// A session is live when its agent process runs and it was active recently.
    fn is_live(&self, s: &Session) -> bool {
        self.running.contains(&s.agent)
            && s.last_active
                .is_some_and(|t| Utc::now() - t < chrono::Duration::hours(LIVE_WINDOW_HOURS))
    }

    /// Global indices of the selected sessions, in session order.
    pub fn selected_indices(&self) -> Vec<usize> {
        (0..self.sessions.len())
            .filter(|&i| self.selected.contains(&self.sessions[i].key()))
            .collect()
    }

    pub fn toast_text(&self) -> Option<&str> {
        self.toast
            .as_ref()
            .filter(|(_, at)| at.elapsed() < TOAST_TTL)
            .map(|(m, _)| m.as_str())
    }

    fn set_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    fn delete_running(&self) -> bool {
        matches!(self.modal, Some(Modal::Delete(DeleteModal::Running { .. })))
    }

    // ----- navigation ------------------------------------------------------------------

    pub fn cursor(&self) -> usize {
        match self.screen {
            Screen::Menu => self.menu_cursor,
            Screen::List => self.list_cursor,
            Screen::Analyze => self.analyze_cursor,
            Screen::Trash => self.trash_cursor,
            Screen::Agents => self.agents_cursor,
        }
    }

    fn cursor_mut(&mut self) -> &mut usize {
        match self.screen {
            Screen::Menu => &mut self.menu_cursor,
            Screen::List => &mut self.list_cursor,
            Screen::Analyze => &mut self.analyze_cursor,
            Screen::Trash => &mut self.trash_cursor,
            Screen::Agents => &mut self.agents_cursor,
        }
    }

    fn row_count(&self) -> usize {
        match self.screen {
            Screen::Menu => MENU_ITEMS.len(),
            Screen::List | Screen::Analyze => self.rows().len(),
            Screen::Trash => self.trash_entries.len(),
            Screen::Agents => self.agent_infos.len(),
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let n = self.row_count();
        if n == 0 {
            self.set_cursor(0);
            return;
        }
        let next = (self.cursor() as isize + delta).clamp(0, n as isize - 1) as usize;
        self.set_cursor(next);
    }

    fn set_cursor(&mut self, i: usize) {
        *self.cursor_mut() = i;
        self.request_preview();
    }

    fn goto(&mut self, screen: Screen) {
        self.screen = screen;
        self.range_anchor = None;
        if screen == Screen::Trash {
            self.load_trash();
        }
    }

    fn load_trash(&mut self) {
        match Trash::new(self.trash_root.clone()).list() {
            Ok(entries) => self.trash_entries = entries,
            Err(e) => {
                self.trash_entries.clear();
                self.set_toast(format!("Could not read trash: {e:#}"));
            }
        }
        self.trash_cursor = clamp_idx(self.trash_cursor, self.trash_entries.len());
    }

    /// Queues a preview load for the session under the cursor, if the preview pane is open.
    fn request_preview(&mut self) {
        if !self.preview_on {
            return;
        }
        let rows = self.rows();
        let Some(row) = rows.get(self.cursor()) else {
            return;
        };
        if !row.is_session {
            return;
        }
        let i = row.sessions[0];
        let key = self.sessions[i].key();
        if self.previews.contains_key(&key) || self.preview_pending.contains(&key) {
            return;
        }
        self.preview_pending.insert(key.clone());
        self.pending.push(Command::Preview {
            key,
            session: self.sessions[i].clone(),
        });
    }

    /// The session shown in the preview pane: the cursor row when it is a session.
    pub fn preview_session(&self) -> Option<(String, &Session)> {
        let rows = self.rows();
        let row = rows.get(self.cursor())?;
        if !row.is_session {
            return None;
        }
        let s = &self.sessions[row.sessions[0]];
        Some((row.key.clone(), s))
    }

    // ----- selection -------------------------------------------------------------------

    fn toggle_sessions(&mut self, idxs: &[usize]) {
        let keys: Vec<String> = idxs.iter().map(|&i| self.sessions[i].key()).collect();
        if keys.is_empty() {
            return;
        }
        if keys.iter().all(|k| self.selected.contains(k)) {
            for k in &keys {
                self.selected.remove(k);
            }
        } else {
            self.selected.extend(keys);
        }
    }

    fn select_sessions(&mut self, idxs: &[usize]) {
        self.selected
            .extend(idxs.iter().map(|&i| self.sessions[i].key()));
    }

    fn toggle_row(&mut self) {
        let rows = self.rows();
        if let Some(row) = rows.get(self.cursor()) {
            let idxs = row.sessions.clone();
            self.toggle_sessions(&idxs);
        }
    }

    /// `v`: first press marks the range start, second press selects the range.
    fn range_mark(&mut self) {
        let cur = self.cursor();
        match self.range_anchor.take() {
            None => self.range_anchor = Some(cur),
            Some(a) => {
                let rows = self.rows();
                let (lo, hi) = (a.min(cur), a.max(cur));
                let idxs: Vec<usize> = rows
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i >= lo && *i <= hi)
                    .flat_map(|(_, r)| r.sessions.iter().copied())
                    .collect();
                self.select_sessions(&idxs);
            }
        }
    }

    /// `a`: selects everything in view, or clears it when all of it is already selected.
    fn toggle_all_in_view(&mut self) {
        let rows = self.rows();
        let idxs: Vec<usize> = rows.iter().flat_map(|r| r.sessions.iter().copied()).collect();
        self.toggle_sessions(&idxs);
    }

    // ----- analyze drill-down ----------------------------------------------------------

    fn open_row(&mut self) {
        let rows = self.rows();
        let Some(row) = rows.get(self.cursor()).cloned() else {
            return;
        };
        if row.is_session {
            self.preview_on = true;
            self.request_preview();
        } else if self.screen == Screen::Analyze {
            self.analyze_path.push(row.key);
            self.analyze_cursor = 0;
            self.range_anchor = None;
        }
    }

    fn up_level(&mut self) {
        let Some(popped) = self.analyze_path.pop() else {
            return;
        };
        self.range_anchor = None;
        let rows = self.rows();
        self.analyze_cursor = rows.iter().position(|r| r.key == popped).unwrap_or(0);
    }

    fn cycle_group(&mut self) {
        self.group_by = match self.group_by {
            GroupBy::Project => GroupBy::Agent,
            GroupBy::Agent => GroupBy::Folder,
            GroupBy::Folder => GroupBy::Project,
        };
        self.analyze_path.clear();
        self.analyze_cursor = 0;
        self.refresh();
    }

    // ----- delete flow -----------------------------------------------------------------

    /// Sessions a delete acts on: the selection, or the cursor row when nothing is selected.
    fn delete_targets(&self) -> Vec<usize> {
        let sel = self.selected_indices();
        if !sel.is_empty() {
            return sel;
        }
        self.rows()
            .get(self.cursor())
            .map(|r| r.sessions.clone())
            .unwrap_or_default()
    }

    /// `d`: opens the confirm modal in its "checking" state and computes the plan in the
    /// background. Nothing moves until the user confirms with `y`.
    fn start_delete(&mut self) {
        let idxs = self.delete_targets();
        if idxs.is_empty() {
            self.set_toast("Nothing to delete");
            return;
        }
        let picked: Vec<Session> = idxs.iter().map(|&i| self.sessions[i].clone()).collect();
        self.plan_seq += 1;
        let seq = self.plan_seq;
        self.modal = Some(Modal::Delete(DeleteModal::Checking {
            seq,
            count: picked.len(),
        }));
        self.pending.push(Command::Plan { seq, picked });
    }

    // ----- input -----------------------------------------------------------------------

    /// Handles one key press. Returns nothing; effects go to `pending` or the state fields.
    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.delete_running() {
                self.set_toast("Waiting for the move to trash to finish");
            } else {
                self.quit = true;
            }
            return;
        }
        if let Some(modal) = self.modal.take() {
            self.modal = self.modal_key(modal, key);
            return;
        }
        if self.filter_input.is_some() {
            self.filter_key(key);
            return;
        }
        match self.screen {
            Screen::Menu => self.menu_key(key),
            Screen::List | Screen::Analyze => self.browse_key(key),
            Screen::Trash => self.trash_key(key),
            Screen::Agents => self.agents_key(key),
        }
    }

    /// Returns the modal that stays open after the key, or `None` when it closes.
    fn modal_key(&mut self, modal: Modal, key: KeyEvent) -> Option<Modal> {
        match modal {
            Modal::Help => None,
            Modal::Delete(DeleteModal::Checking { seq, count }) => match key.code {
                // The plan is still computing; only cancelling is allowed.
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => None,
                _ => Some(Modal::Delete(DeleteModal::Checking { seq, count })),
            },
            Modal::Delete(DeleteModal::Confirm { plan, .. }) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if plan.sessions.is_empty() {
                        self.set_toast("Nothing can be moved: every selected session is blocked");
                        return None;
                    }
                    let count = plan.sessions.len();
                    self.busy = Some(format!("moving {count} sessions to trash"));
                    self.pending.push(Command::Delete(plan));
                    Some(Modal::Delete(DeleteModal::Running { count }))
                }
                _ => None,
            },
            Modal::Delete(DeleteModal::Running { count }) => {
                Some(Modal::Delete(DeleteModal::Running { count }))
            }
            Modal::Trash(TrashModal::Restore(id)) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.busy = Some(format!("restoring {id}"));
                    self.pending.push(Command::Restore(id));
                    None
                }
                _ => None,
            },
            Modal::Trash(TrashModal::EmptyOne(id)) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.busy = Some(format!("emptying {id}"));
                    self.pending.push(Command::Empty(Some(id)));
                    None
                }
                _ => None,
            },
            Modal::Trash(TrashModal::EmptyAll(mut typed)) => match key.code {
                KeyCode::Esc => None,
                KeyCode::Enter => {
                    if typed == "yes" {
                        self.busy = Some("emptying trash".to_string());
                        self.pending.push(Command::Empty(None));
                        None
                    } else {
                        self.set_toast("Type yes and press Enter to empty the whole trash");
                        Some(Modal::Trash(TrashModal::EmptyAll(typed)))
                    }
                }
                KeyCode::Backspace => {
                    typed.pop();
                    Some(Modal::Trash(TrashModal::EmptyAll(typed)))
                }
                KeyCode::Char(c) => {
                    typed.push(c);
                    Some(Modal::Trash(TrashModal::EmptyAll(typed)))
                }
                _ => Some(Modal::Trash(TrashModal::EmptyAll(typed))),
            },
        }
    }

    fn filter_key(&mut self, key: KeyEvent) {
        let Some(mut buf) = self.filter_input.take() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                self.filter = if buf.is_empty() { None } else { Some(buf) };
                self.list_cursor = 0;
                self.analyze_cursor = 0;
                self.refresh();
            }
            KeyCode::Backspace => {
                buf.pop();
                self.filter_input = Some(buf);
            }
            KeyCode::Char(c) => {
                buf.push(c);
                self.filter_input = Some(buf);
            }
            _ => self.filter_input = Some(buf),
        }
    }

    fn browse_key(&mut self, key: KeyEvent) {
        let analyze = self.screen == Screen::Analyze;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::PageDown => self.move_cursor(PAGE),
            KeyCode::PageUp => self.move_cursor(-PAGE),
            KeyCode::Home => self.set_cursor(0),
            KeyCode::End | KeyCode::Char('G') => {
                let n = self.row_count();
                self.set_cursor(n.saturating_sub(1));
            }
            KeyCode::Char('g') if analyze => self.cycle_group(),
            KeyCode::Char('g') => self.set_cursor(0),
            KeyCode::Tab if analyze => self.cycle_group(),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => self.open_row(),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace if analyze => self.up_level(),
            KeyCode::Char(' ') => self.toggle_row(),
            KeyCode::Char('v') => self.range_mark(),
            KeyCode::Char('a') => self.toggle_all_in_view(),
            KeyCode::Char('d') | KeyCode::Delete => self.start_delete(),
            KeyCode::Char('p') => {
                self.preview_on = !self.preview_on;
                self.request_preview();
            }
            KeyCode::Char('o') => self.open_project(),
            KeyCode::Char('s') => {
                self.sort = next_sort(self.sort);
                self.refresh();
            }
            KeyCode::Char('/') => {
                self.filter_input = Some(self.filter.clone().unwrap_or_default());
            }
            KeyCode::Char('t') => self.goto(Screen::Trash),
            KeyCode::Char('r') => self.request_rescan(),
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => self.escape(),
            _ => {}
        }
    }

    fn open_project(&mut self) {
        let rows = self.rows();
        let project = rows.get(self.cursor()).and_then(|r| {
            r.sessions
                .first()
                .and_then(|&i| self.sessions[i].project.clone())
        });
        match project {
            Some(p) => self.pending.push(Command::OpenFolder(p)),
            None => self.set_toast("No project folder for this row"),
        }
    }

    fn request_rescan(&mut self) {
        if self.scanning {
            self.set_toast("Already scanning");
        } else {
            self.pending.push(Command::Rescan);
        }
    }

    /// Esc clears the filter, then the selection, then goes back to the menu.
    fn escape(&mut self) {
        self.range_anchor = None;
        if self.filter.is_some() {
            self.filter = None;
            self.refresh();
        } else if !self.selected.is_empty() {
            self.selected.clear();
        } else {
            self.goto(Screen::Menu);
        }
    }

    fn menu_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Enter | KeyCode::Right => {
                let item = MENU_ITEMS[self.menu_cursor.min(MENU_ITEMS.len() - 1)];
                self.activate(item);
            }
            KeyCode::Char('a') => self.activate(MenuItem::Analyze),
            KeyCode::Char('l') => self.activate(MenuItem::List),
            KeyCode::Char('t') => self.activate(MenuItem::Trash),
            KeyCode::Char('g') => self.activate(MenuItem::Agents),
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            _ => {}
        }
    }

    fn activate(&mut self, item: MenuItem) {
        match item {
            MenuItem::Analyze => {
                self.analyze_path.clear();
                self.goto(Screen::Analyze);
            }
            MenuItem::List => self.goto(Screen::List),
            MenuItem::Trash => self.goto(Screen::Trash),
            MenuItem::Agents => self.goto(Screen::Agents),
            MenuItem::Quit => self.quit = true,
        }
    }

    fn trash_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Home | KeyCode::Char('g') => self.set_cursor(0),
            KeyCode::End | KeyCode::Char('G') => {
                let n = self.row_count();
                self.set_cursor(n.saturating_sub(1));
            }
            KeyCode::Char('r') => {
                if let Some(id) = self.selected_trash_id() {
                    self.modal = Some(Modal::Trash(TrashModal::Restore(id)));
                }
            }
            KeyCode::Char('x') => {
                if let Some(id) = self.selected_trash_id() {
                    self.modal = Some(Modal::Trash(TrashModal::EmptyOne(id)));
                }
            }
            KeyCode::Char('X') => {
                self.modal = Some(Modal::Trash(TrashModal::EmptyAll(String::new())));
            }
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => self.goto(Screen::Menu),
            _ => {}
        }
    }

    fn selected_trash_id(&self) -> Option<String> {
        self.trash_entries
            .get(self.trash_cursor)
            .map(|e| e.id.clone())
    }

    fn agents_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('r') => self.request_rescan(),
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => self.goto(Screen::Menu),
            _ => {}
        }
    }
}

fn clamp_idx(i: usize, n: usize) -> usize {
    if n == 0 {
        0
    } else {
        i.min(n - 1)
    }
}

fn next_sort(s: SortBy) -> SortBy {
    match s {
        SortBy::Size => SortBy::Recent,
        SortBy::Recent => SortBy::Oldest,
        SortBy::Oldest => SortBy::Messages,
        SortBy::Messages => SortBy::Name,
        SortBy::Name => SortBy::Size,
    }
}

/// Puts the terminal into the TUI state and restores it on drop or panic.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = TerminalGuard;
        execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn restore_terminal() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
}

/// Entry point called by the CLI. Runs the interactive loop until the user quits.
///
/// Setting `SZ_TUI_SNAPSHOT=<path>` skips the terminal: it scans, renders one 120x40 frame of
/// `mode` into a text file at `<path>` and returns. Nothing is deleted or cached.
///
/// `background_purge` runs the automatic trash deletion once the app is up, unless
/// `SZ_NO_AUTO_PURGE` is set.
pub fn run(opts: Options, mode: Mode, background_purge: bool) -> Result<()> {
    if let Ok(path) = std::env::var("SZ_TUI_SNAPSHOT") {
        let text = snapshot(opts, mode, 120, 40)?;
        std::fs::write(&path, text)?;
        return Ok(());
    }

    let cached_sessions = cache::load();
    let has_cache = cached_sessions.is_some();
    let mut app = App::new(
        opts,
        paths::trash_dir(),
        mode,
        cached_sessions.unwrap_or_default(),
        has_cache,
    );
    app.persist = true;
    match Config::load() {
        Ok(config) => {
            app.trash_retention_days = config.trash_retention_days;
            if background_purge && !config::auto_purge_disabled() {
                app.start_auto_purge(config);
            }
        }
        Err(e) => app.notify(format!("Settings not read, trash auto-delete skipped: {e:#}")),
    }
    app.start_scan();

    let guard = TerminalGuard::enter()?;
    let result = event_loop(&mut app);
    // Restore the terminal first, so the summary lands on the normal screen.
    drop(guard);
    if app.stats.has_disk_change() {
        app.stats.trash_now = app.trash_now();
    }
    for line in app.stats.render_lines() {
        println!("{line}");
    }
    result
}

fn event_loop(app: &mut App) -> Result<()> {
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    loop {
        app.tick = app.tick.wrapping_add(1);
        app.poll_background();
        terminal.draw(|f| ui::render(f, app))?;
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                app.on_key(key);
            }
        }
        app.dispatch_pending();
        if app.should_quit() {
            return Ok(());
        }
    }
}

/// Scans `opts.home` synchronously and renders one frame of `mode` as plain text.
pub fn snapshot(opts: Options, mode: Mode, width: u16, height: u16) -> Result<String> {
    let sessions = scan::scan_all(&opts.home, opts.agents.as_deref());
    let infos = scan::agent_infos(&opts.home);
    let mut app = App::new(opts, paths::trash_dir(), mode, sessions, false);
    app.running = infos.iter().filter(|i| i.running).map(|i| i.kind).collect();
    app.agent_infos = infos;
    render_to_string(&app, width, height)
}

/// Renders the app into a `TestBackend` and returns the buffer as text, one line per row.
pub fn render_to_string(app: &App, width: u16, height: u16) -> Result<String> {
    let mut term = Terminal::new(TestBackend::new(width, height))?;
    term.draw(|f| ui::render(f, app))?;
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..height {
        let mut line = String::new();
        for x in 0..width {
            line.push_str(buf[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    Ok(out)
}


#[cfg(test)]
pub(crate) mod fixtures {
    use std::path::Path;
    use super::*;
    use chrono::Duration;

    const MB: u64 = 1 << 20;
    const GB: u64 = 1 << 30;

    /// The one place tests construct a `Session`.
    pub fn sess(
        agent: AgentKind,
        id: &str,
        title: Option<&str>,
        project: Option<&str>,
        bytes: u64,
        days_ago: i64,
    ) -> Session {
        Session {
            agent,
            id: id.to_string(),
            title: title.map(str::to_string),
            project: project.map(PathBuf::from),
            created: None,
            last_active: Some(Utc::now() - Duration::days(days_ago)),
            message_count: Some(3),
            bytes,
            artifacts: Vec::new(),
            parent_id: None,
        }
    }

    /// alpha (codex, 500 MB, /p/app, 1 day), beta (claude, 100 MB, /p/app, 2 days),
    /// gamma (codex, 2 GB, /p/other, 30 days), untitled (kiro-cli, 10 MB, no project, 5 days).
    pub fn sample() -> Vec<Session> {
        vec![
            sess(AgentKind::Codex, "a1", Some("alpha"), Some("/p/app"), 500 * MB, 1),
            sess(AgentKind::Claude, "b1", Some("beta"), Some("/p/app"), 100 * MB, 2),
            sess(AgentKind::Codex, "c1", Some("gamma"), Some("/p/other"), 2 * GB, 30),
            sess(AgentKind::KiroCli, "d1", None, None, 10 * MB, 5),
        ]
    }

    pub fn app(mode: Mode, sessions: Vec<Session>, root: &Path) -> App {
        App::new(
            Options {
                home: root.to_path_buf(),
                force: false,
                agents: None,
            },
            root.join("trash"),
            mode,
            sessions,
            false,
        )
    }

    pub fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    pub fn type_keys(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(ch(c));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use std::path::Path;
    use sz_core::model::Artifact;
    use sz_core::scan::ScanEvent;

    fn labels(app: &App) -> Vec<String> {
        app.rows().into_iter().map(|r| r.label).collect()
    }

    fn wait_until(app: &mut App, done: impl Fn(&App) -> bool) {
        let start = Instant::now();
        loop {
            app.poll_background();
            if done(app) {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn list_is_sorted_by_size_and_navigation_clamps() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(Mode::List, sample(), root.path());
        assert_eq!(labels(&app), vec!["gamma", "alpha", "beta", "(untitled)"]);
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.cursor(), 2);
        for _ in 0..10 {
            app.on_key(key(KeyCode::Down));
        }
        assert_eq!(app.cursor(), 3);
        app.on_key(key(KeyCode::Home));
        assert_eq!(app.cursor(), 0);
        app.on_key(key(KeyCode::End));
        assert_eq!(app.cursor(), 3);
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.cursor(), 2);
    }

    #[test]
    fn space_range_and_select_all_toggle_selection() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(Mode::List, sample(), root.path());
        app.on_key(ch(' '));
        assert_eq!(app.selected_indices().len(), 1);
        app.on_key(ch(' '));
        assert!(app.selected.is_empty());

        app.on_key(key(KeyCode::Home));
        app.on_key(ch('v'));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        app.on_key(ch('v'));
        assert_eq!(app.selected_indices().len(), 3);
        assert!(app.range_anchor.is_none());

        app.on_key(ch('a'));
        assert_eq!(app.selected_indices().len(), 4);
        app.on_key(ch('a'));
        assert!(app.selected.is_empty());

        app.on_key(ch(' '));
        app.on_key(key(KeyCode::Esc));
        assert!(app.selected.is_empty());
    }

    #[test]
    fn filter_applies_on_enter_and_clears_on_escape() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(Mode::List, sample(), root.path());
        app.on_key(ch('/'));
        type_keys(&mut app, "codex");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.visible.len(), 2);
        assert_eq!(labels(&app), vec!["gamma", "alpha"]);

        app.on_key(key(KeyCode::Esc));
        assert!(app.filter.is_none());
        assert_eq!(app.visible.len(), 4);

        app.on_key(ch('/'));
        type_keys(&mut app, "zzz");
        app.on_key(key(KeyCode::Esc));
        assert!(app.filter_input.is_none());
        assert!(app.filter.is_none());
    }

    #[test]
    fn sort_cycles_through_all_orders() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(Mode::List, sample(), root.path());
        app.on_key(ch('s'));
        assert_eq!(app.sort, SortBy::Recent);
        assert_eq!(labels(&app)[0], "alpha");
        for _ in 0..4 {
            app.on_key(ch('s'));
        }
        assert_eq!(app.sort, SortBy::Size);
    }

    #[test]
    fn analyze_drills_down_and_back_up() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(Mode::Analyze, sample(), root.path());
        let top = labels(&app);
        assert_eq!(top.len(), 3);
        assert!(top[0].contains("other"), "largest project first: {top:?}");
        assert_eq!(top[2], "(no project)", "no-project group is always last");

        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.breadcrumb(), vec!["/p/other"]);
        assert_eq!(labels(&app), vec!["codex"]);

        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.breadcrumb().len(), 2);
        let rows = app.rows();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_session);
        assert_eq!(rows[0].label, "gamma");

        app.on_key(key(KeyCode::Left));
        assert_eq!(app.breadcrumb().len(), 1);
        assert_eq!(app.cursor(), 0);
        app.on_key(key(KeyCode::Backspace));
        assert!(app.analyze_path.is_empty());
        assert!(app.breadcrumb().is_empty());
        assert!(app.cursor() < 3);
    }

    #[test]
    fn analyze_space_on_group_selects_every_session_under_it() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(Mode::Analyze, sample(), root.path());
        app.on_key(key(KeyCode::Down));
        assert!(labels(&app)[1].contains("app"));
        app.on_key(ch(' '));
        let keys: HashSet<String> = app.selected.clone();
        assert_eq!(
            keys,
            HashSet::from(["codex:a1".to_string(), "claude:b1".to_string()])
        );
        app.on_key(ch(' '));
        assert!(app.selected.is_empty());
    }

    #[test]
    fn tab_regroups_analyze_by_agent() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(Mode::Analyze, sample(), root.path());
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.group_by, GroupBy::Agent);
        assert!(app.analyze_path.is_empty());
        assert_eq!(labels(&app)[0], "codex");
    }

    #[test]
    fn escape_clears_filter_then_selection_then_goes_to_menu() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(Mode::List, sample(), root.path());
        app.on_key(ch(' '));
        app.on_key(ch('/'));
        type_keys(&mut app, "alpha");
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Esc));
        assert!(app.filter.is_none());
        assert_eq!(app.selected.len(), 1);
        app.on_key(key(KeyCode::Esc));
        assert!(app.selected.is_empty());
        assert_eq!(app.screen, Screen::List);
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Menu);
    }

    #[test]
    fn menu_opens_list_and_quit_item_quits() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(Mode::Menu, sample(), root.path());
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::List);
        app.on_key(key(KeyCode::Esc));
        app.on_key(ch('q'));
        assert!(app.should_quit());
    }

    #[test]
    fn ctrl_c_quits_and_help_closes_on_any_key() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(Mode::List, sample(), root.path());
        app.on_key(ch('?'));
        assert!(matches!(app.modal, Some(Modal::Help)));
        app.on_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit());
    }

    #[test]
    fn preview_toggle_queues_a_load_for_the_cursor_session() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(Mode::List, sample(), root.path());
        app.on_key(ch('p'));
        assert!(app.preview_on);
        assert!(matches!(
            app.pending.first(),
            Some(Command::Preview { key, .. }) if key == "codex:c1"
        ));
        // A second request for the same session is not queued again.
        app.request_preview();
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn scan_events_replace_each_agent_and_drop_unreported_cache_rows() {
        let root = tempfile::tempdir().unwrap();
        let cached = vec![
            sess(AgentKind::Codex, "old", Some("stale"), None, MB1, 1),
            sess(AgentKind::Gemini, "g1", Some("gone"), None, MB1, 1),
        ];
        let mut app = app(Mode::List, cached, root.path());
        app.scanning = true;
        app.apply_scan_event(ScanEvent::AgentStarted(AgentKind::Codex));
        app.apply_scan_event(ScanEvent::AgentStarted(AgentKind::Claude));
        app.apply_scan_event(ScanEvent::AgentDone {
            agent: AgentKind::Codex,
            sessions: vec![
                sess(AgentKind::Codex, "n1", Some("fresh one"), None, MB1, 0),
                sess(AgentKind::Codex, "n2", Some("fresh two"), None, MB1, 0),
            ],
            error: None,
            millis: 1,
        });
        app.apply_scan_event(ScanEvent::AgentDone {
            agent: AgentKind::Claude,
            sessions: Vec::new(),
            error: Some("boom".to_string()),
            millis: 1,
        });
        assert_eq!(app.scan_done, 2);
        assert_eq!(app.scan_total, 2);
        assert!(app.scan_errors.contains_key(&AgentKind::Claude));
        assert_eq!(app.sessions.len(), 3, "cached codex row replaced, gemini still listed");

        app.apply_scan_event(ScanEvent::Finished);
        assert!(!app.scanning);
        assert!(!app.cached);
        let keys: Vec<String> = app.sessions.iter().map(|s| s.key()).collect();
        assert!(keys.contains(&"codex:n1".to_string()));
        assert!(!keys.contains(&"codex:old".to_string()));
        assert!(!keys.contains(&"gemini:g1".to_string()), "unreported agent dropped");
        assert_eq!(app.sessions.len(), 2);
    }


    const MB1: u64 = 1 << 20;

    fn file_session(dir: &Path) -> Session {
        let file = dir.join("codex").join("s1.jsonl");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"{\"x\":1}\n").unwrap();
        let mut s = sess(AgentKind::Codex, "s1", Some("moved me"), Some("/p/app"), 0, 30);
        s.artifacts = vec![Artifact::File(file)];
        s.recompute_bytes();
        s
    }

    #[test]
    fn delete_flow_moves_to_trash_then_restores_from_trash_screen() {
        let tmp = tempfile::tempdir().unwrap();
        let session = file_session(tmp.path());
        let file = session.artifacts[0].primary_path().to_path_buf();
        let mut app = app(Mode::List, vec![session], tmp.path());

        // Escape hatch: cancelling the confirm leaves the file alone.
        app.on_key(ch('d'));
        assert!(matches!(
            app.modal,
            Some(Modal::Delete(DeleteModal::Checking { .. }))
        ));
        app.dispatch_pending();
        wait_until(&mut app, |a| {
            matches!(a.modal, Some(Modal::Delete(DeleteModal::Confirm { .. })))
        });
        app.on_key(ch('n'));
        assert!(app.modal.is_none());
        assert!(file.exists());

        app.on_key(ch('d'));
        app.dispatch_pending();
        wait_until(&mut app, |a| {
            matches!(a.modal, Some(Modal::Delete(DeleteModal::Confirm { .. })))
        });
        app.on_key(ch('y'));
        assert!(matches!(
            app.modal,
            Some(Modal::Delete(DeleteModal::Running { count: 1 }))
        ));
        app.dispatch_pending();
        wait_until(&mut app, |a| a.sessions.is_empty() && a.modal.is_none());
        assert!(!file.exists());
        assert!(app
            .toast_text()
            .unwrap()
            .starts_with("Moved 1 sessions"));
        let trash = Trash::new(tmp.path().join("trash"));
        assert_eq!(trash.list().unwrap().len(), 1);

        app.on_key(ch('t'));
        assert_eq!(app.screen, Screen::Trash);
        assert_eq!(app.trash_entries.len(), 1);
        app.on_key(ch('r'));
        app.on_key(ch('n'));
        assert!(app.modal.is_none());
        app.on_key(ch('r'));
        app.on_key(ch('y'));
        app.dispatch_pending();
        wait_until(&mut app, |a| a.trash_entries.is_empty() && a.busy.is_none());
        assert!(file.exists(), "restore put the file back");
        assert!(trash.list().unwrap().is_empty());
    }

    #[test]
    fn deleting_a_parent_includes_its_children_in_the_plan() {
        let tmp = tempfile::tempdir().unwrap();
        // The parent is larger, so the size sort puts it under the cursor.
        let parent = sess(AgentKind::Codex, "p1", Some("parent"), Some("/p/app"), 2 * MB1, 30);
        let mut child = sess(AgentKind::Codex, "c1", Some("child"), Some("/p/app"), MB1, 30);
        child.parent_id = Some("p1".to_string());
        let mut app = app(Mode::List, vec![parent, child], tmp.path());
        assert!(app.rows().iter().any(|r| r.label == "↳ child"));

        app.on_key(ch('d'));
        app.dispatch_pending();
        wait_until(&mut app, |a| {
            matches!(
                a.modal,
                Some(Modal::Delete(DeleteModal::Confirm { children: 1, .. }))
            )
        });
        match &app.modal {
            Some(Modal::Delete(DeleteModal::Confirm { plan, .. })) => {
                assert_eq!(plan.sessions.len(), 2, "child is planned with its parent");
            }
            _ => panic!("expected confirm modal"),
        }
    }

    #[test]
    fn stale_plan_reply_is_ignored_after_cancel() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(Mode::List, sample(), root.path());
        app.on_key(ch('d'));
        app.on_key(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        app.apply_job(JobResult::Planned {
            seq: 1,
            plan: DeletePlan {
                sessions: Vec::new(),
                bytes: 0,
                blocked: Vec::new(),
            },
            children: 0,
        });
        assert!(app.modal.is_none(), "reply for a cancelled request must not reopen the modal");
    }

    #[test]
    fn delete_with_nothing_under_cursor_only_toasts() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(Mode::List, Vec::new(), root.path());
        app.on_key(ch('d'));
        assert!(app.modal.is_none());
        assert_eq!(app.toast_text(), Some("Nothing to delete"));
    }

    #[test]
    fn empty_all_trash_needs_typed_yes() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(Mode::Trash, Vec::new(), root.path());
        app.on_key(ch('X'));
        type_keys(&mut app, "ye");
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(
            app.modal,
            Some(Modal::Trash(TrashModal::EmptyAll(ref t))) if t == "ye"
        ));
        assert!(app.pending.is_empty());
        app.on_key(ch('s'));
        app.on_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert!(matches!(app.pending.first(), Some(Command::Empty(None))));
    }

    #[test]
    fn quit_is_refused_while_a_delete_is_running() {
        let root = tempfile::tempdir().unwrap();
        let mut app = app(Mode::List, sample(), root.path());
        app.modal = Some(Modal::Delete(DeleteModal::Running { count: 2 }));
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.should_quit());
        app.on_key(ch('q'));
        assert!(matches!(
            app.modal,
            Some(Modal::Delete(DeleteModal::Running { count: 2 }))
        ));
    }

    /// Real home, read-only. Run with `cargo test -p sessionzero snapshot_real_home -- --ignored --nocapture`.
    /// Writes the two 120x40 frames to `$KIROCREW_SCRATCH/sz-list.txt` and `sz-analyze.txt`.
    #[test]
    #[ignore]
    fn snapshot_real_home() {
        let scratch = std::env::var("KIROCREW_SCRATCH").unwrap_or_else(|_| "/tmp".to_string());
        let home = dirs::home_dir().expect("home");
        for (mode, name) in [(Mode::List, "sz-list.txt"), (Mode::Analyze, "sz-analyze.txt")] {
            let opts = Options {
                home: home.clone(),
                force: false,
                agents: None,
            };
            let text = snapshot(opts, mode, 120, 40).expect("snapshot");
            std::fs::write(PathBuf::from(&scratch).join(name), text).expect("write");
        }
    }
}

#[cfg(test)]
mod auto_purge_tests {
    use super::*;

    fn blank_app(root: &std::path::Path) -> App {
        App::new(
            Options {
                home: root.to_path_buf(),
                force: false,
                agents: None,
            },
            root.join("trash"),
            Mode::Trash,
            Vec::new(),
            false,
        )
    }

    #[test]
    fn purge_result_becomes_a_toast_and_is_silent_when_nothing_ran() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut app = blank_app(root.path());
        assert!(app.toast_text().is_none());
        app.apply_job(JobResult::AutoPurged {
            report: PurgeReport {
                entries: 2,
                bytes: 4096,
            },
            days: 30,
        });
        let toast = app.toast_text().expect("toast after purge");
        assert!(toast.starts_with("Auto-deleted 2 trash entries ("), "{toast}");
        assert!(toast.ends_with("older than 30 days"), "{toast}");
    }

    #[test]
    fn retention_defaults_to_thirty_days_until_settings_are_read() {
        let root = tempfile::tempdir().expect("tempdir");
        let app = blank_app(root.path());
        assert_eq!(app.trash_retention_days, 30);
    }
}

#[cfg(test)]
mod session_summary_tests {
    use super::*;

    const MB: u64 = 1 << 20;
    const GB: u64 = 1 << 30;

    fn trash_now(entries: usize, bytes: u64, days_until_next: Option<i64>) -> TrashNow {
        TrashNow {
            entries,
            bytes,
            retention_days: 30,
            days_until_next,
        }
    }

    #[test]
    fn no_disk_change_renders_nothing() {
        let stats = SessionStats {
            trash_now: Some(trash_now(2, GB, Some(5))),
            ..SessionStats::default()
        };
        assert!(stats.render_lines().is_empty());
    }

    #[test]
    fn errors_without_a_disk_change_render_nothing() {
        let mut stats = SessionStats::default();
        stats.record_error("every session failed: codex:a1: permission denied");
        assert!(stats.render_lines().is_empty());
    }

    #[test]
    fn moved_only_says_not_freed_yet_and_explains_the_trash() {
        let stats = SessionStats {
            moved_sessions: 3,
            moved_bytes: 1_288_490_189,
            moved_entries: 2,
            trash_now: Some(trash_now(2, 1_288_490_189, Some(23))),
            ..SessionStats::default()
        };
        assert_eq!(
            stats.render_lines(),
            vec![
                "Moved to trash this session: 3 sessions (1.2 GB) in 2 trash entries (not freed yet)",
                "Trash now: 2 entries, 1.2 GB; next auto-delete in 23 days",
                "Space is freed when you empty the trash.",
            ]
        );
    }

    #[test]
    fn moved_and_restored_lists_both_and_singular_forms() {
        let stats = SessionStats {
            moved_sessions: 1,
            moved_bytes: 500 * MB,
            moved_entries: 1,
            restored_entries: 1,
            restored_bytes: 500 * MB,
            trash_now: Some(trash_now(0, 0, None)),
            ..SessionStats::default()
        };
        assert_eq!(
            stats.render_lines(),
            vec![
                "Moved to trash this session: 1 session (500.0 MB) in 1 trash entry (not freed yet)",
                "Restored from trash this session: 1 entry (500.0 MB)",
                "Trash now: 0 entries, 0 B",
                "Space is freed when you empty the trash.",
            ]
        );
    }

    #[test]
    fn emptied_reports_bytes_freed_on_disk_without_the_trash_note() {
        let stats = SessionStats {
            emptied_entries: 4,
            emptied_bytes: 2 * GB,
            trash_now: Some(trash_now(0, 0, None)),
            ..SessionStats::default()
        };
        assert_eq!(
            stats.render_lines(),
            vec![
                "Permanently deleted from trash this session (x / X): 4 entries, 2.0 GB freed on disk.",
                "Trash now: 0 entries, 0 B",
            ]
        );
    }

    #[test]
    fn auto_purged_reports_bytes_freed_on_disk() {
        let stats = SessionStats {
            purged_entries: 1,
            purged_bytes: 300 * MB,
            trash_now: Some(trash_now(0, 0, None)),
            ..SessionStats::default()
        };
        let lines = stats.render_lines();
        assert_eq!(
            lines[0],
            "Auto-deleted by the startup trash purge this session: 1 entry, 300.0 MB freed on disk."
        );
    }

    #[test]
    fn errors_show_count_and_first_message_truncated_to_120_chars() {
        let long = format!("codex:a1: {}", "x".repeat(200));
        let mut stats = SessionStats {
            moved_sessions: 1,
            moved_bytes: 1024,
            moved_entries: 1,
            trash_now: Some(trash_now(1, 1024, Some(30))),
            ..SessionStats::default()
        };
        stats.record_error(long.clone());
        stats.record_error("second failure");

        let lines = stats.render_lines();
        let err = lines.last().expect("errors line");
        let prefix = "Errors this session: 2 (first: ";
        assert!(err.starts_with(prefix), "{err}");
        let shown = err.strip_prefix(prefix).unwrap().strip_suffix(')').unwrap();
        assert_eq!(shown.chars().count(), 120);
        assert!(shown.ends_with("..."));
        assert!(shown.starts_with("codex:a1: xxx"));
    }

    #[test]
    fn errors_line_is_omitted_when_there_are_none() {
        let stats = SessionStats {
            moved_sessions: 1,
            moved_bytes: 1024,
            moved_entries: 1,
            trash_now: Some(trash_now(1, 1024, Some(30))),
            ..SessionStats::default()
        };
        assert!(stats.render_lines().iter().all(|l| !l.starts_with("Errors")));
    }

    #[test]
    fn forever_retention_says_kept_forever() {
        let stats = SessionStats {
            moved_sessions: 1,
            moved_bytes: 1024,
            moved_entries: 1,
            trash_now: Some(TrashNow {
                entries: 3,
                bytes: 1024,
                retention_days: 0,
                days_until_next: None,
            }),
            ..SessionStats::default()
        };
        assert!(stats
            .render_lines()
            .contains(&"Trash now: 3 entries, 1.0 KB; kept forever".to_string()));
    }

    #[test]
    fn not_freed_yet_note_appears_only_when_moved_bytes_are_positive() {
        let with_bytes = SessionStats {
            moved_sessions: 1,
            moved_bytes: MB,
            moved_entries: 1,
            trash_now: Some(trash_now(1, MB, Some(30))),
            ..SessionStats::default()
        };
        let joined = with_bytes.render_lines().join("\n");
        assert!(joined.contains("(not freed yet)"), "{joined}");
        assert!(joined.contains("Space is freed when you empty the trash."));

        let zero_bytes = SessionStats {
            moved_sessions: 1,
            moved_bytes: 0,
            moved_entries: 1,
            trash_now: Some(trash_now(1, 0, Some(30))),
            ..SessionStats::default()
        };
        let joined = zero_bytes.render_lines().join("\n");
        assert!(!joined.contains("not freed yet"), "{joined}");
    }

    #[test]
    fn multiline_error_is_flattened_to_one_line() {
        let mut stats = SessionStats {
            emptied_entries: 1,
            emptied_bytes: 10,
            ..SessionStats::default()
        };
        stats.record_error("first line\nsecond line");
        let last = stats.render_lines().pop().unwrap();
        assert_eq!(last, "Errors this session: 1 (first: first line second line)");
    }

    #[test]
    fn app_updates_stats_from_job_results() {
        let root = tempfile::tempdir().unwrap();
        let mut app = App::new(
            Options {
                home: root.path().to_path_buf(),
                force: false,
                agents: None,
            },
            root.path().join("trash"),
            Mode::Trash,
            Vec::new(),
            false,
        );
        app.apply_job(JobResult::Restored {
            id: "missing".into(),
            bytes: 1024,
            result: Ok(()),
        });
        app.apply_job(JobResult::Emptied {
            count: 2,
            result: Ok(4096),
        });
        app.apply_job(JobResult::Emptied {
            count: 0,
            result: Err("lock held".into()),
        });
        assert_eq!(app.stats.restored_entries, 1);
        assert_eq!(app.stats.restored_bytes, 1024);
        assert_eq!(app.stats.emptied_entries, 2);
        assert_eq!(app.stats.emptied_bytes, 4096);
        assert_eq!(app.stats.error_count, 1);
        assert_eq!(app.stats.first_error.as_deref(), Some("lock held"));
    }

    #[test]
    fn trash_now_reads_an_empty_trash_root() {
        let root = tempfile::tempdir().unwrap();
        let app = App::new(
            Options {
                home: root.path().to_path_buf(),
                force: false,
                agents: None,
            },
            root.path().join("trash"),
            Mode::Trash,
            Vec::new(),
            false,
        );
        assert_eq!(
            app.trash_now(),
            Some(TrashNow {
                entries: 0,
                bytes: 0,
                retention_days: config::DEFAULT_TRASH_RETENTION_DAYS,
                days_until_next: None,
            })
        );
    }
}
