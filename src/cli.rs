//! Argument parsing and the thin dispatch that wires real I/O to the
//! orchestration in [`crate::command`].
//!
//! Stream discipline: the `status` report is the command's *answer* and goes
//! to **stdout**; progress (the streamed sync/reset trail), summaries,
//! warnings and hints are status and go to **stderr** so they never pollute a
//! piped result.

use crate::command::{self, RealCommandRunner};
use crate::config::{self, Config};
use crate::output::{self, StderrReporter};
use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use std::time::Instant;

#[derive(Parser)]
#[command(
    name = "skillctl",
    about = "skillctl — for when it's a skill issue.",
    long_about = "skillctl — for when it's a skill issue.\n\n\
                  Point Claude & Codex at a marketplace worktree, then reset \
                  them back.",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Detect this repo and write `~/.config/skillctl/config.toml`.
    Init {
        /// Overwrite an existing config.
        #[arg(long)]
        force: bool,
        /// Override the detected default branch (display only).
        #[arg(long)]
        default_branch: Option<String>,
    },
    /// Point Codex then Claude at this worktree; install every plugin.
    Sync,
    /// Point both runtimes back at the repo's default branch.
    Reset,
    /// Live snapshot of where both runtimes currently point.
    Status,
}

fn home() -> Result<Utf8PathBuf> {
    let h = dirs::home_dir().context("cannot determine home directory")?;
    Utf8PathBuf::from_path_buf(h).map_err(|p| anyhow::anyhow!("non-UTF-8 home: {p:?}"))
}

fn skillctl_config_path() -> Result<Utf8PathBuf> {
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    Ok(config::config_path(xdg.as_deref(), &home()?))
}

fn codex_config_path() -> Result<Utf8PathBuf> {
    Ok(home()?.join(".codex").join("config.toml"))
}

fn cwd() -> Result<Utf8PathBuf> {
    let c = std::env::current_dir().context("cannot read current directory")?;
    Utf8PathBuf::from_path_buf(c).map_err(|p| anyhow::anyhow!("non-UTF-8 cwd: {p:?}"))
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let runner = RealCommandRunner;
    let reporter = StderrReporter;

    match cli.command {
        Command::Init {
            force,
            default_branch,
        } => {
            let cfg_path = skillctl_config_path()?;
            let report = command::init(
                &runner,
                &cwd()?,
                &cfg_path,
                force,
                default_branch.as_deref(),
            )?;
            emit(&output::render_init(&report))?;
        }
        Command::Sync => {
            let cfg = load_config()?;
            let start = Instant::now();
            let report = command::sync(&runner, &cwd()?, &codex_config_path()?, &cfg, &reporter)?;
            anstream::eprintln!(
                "{}",
                output::sync_summary(report.plugins.len(), start.elapsed())
            );
            anstream::eprintln!("  {}", output::warning(output::RESTART_NOTICE));
        }
        Command::Reset => {
            let cfg = load_config()?;
            let start = Instant::now();
            let report = command::reset(&runner, &cwd()?, &cfg, &reporter)?;
            anstream::eprintln!(
                "{}",
                output::reset_summary(&report.owner_repo, start.elapsed())
            );
            anstream::eprintln!("  {}", output::warning(output::RESTART_NOTICE));
        }
        Command::Status => {
            let cfg = load_config()?;
            let report = command::status(&runner, &cwd()?, &codex_config_path()?, &cfg)?;
            emit(&output::render_status(&report))?;
            if !report.remote_matches {
                anstream::eprintln!(
                    "  {}",
                    output::hint(
                        "`skillctl sync` will refuse until origin matches the \
                         configured remote"
                    )
                );
            }
            anstream::eprintln!(
                "  {}",
                output::warning("restart Claude and Codex after any sync/reset")
            );
        }
    }
    Ok(())
}

/// Write a stdout payload, exiting silently (status 0) on a closed pipe so
/// `skillctl status | head` behaves like a normal Unix tool instead of
/// panicking the way `println!` would.
fn emit(s: &str) -> Result<()> {
    use std::io::Write;
    let mut out = anstream::stdout();
    match writeln!(out, "{s}").and_then(|()| out.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(e) => Err(e).context("writing to stdout"),
    }
}

fn load_config() -> Result<Config> {
    Config::load(&skillctl_config_path()?)
}
