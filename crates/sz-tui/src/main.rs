mod app;
mod cli;
mod ui;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use crate::cli::Ctx;

#[derive(Parser)]
#[command(name = "sz", version, about = "List, size, group and safely delete AI-agent session files")]
struct Cli {
    /// Emit machine-readable JSON output.
    #[arg(long, global = true)]
    json: bool,

    /// Use this directory as the home directory instead of the real one (fixtures, testing).
    #[arg(long, global = true, value_name = "PATH")]
    home: Option<PathBuf>,

    /// Limit to this agent. Repeat for several. Also the agent rule for `clean`.
    #[arg(long = "agent", global = true, value_name = "NAME")]
    agents: Vec<String>,

    /// Act on sessions that are live or unsafe to touch (skips the running-agent guard).
    #[arg(long, global = true)]
    force: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Report disk usage per project and age (interactive, or a JSON tree with --json).
    Analyze,
    /// List sessions.
    List {
        /// Sort order: size, recent, oldest, messages or name.
        #[arg(long, value_name = "KEY", default_value = "size")]
        sort: String,
        /// Show at most this many sessions.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// Only sessions whose title, project, agent or id matches this regex.
        #[arg(long, value_name = "REGEX")]
        filter: Option<String>,
    },
    /// Move sessions matching rules to the trash.
    Clean {
        /// Only sessions older than this duration (e.g. 30d, 12h).
        #[arg(long, value_name = "DUR")]
        older: Option<String>,
        /// Only sessions larger than this size (e.g. 10MB).
        #[arg(long, value_name = "SIZE")]
        larger: Option<String>,
        /// Only sessions whose project path matches this regex.
        #[arg(long, value_name = "REGEX")]
        project: Option<String>,
        /// Only sessions with no project.
        #[arg(long)]
        no_project: bool,
        /// Show the plan without moving anything.
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Manage the trash of deleted sessions. With no subcommand, opens the trash view.
    Trash {
        #[command(subcommand)]
        action: Option<TrashAction>,
    },
    /// Show settings, or change one, e.g. `sz config trash-retention 7`.
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Show detected agents, session counts and data roots.
    Agents,
    /// Show disk usage per agent and per project.
    Du,
    /// Print metadata and the first messages of one session (agent:id).
    Preview {
        /// Session key, e.g. claude:5f1c0a02-2b3c-4d5e-9f01-000000000002.
        target: String,
    },
}

#[derive(Subcommand)]
enum TrashAction {
    /// List trashed sessions, with when each one is auto-deleted.
    List,
    /// Restore a trashed entry by id.
    Restore { id: String },
    /// Permanently delete trash entries (all of them when no id or --older is given).
    Empty {
        /// Entry to delete. Omit to empty the whole trash.
        id: Option<String>,
        /// Only entries trashed longer ago than this duration (e.g. 7d, 12h).
        #[arg(long, value_name = "DUR", conflicts_with = "id")]
        older: Option<String>,
        /// Skip the typed confirmation.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Days a trashed session is kept before it is deleted automatically: a number of days
    /// (7 or 7d, default 30), or `never`.
    TrashRetention { value: String },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("sz: error: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let home = match cli.home.clone().or_else(dirs::home_dir) {
        Some(home) => home,
        None => bail!("could not determine home directory"),
    };
    let ctx = Ctx {
        home,
        json: cli.json,
        force: cli.force,
        agents: cli::parse_agents(&cli.agents)?,
    };

    // Automatic trash deletion runs on every command except these. The TUI runs it on a
    // background thread instead of before the first frame.
    let auto_purge = !matches!(
        cli.command,
        Some(Command::Config { .. })
            | Some(Command::Trash {
                action: Some(TrashAction::List | TrashAction::Restore { .. }),
            })
    );
    let opens_tui = match &cli.command {
        None | Some(Command::Trash { action: None }) => true,
        Some(Command::Analyze) => !ctx.json,
        Some(Command::List { .. }) => !ctx.json && std::io::stdout().is_terminal(),
        _ => false,
    };
    if auto_purge && !opens_tui {
        cli::auto_purge_cli();
    }
    let background_purge = auto_purge && opens_tui;

    match cli.command {
        None => tui(&ctx, app::Mode::Menu, background_purge),
        Some(Command::Analyze) => {
            if ctx.json {
                cli::analyze_json(&ctx)
            } else {
                tui(&ctx, app::Mode::Analyze, background_purge)
            }
        }
        Some(Command::List {
            sort,
            limit,
            filter,
        }) => {
            if opens_tui {
                tui(&ctx, app::Mode::List, background_purge)
            } else {
                cli::list(&ctx, &sort, limit, filter.as_deref())
            }
        }
        Some(Command::Clean {
            older,
            larger,
            project,
            no_project,
            dry_run,
            yes,
        }) => cli::clean(
            &ctx,
            &cli::CleanOpts {
                older: older.as_deref(),
                larger: larger.as_deref(),
                project: project.as_deref(),
                no_project,
                dry_run,
                yes,
            },
        ),
        Some(Command::Trash { action: None }) => tui(&ctx, app::Mode::Trash, background_purge),
        Some(Command::Trash {
            action: Some(TrashAction::List),
        }) => cli::trash_list(&ctx),
        Some(Command::Trash {
            action: Some(TrashAction::Restore { id }),
        }) => cli::trash_restore(&ctx, &id),
        Some(Command::Trash {
            action: Some(TrashAction::Empty { id, older, yes }),
        }) => cli::trash_empty(&ctx, id.as_deref(), older.as_deref(), yes),
        Some(Command::Config { action: None }) => cli::config_show(&ctx),
        Some(Command::Config {
            action: Some(ConfigAction::TrashRetention { value }),
        }) => cli::config_trash_retention(&ctx, &value),
        Some(Command::Agents) => cli::agents(&ctx),
        Some(Command::Du) => cli::du(&ctx),
        Some(Command::Preview { target }) => cli::preview(&ctx, &target),
    }
}

/// Hands the terminal to the interactive app in the given mode. `background_purge` starts the
/// automatic trash deletion on a worker thread once the app is up.
fn tui(ctx: &Ctx, mode: app::Mode, background_purge: bool) -> Result<ExitCode> {
    let opts = app::Options {
        home: ctx.home.clone(),
        force: ctx.force,
        agents: (!ctx.agents.is_empty()).then(|| ctx.agents.clone()),
    };
    app::run(opts, mode, background_purge)?;
    Ok(ExitCode::SUCCESS)
}
