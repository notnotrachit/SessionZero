//! TUI rendering. Every function here is a pure view of `App` state, so tests can render into a
//! `TestBackend` and assert on the text.

use chrono::{DateTime, Utc};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, Paragraph, Row as TRow, Table, TableState, Wrap,
};
use ratatui::Frame;
use sz_core::group;
use sz_core::model::{AgentKind, Artifact};
use sz_core::util::truncate;

use crate::app::{App, DeleteModal, MenuItem, Modal, Row, Screen, TrashModal, MENU_ITEMS};

const ACCENT: Color = Color::Cyan;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const GB: u64 = 1 << 30;
const MB100: u64 = 100 * (1 << 20);

pub fn render(f: &mut Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(3),
    ])
    .split(area);

    f.render_widget(Paragraph::new(header_line(app)), chunks[0]);
    match app.screen {
        Screen::Menu => render_menu(f, chunks[1], app),
        Screen::List | Screen::Analyze => render_browse(f, chunks[1], app),
        Screen::Trash => render_trash(f, chunks[1], app),
        Screen::Agents => render_agents(f, chunks[1], app),
    }
    render_footer(f, chunks[2], app);
    if let Some(modal) = &app.modal {
        render_modal(f, area, modal, app);
    }
}

fn spinner(tick: usize) -> &'static str {
    SPINNER[tick % SPINNER.len()]
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn bordered(title: impl Into<String>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title.into())
}

fn screen_name(s: Screen) -> &'static str {
    match s {
        Screen::Menu => "menu",
        Screen::List => "list",
        Screen::Analyze => "analyze",
        Screen::Trash => "trash",
        Screen::Agents => "agents",
    }
}

fn group_name(app: &App) -> &'static str {
    match app.group_by {
        group::GroupBy::Project => "Project",
        group::GroupBy::Agent => "Agent",
        group::GroupBy::Folder => "Folder",
    }
}

fn sort_name(app: &App) -> &'static str {
    match app.sort {
        group::SortBy::Size => "size",
        group::SortBy::Recent => "recent",
        group::SortBy::Oldest => "oldest",
        group::SortBy::Messages => "messages",
        group::SortBy::Name => "name",
    }
}

fn agent_style(kind: AgentKind) -> Style {
    let color = match kind {
        AgentKind::Codex => Color::Green,
        AgentKind::Claude => Color::Rgb(217, 119, 87),
        AgentKind::KiroCli => Color::Rgb(144, 96, 255),
        AgentKind::KiroCrew => Color::Magenta,
        AgentKind::OpenCode => Color::Cyan,
        AgentKind::Grok => Color::White,
        AgentKind::Gemini => Color::Blue,
        AgentKind::Cursor => Color::DarkGray,
        AgentKind::Vscode => Color::LightBlue,
        AgentKind::Copilot => Color::LightGreen,
        AgentKind::Pi => Color::Yellow,
    };
    Style::default().fg(color)
}

fn size_color(bytes: u64) -> Color {
    if bytes >= GB {
        Color::Red
    } else if bytes >= MB100 {
        Color::Yellow
    } else {
        Color::Green
    }
}

/// `█` / `░` bar, scaled so `max` fills `width` cells. Non-zero sizes always show one cell.
pub fn size_bar(bytes: u64, max: u64, width: usize) -> Vec<Span<'static>> {
    let filled = if max == 0 || bytes == 0 {
        0
    } else {
        (((bytes as f64 / max as f64) * width as f64).round() as usize).clamp(1, width)
    };
    vec![
        Span::styled("█".repeat(filled), Style::default().fg(size_color(bytes))),
        Span::styled("░".repeat(width - filled), dim()),
    ]
}

/// Compact age: `now`, `5m`, `2h`, `3d`, `5mo`, `2y`.
pub fn rel_time_at(t: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(t) = t else {
        return "-".to_string();
    };
    let mins = (now - t).num_minutes().max(0);
    let hours = mins / 60;
    let days = hours / 24;
    if mins < 1 {
        "now".to_string()
    } else if mins < 60 {
        format!("{mins}m")
    } else if hours < 48 {
        format!("{hours}h")
    } else if days < 60 {
        format!("{days}d")
    } else if days < 365 {
        format!("{}mo", days / 30)
    } else {
        format!("{}y", days / 365)
    }
}

fn human(bytes: u64) -> String {
    group::human_bytes(bytes)
}

fn header_line(app: &App) -> Line<'static> {
    let total: u64 = app.sessions.iter().map(|s| s.bytes).sum();
    let mut spans = vec![
        Span::styled(
            " sz ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(screen_name(app.screen), bold()),
        Span::raw("  "),
    ];
    if app.scanning {
        spans.push(Span::styled(
            format!(
                "{} scanning… {}/{} agents",
                spinner(app.tick),
                app.scan_done,
                app.scan_total
            ),
            Style::default().fg(Color::Yellow),
        ));
        spans.push(Span::raw(" · "));
    } else if app.cached {
        spans.push(Span::styled("cached", dim()));
        spans.push(Span::raw(" · "));
    }
    spans.push(Span::raw(format!(
        "{} sessions · {}",
        app.sessions.len(),
        human(total)
    )));
    Line::from(spans)
}

fn hints(screen: Screen) -> &'static str {
    match screen {
        Screen::Menu => "↑↓ move · Enter open · a analyze · l list · t trash · g agents · ? help · q quit",
        Screen::List => {
            "↑↓ move · space select · v range · a all · d delete · p preview · o open · s sort · / filter · t trash · r rescan · ? help · q quit"
        }
        Screen::Analyze => {
            "↑↓ move · → open · ← up · Tab group · space select · v range · d delete · p preview · s sort · / filter · t trash · ? help · q quit"
        }
        Screen::Trash => "↑↓ move · r restore · x empty entry · X empty all · Esc menu · q quit",
        Screen::Agents => "r rescan · Esc menu · q quit",
    }
}

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::vertical([Constraint::Length(2), Constraint::Length(1)]).split(area);
    let hint = match &app.filter_input {
        Some(buf) => format!("/{buf}▏  Enter apply · Esc cancel"),
        None => hints(app.screen).to_string(),
    };
    f.render_widget(Paragraph::new(hint).style(dim()).wrap(Wrap { trim: true }), rows[0]);

    let mut parts: Vec<String> = Vec::new();
    let selected = app.selected_indices();
    if !selected.is_empty() {
        let bytes: u64 = selected.iter().map(|&i| app.sessions[i].bytes).sum();
        parts.push(format!("{} selected · {}", selected.len(), human(bytes)));
    }
    if let Some(busy) = &app.busy {
        parts.push(format!("{} {busy}", spinner(app.tick)));
    }
    if let Some(toast) = app.toast_text() {
        parts.push(toast.to_string());
    }
    if let Some(filter) = &app.filter {
        parts.push(format!("filter: /{filter}"));
    }
    f.render_widget(
        Paragraph::new(parts.join("  ·  ")).style(Style::default().fg(ACCENT)),
        rows[1],
    );
}

fn render_menu(f: &mut Frame, area: Rect, app: &App) {
    let block = bordered(" menu ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    for (i, item) in MENU_ITEMS.iter().enumerate() {
        let (label, desc) = menu_text(*item);
        let style = if i == app.menu_cursor {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {label:<9}"), style),
            Span::styled(format!(" {desc}"), dim()),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(" totals by agent", bold())));
    let mut totals: Vec<(AgentKind, usize, u64)> = AgentKind::all()
        .iter()
        .map(|&k| {
            let (n, b) = app
                .sessions
                .iter()
                .filter(|s| s.agent == k)
                .fold((0usize, 0u64), |(n, b), s| (n + 1, b + s.bytes));
            (k, n, b)
        })
        .filter(|(_, n, _)| *n > 0)
        .collect();
    totals.sort_by(|a, b| b.2.cmp(&a.2));
    let max = totals.first().map(|t| t.2).unwrap_or(0);
    if totals.is_empty() {
        let msg = if app.scanning {
            "  scanning…"
        } else {
            "  no sessions found"
        };
        lines.push(Line::from(Span::styled(msg, dim())));
    }
    for (kind, n, bytes) in totals {
        let mut spans = vec![
            Span::styled(format!(" {:<10}", kind.label()), agent_style(kind)),
            Span::raw(format!("{n:>6} sessions  {:>9}  ", human(bytes))),
        ];
        spans.extend(size_bar(bytes, max, 20));
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn menu_text(item: MenuItem) -> (&'static str, &'static str) {
    match item {
        MenuItem::Analyze => ("Analyze", "tree by project, agent or folder with size bars"),
        MenuItem::List => ("List", "flat list, biggest first"),
        MenuItem::Trash => ("Trash", "restore or permanently empty deleted sessions"),
        MenuItem::Agents => ("Agents", "detected agents and their data roots"),
        MenuItem::Quit => ("Quit", ""),
    }
}

/// Splits the body into a main pane and an optional preview pane.
fn split_preview(area: Rect, preview_on: bool) -> (Rect, Option<Rect>) {
    if !preview_on {
        return (area, None);
    }
    if area.width >= 140 {
        let c = Layout::horizontal([Constraint::Min(60), Constraint::Length(64)]).split(area);
        (c[0], Some(c[1]))
    } else {
        let c = Layout::vertical([Constraint::Min(8), Constraint::Length(13)]).split(area);
        (c[0], Some(c[1]))
    }
}

fn render_browse(f: &mut Frame, area: Rect, app: &App) {
    let (main, preview) = split_preview(area, app.preview_on);
    let rows = app.rows();
    let title = if app.screen == Screen::Analyze {
        let mut crumbs = vec![group_name(app).to_string()];
        crumbs.extend(app.breadcrumb());
        format!(" analyze · {} ", crumbs.join(" › "))
    } else {
        let filter = app
            .filter
            .as_ref()
            .map(|p| format!(" · filter /{p}"))
            .unwrap_or_default();
        format!(" sessions · sort: {}{filter} ", sort_name(app))
    };
    let block = bordered(title);

    if rows.is_empty() {
        let msg = if app.scanning && app.sessions.is_empty() {
            "Scanning…"
        } else if app.sessions.is_empty() {
            "No sessions found. Press r to rescan."
        } else if app.filter.is_some() {
            "No sessions match the filter. Press Esc to clear it."
        } else {
            "Nothing here. Esc goes back."
        };
        f.render_widget(Paragraph::new(msg).style(dim()).block(block), main);
    } else {
        let table = if app.screen == Screen::Analyze {
            analyze_table(app, &rows, block)
        } else {
            list_table(app, &rows, main.width, block)
        };
        let mut state = TableState::default();
        state.select(Some(app.cursor()));
        f.render_stateful_widget(table, main, &mut state);
    }

    if let Some(p) = preview {
        render_preview(f, p, app);
    }
}

fn list_table(app: &App, rows: &[Row], width: u16, block: Block<'static>) -> Table<'static> {
    let max = rows.iter().map(|r| r.bytes).max().unwrap_or(0);
    let now = Utc::now();
    let show_project = width >= 110;
    let show_msgs = width >= 90;

    let mut widths = vec![
        Constraint::Length(3),
        Constraint::Length(12),
        Constraint::Min(10),
    ];
    let mut header = vec!["", "agent", "title"];
    if show_project {
        widths.push(Constraint::Length(26));
        header.push("project");
    }
    widths.push(Constraint::Length(4));
    header.push("last");
    if show_msgs {
        widths.push(Constraint::Length(5));
        header.push("msgs");
    }
    widths.push(Constraint::Length(9));
    header.push("size");
    widths.push(Constraint::Length(10));
    header.push("");

    let body: Vec<TRow> = rows
        .iter()
        .map(|r| {
            let selected = is_selected(app, r);
            let mut cells: Vec<Cell> = vec![Cell::from(Line::from(Span::styled(
                if selected { "[x]" } else { "[ ]" },
                if selected {
                    Style::default().fg(Color::Green)
                } else {
                    dim()
                },
            )))];
            let mut agent_spans = vec![Span::styled(
                if r.live { "● " } else { "  " },
                Style::default().fg(Color::Yellow),
            )];
            if let Some(agent) = r.agent {
                agent_spans.push(Span::styled(agent.label(), agent_style(agent)));
            }
            cells.push(Cell::from(Line::from(agent_spans)));
            cells.push(Cell::from(Line::from(r.label.clone())));
            if show_project {
                let project = r
                    .project
                    .as_ref()
                    .map(|p| group::display_path(p))
                    .unwrap_or_else(|| "-".to_string());
                cells.push(Cell::from(Line::from(truncate(&project, 26))));
            }
            cells.push(Cell::from(Line::from(rel_time_at(r.last, now))));
            if show_msgs {
                let msgs = r.msgs.map(|m| m.to_string()).unwrap_or_else(|| "-".to_string());
                cells.push(Cell::from(Line::from(msgs).alignment(Alignment::Right)));
            }
            cells.push(Cell::from(
                Line::from(human(r.bytes)).alignment(Alignment::Right),
            ));
            cells.push(Cell::from(Line::from(size_bar(r.bytes, max, 10))));
            TRow::new(cells)
        })
        .collect();

    Table::new(body, widths)
        .header(TRow::new(header).style(bold()))
        .block(block)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .column_spacing(1)
}

/// A row is selected when it has sessions and every one of them is selected.
fn is_selected(app: &App, r: &Row) -> bool {
    !r.sessions.is_empty()
        && r.sessions
            .iter()
            .all(|&i| app.selected.contains(&app.sessions[i].key()))
}

fn analyze_table(app: &App, rows: &[Row], block: Block<'static>) -> Table<'static> {
    let max = rows.iter().map(|r| r.bytes).max().unwrap_or(0);
    let widths = vec![
        Constraint::Length(3),
        Constraint::Min(10),
        Constraint::Length(11),
        Constraint::Length(9),
        Constraint::Length(10),
        Constraint::Length(6),
        Constraint::Length(4),
    ];
    let header = TRow::new(vec!["", "name", "agent", "size", "", "count", "last"]).style(bold());
    let now = Utc::now();
    let body: Vec<TRow> = rows
        .iter()
        .map(|r| {
            let selected = is_selected(app, r);
            let mark = Cell::from(Line::from(Span::styled(
                if selected { "[x]" } else { "[ ]" },
                if selected {
                    Style::default().fg(Color::Green)
                } else {
                    dim()
                },
            )));
            let name = if r.is_session {
                r.label.clone()
            } else {
                format!("▸ {}", r.label)
            };
            let agent = r.agent.map(|a| a.label()).unwrap_or("");
            let agent_cell = Cell::from(Line::from(Span::styled(
                agent,
                r.agent.map(agent_style).unwrap_or_default(),
            )));
            TRow::new(vec![
                mark,
                Cell::from(Line::from(name)),
                agent_cell,
                Cell::from(Line::from(human(r.bytes)).alignment(Alignment::Right)),
                Cell::from(Line::from(size_bar(r.bytes, max, 10))),
                Cell::from(Line::from(r.count.to_string()).alignment(Alignment::Right)),
                Cell::from(Line::from(rel_time_at(r.last, now))),
            ])
        })
        .collect();
    Table::new(body, widths)
        .header(header)
        .block(block)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .column_spacing(1)
}

fn artifact_kind(a: &Artifact) -> &'static str {
    match a {
        Artifact::File(_) => "file",
        Artifact::Dir(_) => "dir",
        Artifact::DbRows { .. } | Artifact::DbPrefix { .. } => "sqlite",
        Artifact::JsonlLines { .. } => "jsonl",
        Artifact::JsonArrayEntry { .. } | Artifact::JsonObjectKey { .. } => "json",
    }
}

fn kv(key: &str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<12}"), dim()),
        Span::raw(value.into()),
    ])
}

fn preview_lines(app: &App) -> Vec<Line<'static>> {
    let Some((key, s)) = app.preview_session() else {
        return vec![Line::from(Span::styled(
            "Select a session to preview. p hides this pane.",
            dim(),
        ))];
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(format!("{:<12}", "agent"), dim()),
            Span::styled(s.agent.label(), agent_style(s.agent)),
        ]),
        kv("id", s.id.clone()),
        kv(
            "title",
            s.title.clone().unwrap_or_else(|| "(untitled)".to_string()),
        ),
        kv(
            "project",
            s.project
                .as_ref()
                .map(|p| group::display_path(p))
                .unwrap_or_else(|| "(no project)".to_string()),
        ),
        kv(
            "created",
            s.created
                .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "-".to_string()),
        ),
        kv("last active", rel_time_at(s.last_active, Utc::now())),
        kv(
            "messages",
            s.message_count
                .map(|m| m.to_string())
                .unwrap_or_else(|| "-".to_string()),
        ),
        kv("size", human(s.bytes)),
        Line::from(""),
        Line::from(Span::styled("artifacts", bold())),
    ];
    for a in &s.artifacts {
        let size = if a.is_shared() {
            "shared".to_string()
        } else {
            human(a.own_bytes())
        };
        lines.push(Line::from(format!(
            "  {:<7} {:>9}  {}",
            artifact_kind(a),
            size,
            group::display_path(a.primary_path())
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("first messages", bold())));
    match app.previews.get(&key) {
        None => lines.push(Line::from(Span::styled("  loading…", dim()))),
        Some(Err(e)) => lines.push(Line::from(Span::styled(
            format!("  could not load: {e}"),
            Style::default().fg(Color::Red),
        ))),
        Some(Ok(msgs)) if msgs.is_empty() => {
            lines.push(Line::from(Span::styled("  (no messages)", dim())))
        }
        Some(Ok(msgs)) => {
            for m in msgs {
                lines.push(Line::from(format!(
                    "[{}] {}",
                    m.role,
                    truncate(&m.text.replace('\n', " "), 200)
                )));
            }
        }
    }
    lines
}

fn render_preview(f: &mut Frame, area: Rect, app: &App) {
    let lines = preview_lines(app);
    f.render_widget(
        Paragraph::new(lines)
            .block(bordered(" preview "))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_trash(f: &mut Frame, area: Rect, app: &App) {
    let block = bordered(" trash · r restore · x empty entry · X empty all ");
    if app.trash_entries.is_empty() {
        f.render_widget(
            Paragraph::new("Trash is empty.").style(dim()).block(block),
            area,
        );
        return;
    }
    let widths = [
        Constraint::Min(20),
        Constraint::Length(17),
        Constraint::Length(10),
        Constraint::Length(10),
    ];
    let header = TRow::new(vec!["id", "moved", "sessions", "size"]).style(bold());
    let body: Vec<TRow> = app
        .trash_entries
        .iter()
        .map(|e| {
            TRow::new(vec![
                Cell::from(e.id.clone()),
                Cell::from(e.created.format("%Y-%m-%d %H:%M").to_string()),
                Cell::from(Line::from(e.sessions.len().to_string()).alignment(Alignment::Right)),
                Cell::from(Line::from(human(e.bytes)).alignment(Alignment::Right)),
            ])
        })
        .collect();
    let table = Table::new(body, widths)
        .header(header)
        .block(block)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .column_spacing(1);
    let mut state = TableState::default();
    state.select(Some(app.trash_cursor));
    f.render_stateful_widget(table, area, &mut state);
}

fn render_agents(f: &mut Frame, area: Rect, app: &App) {
    let block = bordered(" agents ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let mut lines: Vec<Line> = Vec::new();
    if app.agent_infos.is_empty() {
        lines.push(Line::from(Span::styled(
            if app.scanning {
                "detecting agents…"
            } else {
                "no agents detected"
            },
            dim(),
        )));
    }
    for info in &app.agent_infos {
        let state = match (info.detected, info.running) {
            (false, _) => "not installed",
            (true, true) => "running",
            (true, false) => "idle",
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {:<10}", info.kind.label()), agent_style(info.kind)),
            Span::raw(format!(" {state}")),
        ]));
        for (root, exists) in &info.roots {
            let mark = if *exists { "✓" } else { "✗" };
            lines.push(Line::from(Span::styled(
                format!("     {mark} {}", group::display_path(root)),
                dim(),
            )));
        }
        if let Some(err) = app.scan_errors.get(&info.kind) {
            lines.push(Line::from(Span::styled(
                format!("     scan error: {err}"),
                Style::default().fg(Color::Red),
            )));
        }
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn centered(area: Rect, pct_w: u16, h: u16) -> Rect {
    let h = h.min(area.height);
    let w = (area.width * pct_w / 100).max(30).min(area.width);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn render_modal(f: &mut Frame, area: Rect, modal: &Modal, app: &App) {
    let (title, lines, color) = match modal {
        Modal::Help => (" help ", help_lines(), ACCENT),
        Modal::Delete(DeleteModal::Checking { count, .. }) => (
            " delete ",
            vec![
                Line::from(format!("Checking {count} selected sessions for running agents…")),
                Line::from(""),
                Line::from(Span::styled("Esc cancels", dim())),
            ],
            Color::Red,
        ),
        Modal::Delete(DeleteModal::Confirm { plan, children }) => {
            let mut head = format!(
                "Move {} sessions ({}) to trash?",
                plan.sessions.len(),
                human(plan.bytes)
            );
            if *children > 0 {
                head.push_str(&format!(" +{children} child sessions"));
            }
            let mut lines = vec![Line::from(head)];
            lines.push(Line::from(""));
            for s in plan.sessions.iter().take(8) {
                lines.push(Line::from(format!(
                    "  {:<10} {} · {}",
                    s.agent.label(),
                    s.title.clone().unwrap_or_else(|| s.id.clone()),
                    s.project
                        .as_ref()
                        .map(|p| group::display_path(p))
                        .unwrap_or_else(|| "(no project)".to_string())
                )));
            }
            if plan.sessions.len() > 8 {
                lines.push(Line::from(format!(
                    "  … and {} more",
                    plan.sessions.len() - 8
                )));
            }
            if !plan.blocked.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("Blocked, left in place ({}):", plan.blocked.len()),
                    Style::default().fg(Color::Yellow),
                )));
                for b in plan.blocked.iter().take(5) {
                    lines.push(Line::from(format!("  {}: {}", b.key, b.reason)));
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Move to trash? (y/N)",
                bold(),
            )));
            (" delete ", lines, Color::Red)
        }
        Modal::Delete(DeleteModal::Running { count }) => (
            " delete ",
            vec![Line::from(format!("Moving {count} sessions to trash…"))],
            Color::Red,
        ),
        Modal::Trash(TrashModal::Restore(id)) => (
            " restore ",
            vec![
                Line::from(format!("Restore trash entry {id} back to its original paths?")),
                Line::from(""),
                Line::from(Span::styled("Restore? (y/N)", bold())),
            ],
            ACCENT,
        ),
        Modal::Trash(TrashModal::EmptyOne(id)) => (
            " permanent delete ",
            vec![
                Line::from(Span::styled(
                    format!("Permanently delete trash entry {id}. This cannot be undone."),
                    Style::default().fg(Color::Red),
                )),
                Line::from(""),
                Line::from(Span::styled("Delete permanently? (y/N)", bold())),
            ],
            Color::Red,
        ),
        Modal::Trash(TrashModal::EmptyAll(typed)) => (
            " permanent delete ",
            vec![
                Line::from(Span::styled(
                    "Permanently empty the WHOLE trash. This cannot be undone.",
                    Style::default().fg(Color::Red),
                )),
                Line::from(""),
                Line::from(format!("Type yes and press Enter: {typed}▏")),
                Line::from(Span::styled("Esc cancels", dim())),
            ],
            Color::Red,
        ),
    };
    let height = (lines.len() as u16).saturating_add(2);
    let rect = centered(area, 70, height);
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines)
            .block(
                bordered(title)
                    .border_style(Style::default().fg(color)),
            )
            .wrap(Wrap { trim: false }),
        rect,
    );
    let _ = app;
}

fn help_lines() -> Vec<Line<'static>> {
    [
        "Move        ↑↓ / j k · PgUp PgDn · Home End (g G)",
        "Open        → / Enter / l · back: ← / h / Backspace (Analyze)",
        "Select      Space toggle · v mark range, v again to select it · a all in view",
        "Delete      d / Del, then y in the confirm modal (moves to trash)",
        "Preview     p toggle · o open the project folder",
        "Group       Tab or g cycles Project → Agent → Folder (Analyze)",
        "Sort        s cycles Size → Recent → Oldest → Messages → Name",
        "Filter      / regex (Enter apply, Esc clear)",
        "Screens     t trash · r rescan · Esc back to menu · q quit",
        "Trash       r restore · x empty entry · X empty all (type yes)",
        "",
        "Esc closes this help.",
    ]
    .iter()
    .map(|s| Line::from(s.to_string()))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_time_units() {
        let now = Utc::now();
        assert_eq!(rel_time_at(None, now), "-");
        assert_eq!(rel_time_at(Some(now), now), "now");
        assert_eq!(rel_time_at(Some(now - chrono::Duration::minutes(5)), now), "5m");
        assert_eq!(rel_time_at(Some(now - chrono::Duration::hours(2)), now), "2h");
        assert_eq!(rel_time_at(Some(now - chrono::Duration::days(3)), now), "3d");
        assert_eq!(rel_time_at(Some(now - chrono::Duration::days(150)), now), "5mo");
        assert_eq!(rel_time_at(Some(now - chrono::Duration::days(800)), now), "2y");
    }

    #[test]
    fn bar_fills_proportionally_and_never_empty_for_nonzero() {
        let full: String = size_bar(100, 100, 10).iter().map(|s| s.content.to_string()).collect();
        assert_eq!(full, "██████████");
        let half: String = size_bar(50, 100, 10).iter().map(|s| s.content.to_string()).collect();
        assert_eq!(half, "█████░░░░░");
        let tiny: String = size_bar(1, 1_000_000, 10).iter().map(|s| s.content.to_string()).collect();
        assert!(tiny.starts_with('█'));
        assert_eq!(tiny.chars().count(), 10);
    }

    #[test]
    fn size_colors_by_threshold() {
        assert_eq!(size_color(2 * GB), Color::Red);
        assert_eq!(size_color(200 * (1 << 20)), Color::Yellow);
        assert_eq!(size_color(1 << 20), Color::Green);
    }
}

#[cfg(test)]
mod render_tests {
    use crossterm::event::KeyCode;

    use crate::app::fixtures::{app, ch, key, sample};
    use crate::app::{render_to_string, Mode};

    #[test]
    fn list_80x24_drops_project_column_and_keeps_core_columns() {
        let root = tempfile::tempdir().unwrap();
        let a = app(Mode::List, sample(), root.path());
        let text = render_to_string(&a, 80, 24).unwrap();
        assert!(text.contains("sz  list"), "header: {text}");
        assert!(text.contains("gamma"));
        assert!(text.contains("2.0 GB"));
        assert!(text.contains("sort: size"));
        assert!(!text.contains("project"), "project column hidden under 110 cols");
        assert!(text.contains("q quit"), "footer hints fit: {text}");
    }

    #[test]
    fn list_160x40_with_preview_shows_project_and_loading_pane() {
        let root = tempfile::tempdir().unwrap();
        let mut a = app(Mode::List, sample(), root.path());
        a.on_key(ch('p'));
        let text = render_to_string(&a, 160, 40).unwrap();
        assert!(text.contains("project"));
        assert!(text.contains("/p/other"));
        assert!(text.contains(" preview "));
        assert!(text.contains("loading…"));
    }

    #[test]
    fn analyze_120x40_shows_groups_breadcrumb_and_no_project_last() {
        let root = tempfile::tempdir().unwrap();
        let a = app(Mode::Analyze, sample(), root.path());
        let text = render_to_string(&a, 120, 40).unwrap();
        assert!(text.contains("analyze · Project"));
        assert!(text.contains("▸ /p/other"));
        let other = text.find("/p/other").unwrap();
        let none = text.find("(no project)").unwrap();
        assert!(other < none, "(no project) renders after the named projects");
    }

    #[test]
    fn analyze_breadcrumb_follows_drill_down() {
        let root = tempfile::tempdir().unwrap();
        let mut a = app(Mode::Analyze, sample(), root.path());
        a.on_key(key(KeyCode::Enter));
        let text = render_to_string(&a, 120, 40).unwrap();
        assert!(text.contains("analyze · Project › /p/other"), "{text}");
    }

    #[test]
    fn menu_80x24_lists_screens_and_totals() {
        let root = tempfile::tempdir().unwrap();
        let a = app(Mode::Menu, sample(), root.path());
        let text = render_to_string(&a, 80, 24).unwrap();
        for word in ["Analyze", "List", "Trash", "Agents", "Quit", "totals by agent", "codex"] {
            assert!(text.contains(word), "missing {word}: {text}");
        }
    }

    #[test]
    fn empty_trash_and_empty_list_have_messages() {
        let root = tempfile::tempdir().unwrap();
        let t = app(Mode::Trash, Vec::new(), root.path());
        assert!(render_to_string(&t, 80, 24).unwrap().contains("Trash is empty."));
        let l = app(Mode::List, Vec::new(), root.path());
        assert!(render_to_string(&l, 80, 24)
            .unwrap()
            .contains("No sessions found. Press r to rescan."));
    }

    #[test]
    fn delete_confirm_modal_asks_before_moving() {
        let root = tempfile::tempdir().unwrap();
        let mut a = app(Mode::List, sample(), root.path());
        a.on_key(ch('d'));
        let checking = render_to_string(&a, 120, 40).unwrap();
        assert!(checking.contains("Checking 1 selected sessions"), "{checking}");
        assert!(!checking.contains("Move to trash? (y/N)"));

        a.dispatch_pending();
        let start = std::time::Instant::now();
        while !matches!(
            a.modal,
            Some(crate::app::Modal::Delete(crate::app::DeleteModal::Confirm { .. }))
        ) {
            a.poll_background();
            assert!(start.elapsed() < std::time::Duration::from_secs(10), "plan timed out");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let text = render_to_string(&a, 120, 40).unwrap();
        assert!(text.contains("Move 1 sessions"), "{text}");
        assert!(text.contains("Move to trash? (y/N)"));
    }

    #[test]
    fn help_overlay_documents_keys() {
        let root = tempfile::tempdir().unwrap();
        let mut a = app(Mode::List, sample(), root.path());
        a.on_key(ch('?'));
        let text = render_to_string(&a, 120, 40).unwrap();
        for word in ["Select", "Delete", "Filter", "Sort"] {
            assert!(text.contains(word), "missing {word}");
        }
    }
}
