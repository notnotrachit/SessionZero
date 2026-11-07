//! Parallel scan of all adapters.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use rayon::prelude::*;

use crate::adapter::Adapter;
use crate::adapters::all_adapters;
use crate::model::{AgentKind, Session};
use crate::util::{normalize_project, running_processes};

/// Progress of a streaming scan.
#[derive(Debug)]
pub enum ScanEvent {
    AgentStarted(AgentKind),
    AgentDone {
        agent: AgentKind,
        sessions: Vec<Session>,
        error: Option<String>,
        millis: u128,
    },
    Finished,
}

/// Detected adapters for `home`, optionally limited to `only`.
fn select_adapters(home: &Path, only: Option<&[AgentKind]>) -> Vec<Box<dyn Adapter>> {
    all_adapters(home)
        .into_iter()
        .filter(|a| only.is_none_or(|o| o.contains(&a.kind())))
        .filter(|a| a.detect())
        .collect()
}

/// Runs one adapter's scan. Errors and panics become a message; they never abort the scan.
/// Placeholder projects (`/` and the home directory) are cleared on the way out.
fn run_scan(adapter: &dyn Adapter, home: &Path) -> (Vec<Session>, Option<String>, u128) {
    let started = Instant::now();
    let outcome = catch_unwind(AssertUnwindSafe(|| adapter.scan()));
    let millis = started.elapsed().as_millis();
    match outcome {
        Ok(Ok(mut sessions)) => {
            let home_dir = Some(home);
            for s in &mut sessions {
                normalize_project(&mut s.project, home_dir);
            }
            (sessions, None, millis)
        }
        Ok(Err(err)) => {
            let msg = format!("{err:#}");
            eprintln!("sz: {} scan failed: {msg}", adapter.kind().label());
            (Vec::new(), Some(msg), millis)
        }
        Err(_) => {
            let msg = "adapter panicked".to_string();
            eprintln!("sz: {} scan failed: {msg}", adapter.kind().label());
            (Vec::new(), Some(msg), millis)
        }
    }
}

/// Blocking scan of every detected adapter (in parallel). Failed adapters are logged and skipped.
pub fn scan_all(home: &Path, only: Option<&[AgentKind]>) -> Vec<Session> {
    let adapters = select_adapters(home, only);
    adapters
        .par_iter()
        .map(|a| run_scan(a.as_ref(), home).0)
        .collect::<Vec<Vec<Session>>>()
        .into_iter()
        .flatten()
        .collect()
}

/// Streams scan progress. One thread per adapter; an event is sent as each adapter starts
/// and finishes. `Finished` is the last event.
pub fn scan_stream(home: PathBuf, only: Option<Vec<AgentKind>>) -> Receiver<ScanEvent> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let adapters = select_adapters(&home, only.as_deref());
        let handles: Vec<JoinHandle<()>> = adapters
            .into_iter()
            .map(|adapter| {
                let tx = tx.clone();
                let home = home.clone();
                thread::spawn(move || {
                    let agent = adapter.kind();
                    let _ = tx.send(ScanEvent::AgentStarted(agent));
                    let (sessions, error, millis) = run_scan(adapter.as_ref(), &home);
                    let _ = tx.send(ScanEvent::AgentDone {
                        agent,
                        sessions,
                        error,
                        millis,
                    });
                })
            })
            .collect();
        for handle in handles {
            let _ = handle.join();
        }
        let _ = tx.send(ScanEvent::Finished);
    });
    rx
}

/// Install state of one agent, for the agent list.
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub kind: AgentKind,
    pub detected: bool,
    pub running: bool,
    pub roots: Vec<(PathBuf, bool)>,
}

/// Detection, running state and data roots for every agent. `running` is only checked for
/// detected agents.
pub fn agent_infos(home: &Path) -> Vec<AgentInfo> {
    all_adapters(home)
        .iter()
        .map(|a| {
            let detected = a.detect();
            let running = detected && running_processes(a.process_names());
            let roots = a
                .roots()
                .into_iter()
                .map(|p| {
                    let exists = p.exists();
                    (p, exists)
                })
                .collect();
            AgentInfo {
                kind: a.kind(),
                detected,
                running,
                roots,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_home_detects_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(scan_all(tmp.path(), None).is_empty());
    }

    #[test]
    fn agent_infos_lists_every_agent_undetected_in_empty_home() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let infos = agent_infos(tmp.path());
        assert_eq!(infos.len(), AgentKind::all().len());
        assert!(infos.iter().all(|i| !i.detected && !i.running));
    }

    /// Read-only timing of a full scan and of the trash plan that follows it, on the real home.
    /// Run with `--ignored --nocapture --release` to see per-agent times.
    #[test]
    #[ignore]
    fn live_scan_all_and_plan_timing() {
        let home = dirs::home_dir().expect("home dir");
        for adapter in all_adapters(&home) {
            if !adapter.detect() {
                continue;
            }
            let started = Instant::now();
            let n = adapter.scan().map(|s| s.len()).unwrap_or(0);
            println!("timing {:<10} {:>6} sessions {:?}", adapter.kind().label(), n, started.elapsed());
        }
        let started = Instant::now();
        let sessions = scan_all(&home, None);
        println!("timing scan_all total (parallel) {} sessions {:?}", sessions.len(), started.elapsed());
        let started = Instant::now();
        let plan = crate::trash::plan(&home, sessions, false);
        println!("timing plan {} sessions, {} blocked {:?}", plan.sessions.len(), plan.blocked.len(), started.elapsed());
    }

    #[test]
    fn scan_clears_root_project_of_kiro_cli_subagents() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cli = tmp.path().join(".kiro/sessions/cli");
        std::fs::create_dir_all(&cli).expect("mkdir");
        let id = "44444444-aaaa-4bbb-8ccc-000000000004";
        std::fs::write(
            cli.join(format!("{id}.json")),
            r#"{"session_id":"x","cwd":"/","parent_session_id":"p"}"#,
        )
        .expect("write");
        let sessions = scan_all(tmp.path(), Some(&[AgentKind::KiroCli]));
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].project.is_none(), "cwd / must not be a project");
        assert_eq!(sessions[0].parent_id.as_deref(), Some("p"));
    }

    #[test]
    fn scan_stream_ends_with_finished() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let rx = scan_stream(tmp.path().to_path_buf(), Some(vec![AgentKind::Codex]));
        let mut last = None;
        for event in rx.iter() {
            last = Some(event);
        }
        assert!(matches!(last, Some(ScanEvent::Finished)));
    }
}
