# SessionZero

SessionZero (`sz`) finds the session files that AI coding agents leave on disk, shows how much space each one takes, groups them by project or agent, and moves the ones you pick to a restorable trash. It works like npkill or Mole: a terminal UI for browsing and deleting, plus a CLI for scripts.

Nothing is removed outright by default. Deleted sessions go to a trash you can restore from. A session that may still be in use by a running agent is blocked unless you pass `--force`.

## Supported agents

| Agent | Label (`--agent`) | Where it reads | What a session is |
|---|---|---|---|
| Codex CLI | `codex` | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, `~/.codex/archived_sessions/` | One rollout file |
| Claude Code | `claude` | `~/.claude/projects/<slug>/<uuid>.jsonl` | One transcript, plus that session's directories and history rows under `~/.claude` |
| Kiro CLI | `kiro` | `~/.kiro/sessions/cli/<uuid>.json` and `.jsonl`, `~/.kiro/sessions/<workspace>/sess_<uuid>/` | One session id across its JSON, JSONL and directory files |
| Kiro Crew | `kiro-crew` | `~/.kiro/crew/sessions/*.jsonl` | One channel transcript |
| OpenCode | `opencode` | `~/.local/share/opencode/opencode.db` (and the legacy `storage/` tree) | Rows in the SQLite store for one session |
| Grok CLI | `grok` | `~/.grok/sessions/<url-encoded-cwd>/<session-id>/` | One session directory |
| Gemini CLI and Antigravity | `gemini` | `~/.gemini/tmp/<key>/chats/session-*.json`, Antigravity conversation stores | One chat file or conversation store |
| Cursor | `cursor` | `~/.cursor/projects/<slug>/agent-transcripts/`, composer rows in Cursor's `globalStorage/state.vscdb` | One agent transcript or composer |
| VS Code | `vscode` | `~/Library/Application Support/Code/User/workspaceStorage/<hash>/chatSessions/` and `globalStorage/emptyWindowChatSessions/` (also Code - Insiders and VSCodium; `~/.config/Code/User` on Linux) | One chat session file, with its editing-session directory |
| GitHub Copilot CLI | `copilot` | `~/.copilot/session-state/<UUID>/` | One session directory, with its per-session state |
| Pi coding agent | `pi` | `~/.pi/agent/sessions/<cwd-dir>/` (JSONL files), plus per-session state under `~/.pi/agent/state/workspace-history/` | One session JSONL file: a top-level session, a fork, or a subagent run |

Agent aliases also work: `claude-code`, `kiro-cli`, `kiro_crew`, `crew`, `open-code`, `antigravity`, `code`, `vs-code`, `vscode-insiders`, `gh-copilot`, `github-copilot`, `pi-agent`, `pi-coding-agent`.

## Install

Pick one. Each one installs the `sz` binary. Run `sz` with no arguments to open the TUI menu.

```sh
# Homebrew (macOS and Linux)
brew install notnotrachit/tap/sessionzero

# npm: runs once without installing, or installs globally
npx sessionzero
npm i -g sessionzero

# Shell installer (macOS and Linux)
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/notnotrachit/SessionZero/releases/latest/download/sessionzero-installer.sh | sh

# From source
cargo install --git https://github.com/notnotrachit/SessionZero sessionzero
```

The npm package downloads the prebuilt binary for your platform from the GitHub release. Building from a local checkout also works: `cargo install --path crates/sz-tui`.

## Usage

Global flags work with every command: `--json`, `--home <PATH>` (read a different home directory, used for fixtures), `--agent <NAME>` (repeatable), and `--force`.

### `sz` (no command)

Opens the interactive menu: Analyze, List, Trash, Agents.

### `sz analyze`

Disk usage per project and agent, as an interactive tree. With `--json` it prints the same tree as JSON.

```sh
sz analyze
sz analyze --json
```

### `sz list`

Lists sessions. In a terminal it opens the List screen. Piped output or `--json` prints a table or a JSON array.

```sh
sz list --sort recent --limit 20
sz list --agent codex --filter 'refactor'
sz list --json
```

Options:

- `--sort <KEY>`: `size` (default), `recent`, `oldest`, `messages`, `name`
- `--limit <N>`: show at most N sessions
- `--filter <REGEX>`: match against title, project, agent or id

### `sz clean`

Moves sessions that match at least one rule to the trash. A run with no rule is refused.

```sh
sz clean --older 30d --dry-run
sz clean --agent claude --larger 500MB
sz clean --project 'scratch' --yes
sz clean --no-project --older 90d --dry-run
```

Options:

- `--older <DUR>`: last activity older than a duration, such as `30d` or `12h`
- `--larger <SIZE>`: larger than a size, such as `10MB`
- `--project <REGEX>`: project path matches a regex
- `--no-project`: sessions with no project
- `--dry-run`: print the plan and move nothing
- `--yes`: skip the `[y/N]` prompt. Without a terminal and without `--yes`, the command refuses.

A parent session is never moved without its child sessions.

### `sz trash`

```sh
sz trash list
sz trash restore 20261107-093012-a1b2c3d4
sz trash empty 20261107-093012-a1b2c3d4 --yes
sz trash empty --yes
```

- `sz trash` with no subcommand opens the Trash screen.
- `restore <id>` puts every file and database row of the entry back where it was. It refuses when a target path is already occupied, and the entry stays in the trash.
- `empty [<id>]` deletes one entry, or the whole trash when no id is given. It asks you to type `yes` unless you pass `--yes`.

### `sz agents`

Shows, for each agent: whether it is installed, whether it is running, how many sessions and bytes it has, and which data roots were checked (missing roots are marked).

### `sz du`

Disk usage per agent and per project.

### `sz preview <agent:id>`

Metadata and the first messages of one session.

```sh
sz preview codex:019c0001-aaaa-7bbb-8ccc-000000000001
```

## TUI keys

| Key | Action |
|---|---|
| `↑` `↓` / `j` `k` | Move |
| `PgUp` `PgDn`, `Home` `End` / `g` `G` | Page, top, bottom |
| `→` / `Enter` / `l` | Open or expand |
| `←` / `h` / `Backspace` | Collapse or go up (Analyze) |
| `Tab` (Analyze) or `g` (Analyze) | Cycle grouping: project, agent, folder |
| `Space` | Toggle selection |
| `v` | Start or end a range selection |
| `a` | Select or unselect all in view |
| `d` / `Delete` | Move selection to trash (asks `y` to confirm) |
| `p` | Toggle preview |
| `o` | Open the project folder |
| `s` | Cycle sort: size, recent, oldest, messages, name |
| `/` | Regex filter. `Enter` applies, `Esc` clears |
| `t` | Trash screen |
| `r` | Rescan |
| Trash screen: `r` / `x` / `X` | Restore selected (confirm) / empty selected (confirm) / empty all (type `yes`) |
| `?` | Help |
| `Esc` | Close a dialog, clear selection, or go back |
| `q` / `Ctrl-C` | Quit |

## Safety model

- **Trash first.** Every delete moves files into a trash entry under `~/.sessionzero/trash/<id>/`. A `manifest.json` records each change, so restore can undo them in reverse order.
- **Shared files are edited, not removed.** Where a session shares a file or database with other sessions, SessionZero removes only that session's lines or rows. Those edits are logged in the manifest and reversed on restore. Writes go through a temp file and a rename, so a crash does not leave a half-written file.
- **Running agents are guarded.** When an agent is running, SessionZero blocks a session that was active in the last 2 hours, has no known last activity, or is stored in a database the agent may write. Use `--force` to override this check.
- **Restore checks first.** `trash restore` verifies every precondition before it changes anything. If a target path is taken, it stops and keeps the entry.
- **`trash empty` is permanent.** It deletes the entry directories, then runs `VACUUM` on each SQLite database those entries touched, so the freed space is returned to the OS. It cannot be undone. Lock errors during `VACUUM` are ignored.

## Data directory

SessionZero keeps its own data (trash and scan cache) in `~/.sessionzero/`. Set `SZ_DATA_DIR` to use another directory:

```sh
SZ_DATA_DIR=/path/to/sz-data sz trash list
```

The agents' own session stores are read from the real home directory, or from `--home` when you pass it.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```

- Fixtures under `tests/fixtures/` are synthetic. They are a fake home directory per agent (`--home` points at them). Keep them synthetic: no real session content, project names, emails or machine paths.
- Live tests that read your real home directory are marked `#[ignore]` and skipped by default. The adapter `live_scan` tests only read. Run them on purpose with `cargo test -- --ignored`. The TUI `snapshot_real_home` test writes its output under `$KIROCREW_SCRATCH` (or `/tmp`).
- Layout: `crates/sz-core` is the engine (adapters, scan, group, rules, cache, trash). `crates/sz-tui` is the `sz` binary (CLI in `cli.rs`, TUI in `app.rs` and `ui.rs`).
- Format notes for each agent are in `docs/formats/`. The engine API is in `docs/core-api.md`, and the TUI spec is in `docs/tui-spec.md`.

## Release (maintainers)

Releases are built by [cargo-dist](https://axodotdev.github.io/cargo-dist/) from `.github/workflows/release.yml`, which `dist generate` writes from `dist-workspace.toml`. Pushing a version tag builds the binaries for macOS and Linux, creates a GitHub release, and publishes the Homebrew formula and npm package.

Repository secrets needed:

- `HOMEBREW_TAP_TOKEN`: fine-grained personal access token with contents write access to `notnotrachit/homebrew-tap`.
- `NPM_TOKEN`: npm automation token that can publish `sessionzero`.

Release:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

After changing `dist-workspace.toml`, run `dist generate` and commit the regenerated workflow. Run `dist plan` to see what a release will produce.
