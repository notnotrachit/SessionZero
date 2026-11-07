//! End-to-end tests for the non-interactive `sz` commands.
//!
//! Each test copies the fixture homes into a fresh temp dir and runs the built binary with
//! `--home` pointing at the copy and `SZ_DATA_DIR` pointing at a second temp dir, so the real
//! home and the real trash are never touched.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

const CLAUDE_OLD: &str =
    ".claude/projects/-Users-test-proj/5f1c0a02-2b3c-4d5e-9f01-000000000002.jsonl";
const CLAUDE_OLD_KEY: &str = "claude:5f1c0a02-2b3c-4d5e-9f01-000000000002";

struct Sandbox {
    _dir: TempDir,
    home: PathBuf,
    data: PathBuf,
    fixtures: PathBuf,
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

/// Copies a directory tree, skipping the fixture bookkeeping files.
fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create dir");
    for entry in fs::read_dir(src).expect("read fixture dir") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name();
        let text = name.to_string_lossy();
        if text == "EXPECTED.json" || text == ".gitkeep" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&from, &to);
        } else {
            fs::copy(&from, &to).expect("copy fixture file");
        }
    }
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let data = dir.path().join("data");
        fs::create_dir_all(&home).expect("home");
        fs::create_dir_all(&data).expect("data");
        // Each agent folder holds the files that belong at the home root.
        for agent in fs::read_dir(fixtures_root()).expect("fixtures") {
            let agent = agent.expect("entry");
            if agent.file_type().expect("type").is_dir() {
                copy_tree(&agent.path(), &home);
            }
        }
        Sandbox {
            fixtures: fixtures_root(),
            _dir: dir,
            home,
            data,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sz"))
            .arg("--home")
            .arg(&self.home)
            .args(args)
            .env("SZ_DATA_DIR", &self.data)
            .output()
            .expect("run sz")
    }

    /// Bytes of a file in the pristine fixture, addressed by its path under the fixture home.
    fn fixture_bytes(&self, rel: &str) -> Vec<u8> {
        fs::read(self.fixtures.join("claude").join(rel)).expect("fixture bytes")
    }
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("exit code")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}\nstderr: {}",
            stdout(out),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[test]
fn agents_json_lists_every_agent_with_sessions() {
    let sb = Sandbox::new();
    let out = sb.run(&["agents", "--json"]);
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    let rows = v.as_array().expect("array");
    assert_eq!(rows.len(), 8);
    let claude = rows
        .iter()
        .find(|r| r["agent"] == "claude")
        .expect("claude row");
    assert!(claude["sessions"].as_u64().unwrap_or(0) >= 3);
    assert_eq!(claude["detected"], true);
}

#[test]
fn list_json_returns_sessions_by_size() {
    let sb = Sandbox::new();
    let out = sb.run(&["list", "--json"]);
    assert_eq!(code(&out), 0);
    let v = json(&out);
    let rows = v.as_array().expect("array");
    assert!(!rows.is_empty());
    let sizes: Vec<u64> = rows
        .iter()
        .map(|r| r["bytes"].as_u64().unwrap_or(0))
        .collect();
    assert!(
        sizes.windows(2).all(|w| w[0] >= w[1]),
        "not sorted by size: {sizes:?}"
    );
}

#[test]
fn list_json_limit_and_filter_apply() {
    let sb = Sandbox::new();
    let out = sb.run(&["list", "--json", "--limit", "1"]);
    assert_eq!(json(&out).as_array().expect("array").len(), 1);

    let out = sb.run(&["list", "--json", "--filter", "^claude$"]);
    let rows = json(&out).as_array().expect("array").clone();
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|r| r["agent"] == "claude"));
}

#[test]
fn clean_without_any_rule_is_refused() {
    let sb = Sandbox::new();
    let out = sb.run(&["clean"]);
    assert_eq!(code(&out), 2);
    assert!(sb.home.join(CLAUDE_OLD).exists());
}

#[test]
fn clean_dry_run_plans_but_moves_nothing() {
    let sb = Sandbox::new();
    let out = sb.run(&["clean", "--older", "1d", "--dry-run"]);
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout(&out).contains("Plan:"), "stdout: {}", stdout(&out));
    assert!(sb.home.join(CLAUDE_OLD).exists());
    assert_eq!(
        json(&sb.run(&["trash", "list", "--json"])),
        Value::Array(vec![])
    );
}

#[test]
fn clean_without_yes_is_refused_when_not_a_terminal() {
    let sb = Sandbox::new();
    let out = sb.run(&["clean", "--older", "1d"]);
    assert_eq!(code(&out), 2, "stdout: {}", stdout(&out));
    assert!(sb.home.join(CLAUDE_OLD).exists());
}

#[test]
fn clean_trash_restore_round_trip_and_empty() {
    let sb = Sandbox::new();
    let original_file = sb.fixture_bytes(CLAUDE_OLD);
    let history_path = sb.home.join(".claude/history.jsonl");
    let original_history = fs::read(&history_path).expect("history");

    // Move old sessions to trash.
    let out = sb.run(&["clean", "--older", "1d", "--yes"]);
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !sb.home.join(CLAUDE_OLD).exists(),
        "session file should be gone"
    );

    // The entry lists the session and carries its id.
    let listed = json(&sb.run(&["trash", "list", "--json"]));
    let entries = listed.as_array().expect("array");
    assert_eq!(entries.len(), 1, "one clean run makes one entry");
    let entry = &entries[0];
    let id = entry["id"].as_str().expect("entry id").to_string();
    let keys: Vec<&str> = entry["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(
        keys.contains(&"5f1c0a02-2b3c-4d5e-9f01-000000000002"),
        "keys: {keys:?}"
    );

    // Restore brings the session file back byte for byte, and the shared history file too.
    let out = sb.run(&["trash", "restore", &id]);
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read(sb.home.join(CLAUDE_OLD)).expect("restored"),
        original_file
    );
    assert_eq!(fs::read(&history_path).expect("history"), original_history);
    assert!(sb.home.join(CLAUDE_OLD).exists());

    // Empty the trash for good.
    let out = sb.run(&["trash", "empty", "--yes"]);
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        json(&sb.run(&["trash", "list", "--json"])),
        Value::Array(vec![])
    );
}

#[test]
fn trash_empty_without_yes_is_refused_when_not_a_terminal() {
    let sb = Sandbox::new();
    let out = sb.run(&["trash", "empty"]);
    assert_eq!(code(&out), 2);
}

#[test]
fn du_json_reports_agent_and_project_totals() {
    let sb = Sandbox::new();
    let out = sb.run(&["du", "--json"]);
    assert_eq!(code(&out), 0);
    let v = json(&out);
    assert!(v["agents"]
        .as_array()
        .expect("agents")
        .iter()
        .any(|a| a["agent"] == "claude"));
    assert!(v["projects"].is_array());
}

#[test]
fn preview_prints_session_messages() {
    let sb = Sandbox::new();
    let out = sb.run(&["preview", CLAUDE_OLD_KEY, "--json"]);
    assert_eq!(
        code(&out),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    assert_eq!(v["session"]["id"], "5f1c0a02-2b3c-4d5e-9f01-000000000002");
    assert!(v["messages"].as_array().expect("messages").len() <= 20);
}

#[test]
fn unknown_agent_is_an_error() {
    let sb = Sandbox::new();
    let out = sb.run(&["list", "--agent", "nope"]);
    assert_eq!(code(&out), 1);
}
