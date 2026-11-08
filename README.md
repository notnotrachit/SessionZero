# SessionZero

[![npm version](https://img.shields.io/npm/v/sessionzero.svg)](https://www.npmjs.com/package/sessionzero)
[![npm downloads](https://img.shields.io/npm/dm/sessionzero.svg)](https://www.npmjs.com/package/sessionzero)
[![Release](https://img.shields.io/github/v/release/notnotrachit/SessionZero)](https://github.com/notnotrachit/SessionZero/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![CI](https://github.com/notnotrachit/SessionZero/actions/workflows/ci.yml/badge.svg)](https://github.com/notnotrachit/SessionZero/actions/workflows/ci.yml)

SessionZero (`sz`) finds the session files that AI coding agents leave on disk, shows how much space each one uses, and moves the ones you pick to a restorable trash.

## Install

Pick one. Each command installs the `sz` binary.

Homebrew (macOS and Linux):

```sh
brew install notnotrachit/tap/sessionzero
```

npm, without installing (runs once):

```sh
npx sessionzero
```

npm, global install:

```sh
npm i -g sessionzero
```

Shell installer (macOS and Linux):

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/notnotrachit/SessionZero/releases/latest/download/sessionzero-installer.sh | sh
```

Cargo (builds from source):

```sh
cargo install --git https://github.com/notnotrachit/SessionZero sessionzero
```

After install, the command is `sz`. With npx, pass arguments after the package name: `npx sessionzero list`.

Windows is not supported.

## Quick start

```sh
sz                               # open the interactive menu
sz list                          # list sessions, biggest first
sz analyze                       # disk usage by project
sz clean --older 30d --dry-run   # show what would move to trash, move nothing
sz trash restore <id>            # undo a clean. Get the id from `sz trash list`
```

## Supported agents

| Agent | `--agent` label | Where it reads | What one session is |
|---|---|---|---|
| Codex CLI | `codex` | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, `~/.codex/archived_sessions/` | One rollout file |
| Claude Code | `claude` | `~/.claude/projects/<slug>/<uuid>.jsonl` | One transcript, plus its directories and history rows under `~/.claude` |
| Kiro CLI | `kiro` | `~/.kiro/sessions/cli/`, `~/.kiro/sessions/<workspace>/sess_<uuid>/` | One session id across its JSON, JSONL and directory files |
| Kiro Crew | `kiro-crew` | `~/.kiro/crew/sessions/*.jsonl` | One channel transcript |
| OpenCode | `opencode` | `~/.local/share/opencode/opencode.db` (and the legacy `storage/` tree) | Rows in the SQLite store for one session |
| Grok CLI | `grok` | `~/.grok/sessions/<url-encoded-cwd>/<session-id>/` | One session directory |
| Gemini CLI and Antigravity | `gemini` | `~/.gemini/tmp/<key>/chats/session-*.json`, Antigravity conversation stores | One chat file or conversation store |
| Cursor | `cursor` | `~/.cursor/projects/<slug>/agent-transcripts/`, composer rows in `globalStorage/state.vscdb` | One agent transcript or composer |
| VS Code (incl. Copilot Chat) | `vscode` | `~/Library/Application Support/Code/User/workspaceStorage/<hash>/chatSessions/` and `globalStorage/emptyWindowChatSessions/` (Code - Insiders and VSCodium too; `~/.config/Code/User` on Linux) | One chat session file, with its editing-session directory |
| GitHub Copilot CLI | `copilot` | `~/.copilot/session-state/<UUID>/` | One session directory |
| Pi coding agent | `pi` | `~/.pi/agent/sessions/<cwd-dir>/` (JSONL), plus `~/.pi/agent/state/workspace-history/` | One session JSONL file: a top-level session, a fork, or a subagent run |

Aliases also work, for example `claude-code`, `kiro-cli`, `crew`, `open-code`, `antigravity`, `vs-code`, `gh-copilot`, and `pi-agent`.

## Keys

Use these in the TUI (`sz`).

| Key | Action |
|---|---|
| `↑` `↓` or `j` `k` | Move |
| `PgUp` `PgDn`, `Home` `End` or `g` `G` | Page, top, bottom |
| `→` `Enter` or `l` | Open or expand |
| `←` `h` or `Backspace` | Collapse or go back |
| `Space` | Select or unselect |
| `v` | Start or end a range selection |
| `a` | Select or unselect all in view |
| `d` or `Delete` | Move selection to trash (asks `y` to confirm) |
| `p` | Toggle preview |
| `o` | Open the project folder |
| `g` or `Tab` (Analyze) | Cycle grouping: project, agent, folder |
| `s` | Cycle sort: size, recent, oldest, messages, name |
| `/` | Regex filter. `Enter` applies, `Esc` clears |
| `t` | Trash screen |
| `r` | Rescan |
| `?` | Help |
| `Esc` | Close a dialog, clear selection, or go back |
| `q` or `Ctrl-C` | Quit |

On the Trash screen: `r` restores the selected entry, `x` empties the selected entry, and `X` empties the whole trash (type `yes`).

## Commands

Global flags work with every command:

- `--json`: print machine-readable output
- `--agent <NAME>`: limit to one agent. Repeat for several
- `--home <PATH>`: read a different home directory (for fixtures and tests)
- `--force`: act on sessions that look live or unsafe to touch

| Command | What it does |
|---|---|
| `sz` | Open the interactive menu |
| `sz analyze` | Disk usage per project and age, as a tree |
| `sz list [--sort KEY] [--limit N] [--filter REGEX]` | List sessions. `KEY` is `size`, `recent`, `oldest`, `messages` or `name` |
| `sz clean [--older DUR] [--larger SIZE] [--project REGEX] [--no-project] [--dry-run] [--yes]` | Move sessions that match at least one rule to trash. A run with no rule is refused |
| `sz trash` | Open the Trash screen |
| `sz trash list` | List trash entries, with when each one is auto-deleted |
| `sz trash restore <id>` | Put an entry back where it was |
| `sz trash empty [<id>] [--older DUR] [--yes]` | Permanently delete one entry, the whole trash, or only entries trashed longer ago than `DUR` |
| `sz config` | Show settings |
| `sz config trash-retention <DAYS\|never>` | Set how long trashed sessions are kept before automatic deletion. `7`, `7d` and `never` are accepted. Default `30` |
| `sz agents` | Show detected agents, running state, session counts and data roots |
| `sz du` | Disk usage per agent and per project |
| `sz preview <agent:id>` | Show metadata and the first messages of one session |

Examples:

```sh
sz list --sort recent --limit 20
sz list --agent codex --filter 'refactor'
sz clean --agent claude --larger 500MB --dry-run
sz clean --project 'scratch' --yes
sz clean --no-project --older 90d --dry-run
sz preview codex:019c0001-aaaa-7bbb-8ccc-000000000001
```

`--older` takes a duration such as `30d` or `12h`. `--larger` takes a size such as `10MB`. Without a terminal, `clean` refuses to run unless you pass `--yes`.

A parent session never moves without its child sessions.

## Safety

- **Trash first.** Deleted sessions move to `~/.sessionzero/trash/<id>/`. Nothing is removed outright.
- **Restore.** `sz trash restore <id>` undoes a clean. It checks every target first. If a target path is taken, it stops and keeps the entry in the trash.
- **Running agents are guarded.** While an agent runs, SessionZero blocks sessions active in the last 2 hours, sessions with no known last activity, and sessions in a database the agent may write. Pass `--force` to override.
- **Shared files are edited, not removed.** If a session shares a file or database with others, only that session's lines or rows are removed. The manifest logs these edits, and restore reverses them.
- **`trash empty` is permanent.** It deletes the entry directories and runs `VACUUM` on the SQLite databases those entries touched, so the disk space returns to the OS. It cannot be undone.
- **Trash expires after 30 days by default.** At startup, SessionZero permanently deletes trash entries older than the retention period. The default is 30 days. A CLI run prints one line to stderr when it deletes anything. The Trash screen shows the retention and the days left for each entry. To change it, run `sz config trash-retention 7` (or `7d`). To keep trash forever, run `sz config trash-retention never`. To skip the check in a session, set `SZ_NO_AUTO_PURGE=1`. Restore still works on any entry until it is deleted.

## Uninstall

Remove the binary with the same tool you used to install it:

```sh
brew uninstall sessionzero
```

```sh
npm rm -g sessionzero
```

For the shell installer, delete `sz` from the folder it went into. Run `which sz` to find it. It is usually `~/.cargo/bin` or `~/.local/bin`.

To remove SessionZero's data, empty the trash first, because the trash lives in that folder:

```sh
sz trash empty --yes
rm -rf ~/.sessionzero
```

The agents' own session files are not touched by uninstalling.

## Data directory

SessionZero stores its trash, scan cache and settings (`config.json`, which holds the trash retention) in `~/.sessionzero/`. To use another location, set `SZ_DATA_DIR`:

```sh
SZ_DATA_DIR=/path/to/sz-data sz trash list
```

The agents' session stores are read from your home directory, or from `--home` when you pass it.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```

- `crates/sz-core` is the engine: adapters, scan, grouping, rules, cache and trash.
- `crates/sz-tui` is the `sz` binary. The CLI is in `cli.rs` and the TUI is in `app.rs` and `ui.rs`.
- Test fixtures under `tests/fixtures/` are synthetic. Keep them that way: no real session content, project names, emails or machine paths.
- Tests that read your real home directory are marked `#[ignore]`. Run them on purpose with `cargo test -- --ignored`.
- The engine API is in `docs/core-api.md`. The TUI spec is in `docs/tui-spec.md`.

## Releasing

Releases are built by [cargo-dist](https://axodotdev.github.io/cargo-dist/) with `.github/workflows/release.yml`. Pushing a version tag builds the binaries, creates a GitHub release, and publishes the Homebrew formula and the npm package.

1. Push a version tag:

   ```sh
   git tag v0.1.0 && git push origin v0.1.0
   ```

2. Make sure these repository secrets exist:
   - `HOMEBREW_TAP_TOKEN`: fine-grained token with contents write access to `notnotrachit/homebrew-tap`
   - `NPM_TOKEN`: npm automation token that can publish `sessionzero`

After you change `dist-workspace.toml`, run `dist generate` and commit the regenerated workflow. Run `dist plan` to preview a release.
