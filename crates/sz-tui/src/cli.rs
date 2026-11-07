//! Non-interactive command handlers for `sz`.
//!
//! Pure helpers (rule building, table and plan rendering, list selection) are unit-tested in
//! this file. The handlers do the I/O and are exercised end to end by `tests/cli.rs`.

use std::fmt::Write as _;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Local, Utc};
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use sz_core::adapters::all_adapters;
use sz_core::cache;
use sz_core::config::{self, Config};
use sz_core::group::{self, Group, GroupBy, SortBy};
use sz_core::model::{AgentKind, Session};
use sz_core::rules::{self, Rule};
use sz_core::scan;
use sz_core::trash::{self, DeletePlan, Trash, TrashEntry};
use sz_core::util::truncate;

/// Rows shown in plan listings and project tables.
const TOP: usize = 20;
/// Messages shown by `preview`.
const PREVIEW_MESSAGES: usize = 20;
const SESSION_HEADERS: [&str; 6] = ["AGENT", "SIZE", "LAST ACTIVE", "MSGS", "PROJECT", "TITLE"];

/// Settings shared by every command, built once from the global flags.
pub struct Ctx {
    pub home: PathBuf,
    pub json: bool,
    pub force: bool,
    /// Empty means every agent.
    pub agents: Vec<AgentKind>,
}

/// Flags of `sz clean`.
pub struct CleanOpts<'a> {
    pub older: Option<&'a str>,
    pub larger: Option<&'a str>,
    pub project: Option<&'a str>,
    pub no_project: bool,
    pub dry_run: bool,
    pub yes: bool,
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Parses repeated `--agent` values. Unknown names are an error listing the known ones.
pub fn parse_agents(names: &[String]) -> Result<Vec<AgentKind>> {
    names
        .iter()
        .map(|n| {
            rules::parse_agent(n)
                .ok_or_else(|| anyhow!("unknown agent '{n}'; known agents: {}", known_agents()))
        })
        .collect()
}

fn known_agents() -> String {
    AgentKind::all()
        .iter()
        .map(|a| a.label())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Builds the cleanup rule from `clean` flags. The caller refuses an empty rule.
pub fn build_rule(
    older: Option<&str>,
    larger: Option<&str>,
    project: Option<&str>,
    no_project: bool,
    agents: &[AgentKind],
) -> Result<Rule> {
    if project.is_some() && no_project {
        bail!("--project and --no-project cannot be used together");
    }
    let older_than = older.map(rules::parse_duration).transpose()?;
    let larger_than = larger.map(rules::parse_size).transpose()?;
    if let Some(p) = project {
        Regex::new(p).map_err(|e| anyhow!("invalid --project regex '{p}': {e}"))?;
    }
    Ok(Rule {
        older_than,
        larger_than,
        agents: agents.to_vec(),
        project: project.map(str::to_owned),
        no_project,
    })
}

/// Maps `--sort` values to a sort order.
pub fn parse_sort(s: &str) -> Result<SortBy> {
    Ok(match s.trim().to_ascii_lowercase().as_str() {
        "size" => SortBy::Size,
        "recent" => SortBy::Recent,
        "oldest" => SortBy::Oldest,
        "messages" => SortBy::Messages,
        "name" => SortBy::Name,
        other => bail!("unknown sort '{other}': use size, recent, oldest, messages or name"),
    })
}

/// Indices of the sessions `sz list` shows: filtered, sorted, then limited.
pub fn list_view(
    sessions: &[Session],
    sort: SortBy,
    filter: Option<&str>,
    limit: Option<usize>,
) -> Vec<usize> {
    let mut idx: Vec<usize> = match filter {
        Some(p) => group::filter(sessions, p),
        None => (0..sessions.len()).collect(),
    };
    group::sort_indices(sessions, &mut idx, sort);
    if let Some(n) = limit {
        idx.truncate(n);
    }
    idx
}

/// Left-aligned table. Every column but the last is padded to its widest cell.
pub fn format_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let header_cells: Vec<String> = headers.iter().map(|h| (*h).to_string()).collect();
    out.push_str(&render_row(&header_cells, &widths));
    for row in rows {
        out.push_str(&render_row(row, &widths));
    }
    out
}

fn render_row(cells: &[String], widths: &[usize]) -> String {
    let last = widths.len().saturating_sub(1);
    let mut line = String::new();
    for (i, width) in widths.iter().enumerate() {
        let cell = cells.get(i).map(String::as_str).unwrap_or("");
        if i == last {
            line.push_str(cell);
        } else {
            let _ = write!(line, "{cell:<width$}  ", width = *width);
        }
    }
    format!("{}\n", line.trim_end())
}

/// Local-time `YYYY-MM-DD HH:MM`, or `-` when unknown.
pub fn fmt_time(t: Option<DateTime<Utc>>) -> String {
    t.map(|t| t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn session_row(s: &Session) -> Vec<String> {
    vec![
        s.agent.label().to_string(),
        group::human_bytes(s.bytes),
        fmt_time(s.last_active),
        s.message_count
            .map(|m| m.to_string())
            .unwrap_or_else(|| "-".to_string()),
        s.project
            .as_deref()
            .map(group::display_path)
            .unwrap_or_else(|| "-".to_string()),
        truncate(s.title.as_deref().unwrap_or("-"), 50),
    ]
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// Human-readable plan: totals, the largest sessions, then anything blocked and why.
/// Human-readable plan: totals, the largest sessions, then anything blocked and why. `children`
/// is how many planned sessions were added because their parent was picked.
pub fn render_plan_with(plan: &DeletePlan, children: usize) -> String {
    let mut out = String::new();
    let n = plan.sessions.len();
    if n == 0 {
        out.push_str("Nothing to move to trash.\n");
    } else {
        let children_note = if children > 0 {
            format!("; +{}", plural(children, "child session"))
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "Plan: move {} to trash ({}{children_note}).",
            plural(n, "session"),
            group::human_bytes(plan.bytes)
        );
        let mut idx: Vec<usize> = (0..n).collect();
        idx.sort_by(|&a, &b| plan.sessions[b].bytes.cmp(&plan.sessions[a].bytes));
        let rows: Vec<Vec<String>> = idx
            .iter()
            .take(TOP)
            .map(|&i| session_row(&plan.sessions[i]))
            .collect();
        out.push_str(&format_table(&SESSION_HEADERS, &rows));
        if n > TOP {
            let _ = writeln!(out, "... and {} more", n - TOP);
        }
    }
    if !plan.blocked.is_empty() {
        let _ = writeln!(out, "Blocked: {}", plural(plan.blocked.len(), "session"));
        for b in &plan.blocked {
            let _ = writeln!(out, "  {}: {}", b.key, b.reason);
        }
    }
    out
}

fn session_json(s: &Session) -> Value {
    json!({
        "agent": s.agent.label(),
        "id": s.id,
        "key": s.key(),
        "title": s.title,
        "project": s.project.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "created": s.created,
        "last_active": s.last_active,
        "message_count": s.message_count,
        "bytes": s.bytes,
    })
}

fn plan_json(plan: &DeletePlan) -> Value {
    json!({
        "sessions": plan.sessions.iter().map(session_json).collect::<Vec<_>>(),
        "count": plan.sessions.len(),
        "bytes": plan.bytes,
        "blocked": plan
            .blocked
            .iter()
            .map(|b| json!({"key": b.key, "reason": b.reason}))
            .collect::<Vec<_>>(),
    })
}

fn group_json(g: &Group, sessions: &[Session]) -> Value {
    json!({
        "label": g.label,
        "key": g.key,
        "bytes": g.bytes,
        "count": g.count,
        "last_active": g.last_active,
        "children": g.children.iter().map(|c| group_json(c, sessions)).collect::<Vec<_>>(),
        "sessions": g.sessions.iter().map(|&i| sessions[i].key()).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// I/O helpers
// ---------------------------------------------------------------------------

fn refuse(msg: &str) -> ExitCode {
    eprintln!("sz: refused: {msg}");
    ExitCode::from(2)
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Scans every agent (or the `--agent` subset). A full scan refreshes the cache.
fn scan(ctx: &Ctx) -> Vec<Session> {
    let only = (!ctx.agents.is_empty()).then_some(ctx.agents.as_slice());
    let sessions = scan::scan_all(&ctx.home, only);
    if only.is_none() {
        if let Err(e) = cache::save(&sessions) {
            eprintln!("sz: warning: could not save scan cache: {e:#}");
        }
    }
    sessions
}

fn read_line() -> Result<String> {
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn ask(question: &str) -> Result<String> {
    print!("{question} ");
    io::stdout().flush()?;
    read_line()
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// `sz analyze --json`: the project tree with session keys on the leaves.
pub fn analyze_json(ctx: &Ctx) -> Result<ExitCode> {
    let sessions = scan(ctx);
    let tree = group::group(&sessions, GroupBy::Project, SortBy::Size);
    let out: Vec<Value> = tree.iter().map(|g| group_json(g, &sessions)).collect();
    print_json(&out)?;
    Ok(ExitCode::SUCCESS)
}

/// `sz list` as a table or JSON array.
pub fn list(ctx: &Ctx, sort: &str, limit: Option<usize>, filter: Option<&str>) -> Result<ExitCode> {
    let sort = parse_sort(sort)?;
    let sessions = scan(ctx);
    let idx = list_view(&sessions, sort, filter, limit);
    if ctx.json {
        let rows: Vec<Value> = idx.iter().map(|&i| session_json(&sessions[i])).collect();
        print_json(&rows)?;
    } else if idx.is_empty() {
        println!("No sessions found.");
    } else {
        let rows: Vec<Vec<String>> = idx.iter().map(|&i| session_row(&sessions[i])).collect();
        print!("{}", format_table(&SESSION_HEADERS, &rows));
    }
    Ok(ExitCode::SUCCESS)
}

/// `sz agents`: detection, running state, session counts and data roots per agent.
pub fn agents(ctx: &Ctx) -> Result<ExitCode> {
    let infos = scan::agent_infos(&ctx.home);
    let sessions = scan(ctx);
    let infos: Vec<_> = infos
        .into_iter()
        .filter(|i| ctx.agents.is_empty() || ctx.agents.contains(&i.kind))
        .collect();
    if ctx.json {
        let out: Vec<Value> = infos
            .iter()
            .map(|i| {
                json!({
                    "agent": i.kind.label(),
                    "detected": i.detected,
                    "running": i.running,
                    "sessions": sessions_of(&sessions, i.kind).count(),
                    "bytes": sessions_of(&sessions, i.kind).map(|s| s.bytes).sum::<u64>(),
                    "roots": i.roots.iter().map(|(p, exists)| json!({
                        "path": p.to_string_lossy(),
                        "exists": exists,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        print_json(&out)?;
        return Ok(ExitCode::SUCCESS);
    }
    let rows: Vec<Vec<String>> = infos
        .iter()
        .map(|i| {
            let roots: Vec<String> = i
                .roots
                .iter()
                .map(|(p, exists)| {
                    let shown = group::display_path(p);
                    if *exists {
                        shown
                    } else {
                        format!("{shown} (missing)")
                    }
                })
                .collect();
            vec![
                i.kind.label().to_string(),
                yes_no(i.detected),
                yes_no(i.running),
                sessions_of(&sessions, i.kind).count().to_string(),
                group::human_bytes(sessions_of(&sessions, i.kind).map(|s| s.bytes).sum()),
                if roots.is_empty() {
                    "-".to_string()
                } else {
                    roots.join(", ")
                },
            ]
        })
        .collect();
    print!(
        "{}",
        format_table(
            &[
                "AGENT",
                "DETECTED",
                "RUNNING",
                "SESSIONS",
                "SIZE",
                "DATA ROOTS"
            ],
            &rows
        )
    );
    Ok(ExitCode::SUCCESS)
}

fn sessions_of(sessions: &[Session], kind: AgentKind) -> impl Iterator<Item = &Session> + Clone {
    sessions.iter().filter(move |s| s.agent == kind)
}

fn yes_no(b: bool) -> String {
    if b { "yes" } else { "no" }.to_string()
}

/// `sz du`: totals per agent and the largest projects.
pub fn du(ctx: &Ctx) -> Result<ExitCode> {
    let sessions = scan(ctx);
    let per_agent: Vec<(AgentKind, usize, u64)> = AgentKind::all()
        .iter()
        .filter(|k| ctx.agents.is_empty() || ctx.agents.contains(k))
        .map(|&k| {
            let mine = sessions_of(&sessions, k);
            (k, mine.clone().count(), mine.map(|s| s.bytes).sum())
        })
        .filter(|(_, count, _)| *count > 0)
        .collect();
    let projects: Vec<Group> = group::group(&sessions, GroupBy::Project, SortBy::Size)
        .into_iter()
        .take(TOP)
        .collect();

    if ctx.json {
        print_json(&json!({
            "agents": per_agent.iter().map(|(k, c, b)| json!({
                "agent": k.label(), "sessions": c, "bytes": b,
            })).collect::<Vec<_>>(),
            "projects": projects.iter().map(|g| json!({
                "project": g.label, "sessions": g.count, "bytes": g.bytes,
            })).collect::<Vec<_>>(),
        }))?;
        return Ok(ExitCode::SUCCESS);
    }

    let agent_rows: Vec<Vec<String>> = per_agent
        .iter()
        .map(|(k, c, b)| vec![k.label().to_string(), c.to_string(), group::human_bytes(*b)])
        .collect();
    print!(
        "{}",
        format_table(&["AGENT", "SESSIONS", "SIZE"], &agent_rows)
    );
    println!();
    let project_rows: Vec<Vec<String>> = projects
        .iter()
        .map(|g| {
            vec![
                g.label.clone(),
                g.count.to_string(),
                group::human_bytes(g.bytes),
            ]
        })
        .collect();
    print!(
        "{}",
        format_table(&["PROJECT", "SESSIONS", "SIZE"], &project_rows)
    );
    Ok(ExitCode::SUCCESS)
}

/// `sz clean`: plan the deletion, then either stop (dry run), confirm and move to trash.
pub fn clean(ctx: &Ctx, o: &CleanOpts) -> Result<ExitCode> {
    let rule = build_rule(o.older, o.larger, o.project, o.no_project, &ctx.agents)?;
    if rule.is_empty() {
        return Ok(refuse(
            "no rule given; pass at least one of --older, --larger, --agent, --project or --no-project",
        ));
    }
    let now = Utc::now();
    let sessions = scan(ctx);
    let picked: Vec<Session> = rules::select(&sessions, &rule, now)
        .into_iter()
        .map(|i| sessions[i].clone())
        .collect();
    // A parent is never trashed while its children stay behind.
    let picked_n = picked.len();
    let with_kids = trash::with_children(&sessions, picked);
    let children = with_kids.len().saturating_sub(picked_n);
    let plan = trash::plan(&ctx.home, with_kids, ctx.force);

    if plan.sessions.is_empty() {
        emit_clean(ctx, &plan, children, o.dry_run, None)?;
        return Ok(if plan.blocked.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(2)
        });
    }
    if o.dry_run {
        emit_clean(ctx, &plan, children, true, None)?;
        return Ok(ExitCode::SUCCESS);
    }

    if !o.yes {
        if ctx.json || !io::stdin().is_terminal() {
            emit_clean(ctx, &plan, children, false, None)?;
            return Ok(refuse(
                "confirmation needed; re-run with --yes to move these sessions",
            ));
        }
        if !ctx.json {
            print!("{}", render_plan_with(&plan, children));
        }
        let answer = ask(&format!(
            "Move {} ({}) to trash? [y/N]",
            plural(plan.sessions.len(), "session"),
            group::human_bytes(plan.bytes)
        ))?;
        if !matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Aborted. Nothing was moved.");
            return Ok(ExitCode::SUCCESS);
        }
    }

    let entry = Trash::default_location().execute(&plan)?;
    let code = if entry.errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    };
    emit_clean(ctx, &plan, children, false, Some(&entry))?;
    Ok(code)
}

fn emit_clean(
    ctx: &Ctx,
    plan: &DeletePlan,
    children: usize,
    dry_run: bool,
    entry: Option<&TrashEntry>,
) -> Result<()> {
    if ctx.json {
        return print_json(&json!({
            "dry_run": dry_run,
            "children": children,
            "plan": plan_json(plan),
            "trashed": entry,
        }));
    }
    print!("{}", render_plan_with(plan, children));
    if dry_run && !plan.sessions.is_empty() {
        println!("Dry run: nothing was moved.");
    }
    if let Some(e) = entry {
        println!(
            "Moved {} ({}) to trash. Entry: {}",
            plural(e.sessions.len(), "session"),
            group::human_bytes(e.bytes),
            e.id
        );
        for err in &e.errors {
            println!("  error: {err}");
        }
        println!("Restore with: sz trash restore {}", e.id);
    }
    Ok(())
}

/// `sz trash list`.
pub fn trash_list(ctx: &Ctx) -> Result<ExitCode> {
    let entries = Trash::default_location().list()?;
    let config = load_config_or_default();
    if ctx.json {
        print_json(&entries)?;
        return Ok(ExitCode::SUCCESS);
    }
    if entries.is_empty() {
        println!("Trash is empty.");
        return Ok(ExitCode::SUCCESS);
    }
    let now = Utc::now();
    let rows: Vec<Vec<String>> = entries
        .iter()
        .map(|e| {
            vec![
                e.id.clone(),
                fmt_time(Some(e.created)),
                plural(e.sessions.len(), "session"),
                group::human_bytes(e.bytes),
                config.auto_delete_text(e.created, now),
            ]
        })
        .collect();
    print!(
        "{}",
        format_table(
            &["ID", "TRASHED", "SESSIONS", "SIZE", "AUTO-DELETE"],
            &rows
        )
    );
    Ok(ExitCode::SUCCESS)
}

/// Reads the config. A broken file prints a warning and falls back to the defaults, so listing
/// the trash never fails because of settings.
fn load_config_or_default() -> Config {
    Config::load().unwrap_or_else(|e| {
        eprintln!("sz: warning: using default settings: {e:#}");
        Config::default()
    })
}

/// Startup pass of automatic trash deletion for CLI commands. Prints one line to stderr when
/// something was removed. Skipped when `SZ_NO_AUTO_PURGE` is set. A config error skips the pass
/// rather than deleting with settings it could not read.
pub fn auto_purge_cli() {
    if config::auto_purge_disabled() {
        return;
    }
    let config = match Config::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("sz: warning: automatic trash deletion skipped: {e:#}");
            return;
        }
    };
    match config::auto_purge(&Trash::default_location(), &config, Utc::now()) {
        Ok(report) if report.entries > 0 => eprintln!(
            "Auto-deleted {} ({}) older than {}. Change with: sz config trash-retention <days|never>",
            entries_text(report.entries),
            group::human_bytes(report.bytes),
            plural_days(config.trash_retention_days),
        ),
        Ok(_) => {}
        Err(e) => eprintln!("sz: warning: automatic trash deletion failed: {e:#}"),
    }
}

/// `1 trash entry` or `N trash entries`.
fn entries_text(n: usize) -> String {
    if n == 1 {
        "1 trash entry".to_string()
    } else {
        format!("{n} trash entries")
    }
}

fn plural_days(n: u32) -> String {
    if n == 1 {
        "1 day".to_string()
    } else {
        format!("{n} days")
    }
}

/// `sz config`: prints every setting and where it is stored.
pub fn config_show(ctx: &Ctx) -> Result<ExitCode> {
    let config = Config::load()?;
    if ctx.json {
        print_json(&json!({
            "trash_retention_days": config.trash_retention_days,
        }))?;
    } else {
        println!("trash-retention: {}", config.retention_text());
        println!("  trashed sessions are deleted automatically after this long (0 or never: kept)");
        println!("  file: {}", Config::path().display());
    }
    Ok(ExitCode::SUCCESS)
}

/// `sz config trash-retention <days|never>`.
pub fn config_trash_retention(ctx: &Ctx, value: &str) -> Result<ExitCode> {
    let days = config::parse_retention(value)?;
    let config = Config {
        trash_retention_days: days,
    };
    config.save()?;
    if ctx.json {
        print_json(&json!({ "trash_retention_days": days }))?;
    } else if days == 0 {
        println!("Automatic deletion is off. Trashed sessions are kept until you empty the trash.");
    } else {
        println!(
            "Trashed sessions are deleted automatically after {}.",
            plural_days(days)
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `sz trash restore <id>`.
pub fn trash_restore(ctx: &Ctx, id: &str) -> Result<ExitCode> {
    let trash = Trash::default_location();
    let summary = trash.list()?.into_iter().find(|e| e.id == id).map(|e| {
        (
            plural(e.sessions.len(), "session"),
            group::human_bytes(e.bytes),
        )
    });
    trash.restore(id)?;
    if ctx.json {
        print_json(&json!({"restored": id}))?;
    } else if let Some((sessions, bytes)) = summary {
        println!("Restored {id}: {sessions} ({bytes}).");
    } else {
        println!("Restored {id}.");
    }
    Ok(ExitCode::SUCCESS)
}

/// `sz trash empty [<id>] [--yes]`. Permanent, so it asks for the word `yes`.
pub fn trash_empty(
    ctx: &Ctx,
    id: Option<&str>,
    older: Option<&str>,
    yes: bool,
) -> Result<ExitCode> {
    let age = older.map(rules::parse_duration).transpose()?;
    if id.is_some() && age.is_some() {
        bail!("an entry id and --older cannot be used together");
    }
    if !yes {
        let target = match (id, older) {
            (Some(i), _) => format!("trash entry {i}"),
            (None, Some(d)) => format!("every trash entry older than {d}"),
            (None, None) => "every trash entry".to_string(),
        };
        eprintln!("This permanently deletes {target}. It cannot be undone.");
        if ctx.json || !io::stdin().is_terminal() {
            return Ok(refuse("confirmation needed; re-run with --yes"));
        }
        if ask("Type 'yes' to continue:")? != "yes" {
            println!("Aborted. Nothing was deleted.");
            return Ok(ExitCode::SUCCESS);
        }
    }
    let trash = Trash::default_location();
    let (removed, freed) = match age {
        Some(age) => {
            let report = trash.purge_expired(age, Utc::now())?;
            (Some(report.entries), report.bytes)
        }
        None => (None, trash.empty(id)?),
    };
    if ctx.json {
        let mut out = json!({ "freed_bytes": freed });
        if let Some(n) = removed {
            out["removed_entries"] = json!(n);
        }
        print_json(&out)?;
    } else if let Some(n) = removed {
        println!("Deleted {}, freed {}.", entries_text(n), group::human_bytes(freed));
    } else {
        println!("Freed {}.", group::human_bytes(freed));
    }
    Ok(ExitCode::SUCCESS)
}

/// `sz preview <agent:id>`: metadata and the first messages of one session.
pub fn preview(ctx: &Ctx, target: &str) -> Result<ExitCode> {
    let (agent_name, id) = target
        .split_once(':')
        .ok_or_else(|| anyhow!("expected <agent:id>, for example claude:5f1c0a02-..."))?;
    let agent = rules::parse_agent(agent_name).ok_or_else(|| {
        anyhow!(
            "unknown agent '{agent_name}'; known agents: {}",
            known_agents()
        )
    })?;
    let sessions = scan::scan_all(&ctx.home, Some(&[agent]));
    let session = sessions
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| anyhow!("no {} session with id '{id}'", agent.label()))?;
    let adapter = all_adapters(&ctx.home)
        .into_iter()
        .find(|a| a.kind() == agent)
        .ok_or_else(|| anyhow!("no adapter for {}", agent.label()))?;
    let messages = adapter.preview(session, PREVIEW_MESSAGES)?;

    if ctx.json {
        return print_json(&json!({
            "session": session_json(session),
            "messages": messages,
        }))
        .map(|()| ExitCode::SUCCESS);
    }
    println!("{}", session.key());
    println!("  title:    {}", session.title.as_deref().unwrap_or("-"));
    println!(
        "  project:  {}",
        session
            .project
            .as_deref()
            .map(group::display_path)
            .unwrap_or_else(|| "-".into())
    );
    println!("  created:  {}", fmt_time(session.created));
    println!("  active:   {}", fmt_time(session.last_active));
    println!(
        "  messages: {}",
        session
            .message_count
            .map_or_else(|| "-".to_string(), |m| m.to_string())
    );
    println!("  size:     {}", group::human_bytes(session.bytes));
    println!();
    for m in &messages {
        let text = truncate(&m.text.replace('\n', " "), 200);
        println!("[{}] {text}", m.role);
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use sz_core::trash::{Blocked, DeletePlan};

    fn sess(agent: AgentKind, id: &str, bytes: u64, project: Option<&str>) -> Session {
        Session {
            title: Some(format!("title {id}")),
            project: project.map(PathBuf::from),
            message_count: Some(3),
            bytes,
            ..Session::new(agent, id)
        }
    }

    #[test]
    fn build_rule_parses_duration_size_and_agents() {
        let rule = build_rule(Some("30d"), Some("10MB"), None, false, &[AgentKind::Codex])
            .expect("valid rule");
        assert_eq!(rule.older_than, Some(Duration::days(30)));
        assert_eq!(rule.larger_than, Some(10 * 1024 * 1024));
        assert_eq!(rule.agents, vec![AgentKind::Codex]);
        assert!(!rule.is_empty());
    }

    #[test]
    fn build_rule_without_flags_is_empty() {
        let rule = build_rule(None, None, None, false, &[]).expect("valid rule");
        assert!(rule.is_empty());
    }

    #[test]
    fn build_rule_rejects_project_with_no_project() {
        let err = build_rule(None, None, Some("alpha"), true, &[]).unwrap_err();
        assert!(err.to_string().contains("cannot be used together"));
    }

    #[test]
    fn build_rule_rejects_bad_regex_and_bad_units() {
        assert!(build_rule(None, None, Some("("), false, &[]).is_err());
        assert!(build_rule(Some("3x"), None, None, false, &[]).is_err());
        assert!(build_rule(None, Some("9QB"), None, false, &[]).is_err());
    }

    #[test]
    fn parse_agents_accepts_aliases_and_rejects_unknown() {
        let ok = parse_agents(&["kiro-cli".into(), "antigravity".into()]).expect("aliases");
        assert_eq!(ok, vec![AgentKind::KiroCli, AgentKind::Gemini]);
        let err = parse_agents(&["nope".into()]).unwrap_err();
        assert!(err.to_string().contains("unknown agent 'nope'"));
    }

    #[test]
    fn parse_sort_maps_names() {
        assert_eq!(parse_sort("Size").expect("size"), SortBy::Size);
        assert_eq!(parse_sort("messages").expect("messages"), SortBy::Messages);
        assert!(parse_sort("bogus").is_err());
    }

    #[test]
    fn list_view_filters_sorts_and_limits() {
        let sessions = vec![
            sess(AgentKind::Codex, "a", 100, Some("/work/alpha")),
            sess(AgentKind::Claude, "b", 300, Some("/work/beta")),
            sess(AgentKind::Codex, "c", 200, Some("/work/alpha")),
        ];
        let all = list_view(&sessions, SortBy::Size, None, None);
        assert_eq!(all, vec![1, 2, 0]);
        let limited = list_view(&sessions, SortBy::Size, None, Some(2));
        assert_eq!(limited, vec![1, 2]);
        let alpha = list_view(&sessions, SortBy::Size, Some("alpha"), None);
        assert_eq!(alpha, vec![2, 0]);
    }

    #[test]
    fn format_table_aligns_columns() {
        let rows = vec![vec!["xxxxx".to_string(), "y".to_string()]];
        let table = format_table(&["A", "BBBB"], &rows);
        assert_eq!(table, "A      BBBB\nxxxxx  y\n");
    }

    #[test]
    fn render_plan_lists_sessions_and_blocked_reasons() {
        let plan = DeletePlan {
            sessions: vec![sess(AgentKind::Claude, "one", 2048, Some("/work/x"))],
            bytes: 2048,
            blocked: vec![Blocked {
                key: "codex:bad".into(),
                reason: "agent is running".into(),
            }],
        };
        let text = render_plan_with(&plan, 0);
        assert!(text.contains("Plan: move 1 session to trash"));
        assert!(text.contains("claude"));
        assert!(text.contains("Blocked: 1 session"));
        assert!(text.contains("codex:bad: agent is running"));
    }

    #[test]
    fn render_plan_reports_nothing_to_move() {
        let plan = DeletePlan {
            sessions: vec![],
            bytes: 0,
            blocked: vec![],
        };
        assert_eq!(render_plan_with(&plan, 0), "Nothing to move to trash.\n");
    }

    #[test]
    fn render_plan_notes_child_sessions() {
        let plan = DeletePlan {
            sessions: vec![
                sess(AgentKind::Codex, "parent", 1024, Some("/work/x")),
                sess(AgentKind::Codex, "kid", 1024, Some("/work/x")),
            ],
            bytes: 2048,
            blocked: vec![],
        };
        let text = render_plan_with(&plan, 1);
        assert!(text.contains("Plan: move 2 sessions to trash (2.0 KB; +1 child session)."));
    }

    #[test]
    fn plural_and_fmt_time_edge_cases() {
        assert_eq!(plural(1, "session"), "1 session");
        assert_eq!(plural(0, "session"), "0 sessions");
        assert_eq!(fmt_time(None), "-");
    }
}
