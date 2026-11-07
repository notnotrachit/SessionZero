# sz TUI spec (ratatui + crossterm)

Style reference: npkill (flat list, live sizes, Space/Del delete) + Mole `mo analyze` (drill-down with size bars, menu).

## Entry point (crates/sz-tui/src/app.rs)
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode { Menu, List, Analyze, Trash }
pub struct Options { pub home: PathBuf, pub force: bool, pub agents: Option<Vec<AgentKind>> }
pub fn run(opts: Options, mode: Mode) -> anyhow::Result<()>;
```
`main.rs` (owned by the CLI worker) calls this. Restore the terminal on panic and on exit (raw mode off, leave alternate screen, show cursor).

## Data flow
1. On start, show `cache::load()` immediately if present (mark "cached").
2. Start `scan::scan_stream(home, agents)`. Merge each `AgentDone` into the session list (replace that agent's rows). Header shows `scanning… <agents done>/<total>` with a spinner, then `<N> sessions · <total size>`. Save `cache::save` on `Finished`.
3. Never block the UI thread on scanning, preview loading or trash IO longer than a frame; use threads + channels. Poll input every ~50 ms.

## Screens
### Menu (`sz`)
Centered list: Analyze (tree), List (biggest first), Trash, Agents, Quit. Shows totals per agent with bars once scanned.

### List (npkill style, `sz list`)
Columns: `[x]` select, agent (colored tag), title (fallback `(untitled)` / id), project (`group::display_path`), last active (relative: `2h`, `3d`, `5mo`), msgs, size (right aligned, `group::human_bytes`), size bar. Sorted by size desc by default. Rows from a running agent that are recent show `●` (live) in yellow. Footer shows selected count + bytes and key hints.

### Analyze (Mole style, `sz analyze`)
Tree from `group::group(sessions, by, sort)`: level 1 groups, level 2 children, level 3 sessions. `→`/Enter expands or drills in; `←`/Backspace collapses / goes up. Breadcrumb at top (`Project › ~/code/app › codex`). Each row: label, size, bar relative to the largest sibling, count, last active. `(no project)` group always last-but-sorted normally. Selecting a group selects all its sessions.

### Preview (`p`, or right pane when terminal width >= 140)
Session metadata (agent, id, title, project, created, last active, messages, size, artifact list with per-artifact size and type) + first messages from `adapter.preview(s, 20)` (find the adapter via `adapters::all_adapters(home)` by kind), loaded in a background thread.

### Delete confirm (modal)
On `d`/Del with a selection (or the cursor row when nothing is selected): run `trash::plan(home, sessions, force)`; show count, bytes to free, list of blocked sessions with reasons, and "Move to trash? (y/N)". On `y` run `Trash::default_location().execute(&plan)` in a background thread; show a progress line, then a toast "Moved N sessions (1.2 GB) to trash · t to view". Remove deleted rows from the list. Errors from `TrashEntry.errors` shown in the toast/modal.

### Trash (`t`, `sz trash` interactive)
List of `Trash::list()` entries: id, created, sessions, bytes. `r` restore (confirm), `x` empty selected entry (confirm, red warning "permanent"), `X` empty all (confirm by typing `yes`). Restored sessions reappear after a rescan.

## Keys
| Key | Action |
|---|---|
| ↑↓ / j k, PgUp/PgDn, Home/End (g/G) | move |
| → / Enter / l, ← / h / Backspace | open/expand, back/collapse |
| Space | toggle select |
| v | range select start/end |
| a | select / unselect all in view |
| d / Del | delete selected (to trash) |
| p | toggle preview |
| o | open project folder (`open` on macOS) |
| g (Analyze) / Tab | cycle group: Project → Agent → Folder |
| s | cycle sort: Size → Recent → Oldest → Messages → Name |
| / | regex filter (`group::filter`), Enter apply, Esc clear |
| t | trash screen |
| r | rescan |
| ? | help overlay |
| Esc | close modal / clear selection / back |
| q / Ctrl-C | quit |

`g` in List mode = go to top (vim). In Analyze, use Tab for grouping and `g` also cycles grouping. Document this in the help overlay.

## Look
- Rounded borders, one accent color, agent tags with fixed colors (codex green, claude orange, kiro purple, kiro-crew magenta, opencode cyan, grok white, gemini blue, cursor gray).
- Size bars with `█`/`░`, color by size: >1 GB red, >100 MB yellow, else green.
- Works on 80x24; columns collapse (drop project, then msgs) when narrow.

## Code layout (TUI worker owns all of these)
`crates/sz-tui/src/app.rs` (state, event loop, run), `ui.rs` (rendering), optional `tui/*.rs` submodules. Keep state logic testable without a terminal; render tests with `ratatui::backend::TestBackend`.
