//! Argument parsing and the thin dispatch that wires real I/O to the
//! orchestration in [`crate::command`].
//!
//! Stream discipline: the `status` report is the command's *answer* and goes
//! to **stdout**; progress (the streamed sync/reset trail), summaries,
//! warnings and hints are status and go to **stderr** so they never pollute a
//! piped result.

use crate::command::{self, RealCommandRunner, TargetSelection};
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
                  Test the skills you're editing in a live Claude & Codex by \
                  pointing both at your marketplace worktree, then reset them \
                  back.",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Detect this repo and write `~/.config/skillctl/config.toml`.
    ///
    /// By default each runtime is managed only when its CLI is on PATH and
    /// the repo ships its marketplace file. `--claude-only`/`--codex-only`
    /// override that and scope to exactly one runtime.
    Init {
        /// Overwrite an existing config.
        #[arg(long)]
        force: bool,
        /// Manage only Claude (ignore Codex entirely).
        #[arg(long, conflicts_with = "codex_only")]
        claude_only: bool,
        /// Manage only Codex (ignore Claude entirely).
        #[arg(long, conflicts_with = "claude_only")]
        codex_only: bool,
    },
    /// Point Codex then Claude at this worktree so your edited skills go live.
    Sync,
    /// Snap both runtimes back to the repo's default branch.
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

    match cli.command {
        Command::Init {
            force,
            claude_only,
            codex_only,
        } => {
            let selection = match (claude_only, codex_only) {
                (true, _) => TargetSelection::ClaudeOnly,
                (_, true) => TargetSelection::CodexOnly,
                _ => TargetSelection::Auto,
            };
            let cfg_path = skillctl_config_path()?;
            let report = command::init(&runner, &cwd()?, &cfg_path, force, selection)?;
            emit(&output::render_init(&report))?;
        }
        Command::Sync => {
            let cfg = load_config()?;
            // Resolve paths *before* the spinner exists so a path error can't
            // strand a live frame on screen.
            let cwd = cwd()?;
            let codex_cfg = codex_config_path()?;
            let reporter = make_reporter();
            let start = Instant::now();
            let result = command::sync(&runner, &cwd, &codex_cfg, &cfg, reporter.as_ref());
            let elapsed = start.elapsed();
            // Clear the spinner *before* the summary or an error report is
            // printed, so neither is drawn over a live frame; the streamed
            // trail printed above the spinner stays on screen.
            reporter.finish();
            let report = result?;
            anstream::eprintln!(
                "{}",
                output::sync_summary(
                    &report,
                    elapsed,
                    cfg.targets.claude.enabled,
                    cfg.targets.codex.enabled
                )
            );
            if !report.codex_unactivated_plugins.is_empty() {
                anstream::eprintln!(
                    "{}",
                    output::codex_unactivated_warning(&report.codex_unactivated_plugins)
                );
            }
        }
        Command::Reset => {
            let cfg = load_config()?;
            let reporter = make_reporter();
            let start = Instant::now();
            let result = command::reset(&runner, &cfg, reporter.as_ref());
            let elapsed = start.elapsed();
            reporter.finish();
            let report = result?;
            anstream::eprintln!(
                "{}",
                output::reset_summary(
                    &report.source,
                    elapsed,
                    cfg.targets.claude.enabled,
                    cfg.targets.codex.enabled
                )
            );
            if !report.codex_unactivated_plugins.is_empty() {
                anstream::eprintln!(
                    "{}",
                    output::codex_unactivated_warning(&report.codex_unactivated_plugins)
                );
            }
        }
        Command::Status => {
            let cfg = load_config()?;
            let report = command::status(&runner, &cwd()?, &codex_config_path()?, &cfg)?;
            emit(&output::render_status(&report))?;
            if matches!(&report.snapshot, Some(s) if !s.remote_matches) {
                anstream::eprintln!(
                    "  {}",
                    output::hint(
                        "`skillctl sync` will refuse until origin matches the \
                         configured remote"
                    )
                );
            }
        }
    }
    Ok(())
}

/// Pick the `sync`/`reset` progress reporter: an animated spinner only on an
/// interactive terminal; otherwise the plain streamed reporter, so piped/CI
/// output stays exactly as it was before the spinner existed.
///
/// Whether ANSI should be emitted at all (`NO_COLOR`, `CLICOLOR`,
/// `CLICOLOR_FORCE`, `TERM`, global override) is deferred to `anstream` — the
/// same authority the rest of the tool's output uses — for one consistent
/// color discipline. A real TTY is *additionally* required: the spinner needs
/// cursor control, so `CLICOLOR_FORCE` into a pipe must not animate. `CI` is
/// its own carve-out: even a CI that wants color must not get a redrawing
/// spinner in its logs.
fn make_reporter() -> Box<dyn output::Reporter> {
    use std::io::IsTerminal;
    let ansi = anstream::AutoStream::choice(&std::io::stderr()) != anstream::ColorChoice::Never;
    let interactive = std::io::stderr().is_terminal() && ansi && std::env::var_os("CI").is_none();
    if interactive {
        Box::new(output::ProgressReporter::new())
    } else {
        Box::new(StderrReporter)
    }
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
