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

/// True when a running process matches one of `names` (case-insensitive, whole path components
/// only). See `process_matches` for the rules.
///
/// Node-based CLIs match through their script path: `node .../@github/copilot/index.js` matches
/// `@github/copilot`. A name like `kiro_crew` still matches `python3.12 -m kiro_crew`, but not
/// `kiro_crew_helper`. Only process lists, command lines and executable paths are read. CPU and
/// memory are not refreshed.
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

/// True when one of the `wanted` names matches a process. A name is split on `/` into path
/// components, and it matches when those components appear next to each other, in whole, in
/// one of the process's component lists: its name, its executable path, or any whitespace-
/// separated command-line word split on `/`. So `pi` matches `node /x/bin/pi`, and
/// `@github/copilot` matches `node /x/node_modules/@github/copilot/index.js`, but neither
/// matches a longer component such as `pinball` or `@github/copilot-helper`.
fn process_matches(name: &str, exe: Option<&Path>, cmd: &[String], wanted: &[String]) -> bool {
    let mut lists: Vec<Vec<String>> = vec![vec![name.to_lowercase()]];
    if let Some(exe) = exe {
        lists.push(
            exe.components()
                .filter_map(|c| c.as_os_str().to_str())
                .map(str::to_lowercase)
                .collect(),
        );
    }
    for arg in cmd {
        for word in arg.split_whitespace() {
            lists.push(split_components(word));
        }
    }
    wanted.iter().any(|w| {
        let parts = split_components(w);
        !parts.is_empty()
            && lists
                .iter()
                .any(|list| list.windows(parts.len()).any(|run| run == parts.as_slice()))
    })
}

/// Lowercased, non-empty `/`-separated components of a token.
fn split_components(token: &str) -> Vec<String> {
    token
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_lowercase)
        .collect()
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
    fn node_cli_matches_through_a_script_path_component() {
        // `node` runs the CLI, so the agent name is only in the script path.
        let w = wanted(&["copilot", "@github/copilot"]);
        let argv = cmd(&[
            "node",
            "/usr/local/lib/node_modules/@github/copilot/index.js",
        ]);
        assert!(process_matches("node", None, &argv, &w));
        assert!(process_matches(
            "node",
            Some(Path::new("/opt/homebrew/bin/node")),
            &cmd(&["node", "/Users/fake/.npm/_npx/x/node_modules/@github/copilot/npm-loader.js"]),
            &w
        ));
    }

    #[test]
    fn multi_component_name_needs_consecutive_components() {
        let w = wanted(&["@github/copilot"]);
        // Both parts present, but not in order.
        assert!(!process_matches(
            "node",
            None,
            &cmd(&["node", "/x/copilot/@github/index.js"]),
            &w
        ));
        // Only the scope present.
        assert!(!process_matches(
            "node",
            None,
            &cmd(&["node", "/x/node_modules/@github/codex/index.js"]),
            &w
        ));
        // A longer package name is not the same component.
        assert!(!process_matches(
            "node",
            None,
            &cmd(&["node", "/x/node_modules/@github/copilot-helper/index.js"]),
            &w
        ));
        // Executable path works the same way.
        let exe = Path::new("/opt/lib/node_modules/@github/copilot/bin");
        assert!(process_matches("node", Some(exe), &[], &w));
    }

    #[test]
    fn pi_matches_its_bin_link_and_its_package_directory() {
        let w = wanted(&["pi", "pi-coding-agent"]);
        assert!(process_matches(
            "node",
            None,
            &cmd(&["node", "/Users/fake/.nvm/versions/node/v22/bin/pi"]),
            &w
        ));
        assert!(process_matches(
            "node",
            None,
            &cmd(&[
                "node",
                "/Users/fake/.npm/lib/node_modules/@mariozechner/pi-coding-agent/dist/cli.js"
            ]),
            &w
        ));
    }

    #[test]
    fn pi_does_not_match_a_longer_or_unrelated_component() {
        let w = wanted(&["pi", "pi-coding-agent"]);
        assert!(!process_matches(
            "node",
            None,
            &cmd(&["node", "/Users/fake/pinball/index.js"]),
            &w
        ));
        assert!(!process_matches(
            "node",
            None,
            &cmd(&["node", "/Users/fake/pi-coding-agent-docs/server.js"]),
            &w
        ));
        assert!(!process_matches("zsh", None, &cmd(&["zsh", "-l"]), &w));
    }

    #[test]
    fn empty_or_slash_only_names_never_match() {
        let w = wanted(&["/", ""]);
        assert!(!process_matches("node", Some(Path::new("/usr/bin/node")), &cmd(&["node", "/"]), &w));
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
