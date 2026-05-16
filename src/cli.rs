//! Argument parsing and the thin dispatch that wires real I/O to the
//! orchestration in [`crate::command`].

use crate::command::{self, RealCommandRunner};
use crate::config::{self, Config};
use crate::output;
use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "skillctl",
    about = "Point Claude & Codex at a marketplace worktree, then reset them back.",
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
            println!("Wrote {}", report.config_path);
            println!("  repo:   {}", report.repo_root);
            println!("  remote: {}", report.remote);
            println!("  default branch: {}", report.default_branch);
            if !report.claude_file_present {
                println!(
                    "  warning: Claude marketplace file not found at the \
                     configured path"
                );
            }
            if !report.codex_file_present {
                println!(
                    "  warning: Codex marketplace file not found at the \
                     configured path"
                );
            }
        }
        Command::Sync => {
            let cfg = load_config()?;
            let report = command::sync(&runner, &cwd()?, &codex_config_path()?, &cfg)?;
            print!("{}", output::render_sync(&report));
        }
        Command::Reset => {
            let cfg = load_config()?;
            let report = command::reset(&runner, &cwd()?, &cfg)?;
            print!("{}", output::render_reset(&report));
        }
        Command::Status => {
            let cfg = load_config()?;
            let report = command::status(&runner, &cwd()?, &codex_config_path()?, &cfg)?;
            print!("{}", output::render_status(&report));
        }
    }
    Ok(())
}

fn load_config() -> Result<Config> {
    Config::load(&skillctl_config_path()?)
}
