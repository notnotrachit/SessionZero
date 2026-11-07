use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use walkdir::WalkDir;

/// Size of a file, or the recursive sum of a directory. Symlinks are not followed.
pub fn path_size(p: &Path) -> u64 {
    match std::fs::symlink_metadata(p) {
        Ok(meta) if meta.is_file() => meta.len(),
        Ok(meta) if meta.is_dir() => WalkDir::new(p)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.metadata().ok())
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum(),
        _ => 0,
    }
}

/// Last modification time of a path.
pub fn mtime(p: &Path) -> Option<DateTime<Utc>> {
    let modified = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(modified))
}

/// True when a running process matches one of `names` (case-insensitive, whole tokens only).
///
/// A process matches through its name, through each component of its executable path, and through
/// each whitespace-separated command-line token, taken with its last `/` segment. So
/// `python3.12 -m kiro_crew` matches `kiro_crew`, but `kiro_crew_helper` does not. Only process
/// lists, command lines and executable paths are read. CPU and memory are not refreshed.
pub fn running_processes(names: &[&str]) -> bool {
    let wanted: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::Always),
    );
    sys.processes().values().any(|proc_| {
        let cmd: Vec<String> = proc_
            .cmd()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        process_matches(
            &proc_.name().to_string_lossy(),
            proc_.exe(),
            &cmd,
            &wanted,
        )
    })
}

/// True when one of the lowercase `wanted` names equals a whole token of the process: its name,
/// a component of its executable path, or the last `/` segment of a command-line word.
fn process_matches(name: &str, exe: Option<&Path>, cmd: &[String], wanted: &[String]) -> bool {
    let mut tokens: Vec<String> = vec![name.to_lowercase()];
    if let Some(exe) = exe {
        tokens.extend(
            exe.components()
                .filter_map(|c| c.as_os_str().to_str())
                .map(str::to_lowercase),
        );
    }
    for arg in cmd {
        for word in arg.split_whitespace() {
            let base = word.rsplit('/').next().unwrap_or(word);
            tokens.push(base.to_lowercase());
        }
    }
    wanted.iter().any(|w| tokens.iter().any(|t| t == w))
}

/// True when a session's project path names no project: the filesystem root, or the home
/// directory itself. Agents record these for sessions started outside any repository, such as
/// Kiro CLI subagents with cwd `/`.
pub fn is_placeholder_project(p: &Path, home: Option<&Path>) -> bool {
    p.parent().is_none() || home.is_some_and(|h| h == p)
}

/// Clears `session.project` when it is a placeholder path. See `is_placeholder_project`.
pub fn normalize_project(project: &mut Option<PathBuf>, home: Option<&Path>) {
    if project
        .as_deref()
        .is_some_and(|p| is_placeholder_project(p, home))
    {
        *project = None;
    }
}

/// Truncate to at most `n` characters, appending an ellipsis when cut.
pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    if n == 0 {
        return String::new();
    }
    let kept: String = s.chars().take(n - 1).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wanted(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_lowercase()).collect()
    }

    fn cmd(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn kiro_crew_runs_as_python_module() {
        let w = wanted(&["kirocrew", "kiro_crew", "kiro-crew"]);
        let argv = cmd(&["python3.12", "-m", "kiro_crew"]);
        assert!(process_matches("python3.12", None, &argv, &w));
    }

    #[test]
    fn match_is_case_insensitive_and_whole_token() {
        let w = wanted(&["kiro_crew"]);
        assert!(process_matches("x", None, &cmd(&["Python", "-m", "KIRO_CREW"]), &w));
        assert!(!process_matches("x", None, &cmd(&["python", "-m", "kiro_crew_helper"]), &w));
        assert!(!process_matches("x", None, &cmd(&["python", "-m", "my_kiro_crew"]), &w));
    }

    #[test]
    fn match_uses_last_path_segment_of_a_command_word() {
        let w = wanted(&["kiro-cli"]);
        assert!(process_matches("node", None, &cmd(&["/opt/bin/kiro-cli", "chat"]), &w));
    }

    #[test]
    fn match_uses_executable_path_components() {
        let w = wanted(&["grok"]);
        // A directory named `grok` on the executable path is a whole-token hit.
        let exe = Path::new("/Users/me/.local/share/grok/bin/grok-2");
        assert!(process_matches("grok-2", Some(exe), &[], &w));
        // A longer name that only starts with the token is not.
        let exe = Path::new("/usr/local/bin/grokking");
        assert!(!process_matches("grokking", Some(exe), &[], &w));
        let exe = Path::new("/usr/local/bin/grok");
        assert!(process_matches("other", Some(exe), &[], &w));
    }

    #[test]
    fn unrelated_process_does_not_match() {
        let w = wanted(&["codex"]);
        assert!(!process_matches("bash", Some(Path::new("/bin/bash")), &cmd(&["bash", "-l"]), &w));
    }

    #[test]
    fn running_processes_finds_the_current_process_by_name() {
        let me = std::env::current_exe().expect("exe");
        let base = me.file_name().unwrap().to_string_lossy().to_lowercase();
        assert!(running_processes(&[base.as_str()]));
        assert!(!running_processes(&["no-such-agent-process-xyz"]));
    }
}
