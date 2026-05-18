//! The single seam between skillctl and the outside world: every `git`,
//! `claude`, and `codex` invocation goes through [`CommandRunner`]. Production
//! uses [`RealCommandRunner`]; tests inject a recording fake so the
//! orchestration logic (ordering, exit-code tolerance, presence checks) is
//! verifiable without touching real global state.

use crate::config::{self, Config};
use crate::git;
use crate::output::{Event, Reporter};
use crate::targets::{claude, codex, Marketplace};
use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

/// `Send + Sync` so a single `&dyn CommandRunner` can be shared across the
/// scoped threads that fan out Claude's plugin installs ([`apply_marketplace`]).
pub trait CommandRunner: Send + Sync {
    /// Run `program args...` (optionally in `cwd`) to completion. Returns
    /// `Err` only when the process could not be spawned at all; a non-zero
    /// exit is reported via [`CommandOutput::status`], never as `Err`.
    fn run(&self, program: &str, args: &[&str], cwd: Option<&Utf8Path>) -> Result<CommandOutput>;
}

pub struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(&self, program: &str, args: &[&str], cwd: Option<&Utf8Path>) -> Result<CommandOutput> {
        let mut cmd = std::process::Command::new(program);
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let out = cmd
            .output()
            .with_context(|| format!("failed to spawn `{program}`"))?;
        Ok(CommandOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

// ===========================================================================
// Sub-command orchestration
// ===========================================================================

/// Which runtimes `init` should manage. `Auto` (no `--*-only` flag) enables a
/// runtime only when its CLI is on `PATH` *and* the repo ships its marketplace
/// file; the `*Only` variants are an explicit hard override that ignores
/// detection entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetSelection {
    Auto,
    ClaudeOnly,
    CodexOnly,
}

/// Per-runtime outcome of `init`'s detection, carried to the renderer.
/// `skip_reason` is `Some` iff the runtime was *not* enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetOutcome {
    pub enabled: bool,
    pub file_present: bool,
    pub skip_reason: Option<String>,
}

impl TargetOutcome {
    fn enabled(file_present: bool) -> Self {
        Self {
            enabled: true,
            file_present,
            skip_reason: None,
        }
    }

    fn skipped(file_present: bool, why: impl Into<String>) -> Self {
        Self {
            enabled: false,
            file_present,
            skip_reason: Some(why.into()),
        }
    }

    /// Auto-detection rule: manage a runtime only when its CLI is on `PATH`
    /// *and* its marketplace file is in the repo.
    fn detected(on_path: bool, file_present: bool, no_file: impl Into<String>) -> Self {
        if !on_path {
            Self::skipped(file_present, "not on PATH")
        } else if !file_present {
            Self::skipped(file_present, no_file)
        } else {
            Self::enabled(file_present)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitReport {
    pub repo_root: Utf8PathBuf,
    pub remote: String,
    pub default_branch: String,
    pub claude: TargetOutcome,
    pub codex: TargetOutcome,
    pub config_path: Utf8PathBuf,
}

/// Is `program` usable here? Routed through the [`CommandRunner`] seam (not a
/// `which` crate) so detection is unit-testable with the same fake the
/// orchestration tests use. A non-spawnable binary surfaces as `Err` from the
/// real runner; both that and a non-zero `--version` mean "not on PATH".
fn on_path(runner: &dyn CommandRunner, program: &str) -> bool {
    runner
        .run(program, &["--version"], None)
        .map(|o| o.success())
        .unwrap_or(false)
}

/// `skillctl init`: detect the repo, refuse to clobber an existing config
/// unless `force`, decide which runtimes to manage (auto-detection or an
/// explicit `--*-only` override), and write the config keyed to `origin`.
pub fn init(
    runner: &dyn CommandRunner,
    cwd: &Utf8Path,
    config_path: &Utf8Path,
    force: bool,
    selection: TargetSelection,
) -> Result<InitReport> {
    let repo_root = git::repo_root(runner, cwd)?;

    if config_path.exists() && !force {
        bail!(
            "skillctl config already exists at {config_path} — \
             pass --force to overwrite"
        );
    }

    let remote = git::origin_url(runner, &repo_root)?;
    let default_branch = git::default_branch(runner, &repo_root);

    let claude_file = repo_root.join(config::CLAUDE_MARKETPLACE_FILE).exists();
    let codex_file = repo_root.join(config::CODEX_MARKETPLACE_FILE).exists();

    let (claude, codex) = match selection {
        TargetSelection::Auto => (
            TargetOutcome::detected(
                on_path(runner, "claude"),
                claude_file,
                format!("no {}", config::CLAUDE_MARKETPLACE_FILE),
            ),
            TargetOutcome::detected(
                on_path(runner, "codex"),
                codex_file,
                format!("no {}", config::CODEX_MARKETPLACE_FILE),
            ),
        ),
        TargetSelection::ClaudeOnly => (
            TargetOutcome::enabled(claude_file),
            TargetOutcome::skipped(codex_file, "excluded by --claude-only"),
        ),
        TargetSelection::CodexOnly => (
            TargetOutcome::skipped(claude_file, "excluded by --codex-only"),
            TargetOutcome::enabled(codex_file),
        ),
    };

    if !claude.enabled && !codex.enabled {
        bail!(
            "no runtimes to manage here: neither `claude` nor `codex` is on \
             PATH with its marketplace file in this repo.\n  \
             install a runtime and re-run, or scope explicitly with \
             --claude-only / --codex-only"
        );
    }

    Config::new(remote.clone(), claude.enabled, codex.enabled).save(config_path)?;

    Ok(InitReport {
        repo_root,
        remote,
        default_branch,
        claude,
        codex,
        config_path: config_path.to_path_buf(),
    })
}

/// `sync`/`reset` are no-ops with both targets off; fail loudly instead of
/// silently doing nothing.
fn ensure_any_target(cfg: &Config) -> Result<()> {
    if !cfg.targets.claude.enabled && !cfg.targets.codex.enabled {
        bail!(
            "no targets enabled in config — re-run `skillctl init` \
             (optionally with --claude-only / --codex-only)"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    pub repo_root: Utf8PathBuf,
    pub claude_name: Option<String>,
    pub codex_name: Option<String>,
    pub plugins: Vec<String>,
    /// Codex plugins that registered but won't auto-install because their
    /// `policy.installation` is not `INSTALLED_BY_DEFAULT` — surfaced as a
    /// post-sync advisory (skillctl cannot install Codex plugins itself).
    pub codex_unactivated_plugins: Vec<String>,
}

/// Read and structurally parse one marketplace file under `repo_root`,
/// returning the parsed `Marketplace` plus the raw JSON (Codex needs the raw
/// text for its shallow policy check).
fn read_market(repo_root: &Utf8Path, rel: &Utf8Path) -> Result<(Marketplace, String)> {
    let path = repo_root.join(rel);
    let raw = std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
    let mkt = Marketplace::parse(&raw).with_context(|| format!("in {path}"))?;
    Ok((mkt, raw))
}

/// What `reset` needs from the marketplace source: the two parsed
/// marketplaces (each `Some` only when that target is enabled) plus the
/// Codex plugins that won't auto-install (see [`codex::installation_advisory`]).
#[derive(Debug)]
struct LoadedMarketplaces {
    claude: Option<Marketplace>,
    codex: Option<Marketplace>,
    codex_unactivated: Vec<String>,
}

/// Source of the parsed marketplace files `reset` applies. Production
/// shallow-clones the configured remote's default branch; tests stub it so
/// the clone + filesystem step never needs real git or a network.
trait MarketplacePair {
    fn load(&self, cfg: &Config) -> Result<LoadedMarketplaces>;
}

/// Production [`MarketplacePair`]: `git clone --depth 1 -- <remote> <tmp>`
/// (no `--branch` ⇒ the remote's default branch; host-agnostic for GitHub,
/// GitLab incl. nested groups, Bitbucket, self-hosted; ssh or https), then
/// read both marketplace files out of the checkout. The `TempDir` is removed
/// on drop, *after* the reads.
struct RemoteClone<'a> {
    runner: &'a dyn CommandRunner,
    remote: &'a str,
}

impl RemoteClone<'_> {
    /// Clone into `checkout`, then read both marketplace files from it. Split
    /// from [`MarketplacePair::load`] so a test can drive this read path
    /// against a pre-populated dir with a scripted-success fake `git clone`.
    fn load_from(&self, cfg: &Config, checkout: &Utf8Path) -> Result<LoadedMarketplaces> {
        let out = self.runner.run(
            "git",
            &[
                "clone",
                "--depth",
                "1",
                "--",
                self.remote,
                checkout.as_str(),
            ],
            None,
        )?;
        if !out.success() {
            bail!(
                "git clone of the configured remote failed (could not fetch {}): {}",
                self.remote,
                out.stderr.trim()
            );
        }
        let claude = cfg
            .targets
            .claude
            .enabled
            .then(|| read_market(checkout, &cfg.targets.claude.marketplace_file))
            .transpose()?
            .map(|(m, _)| m);
        let (codex, codex_unactivated) = if cfg.targets.codex.enabled {
            let (m, raw) = read_market(checkout, &cfg.targets.codex.marketplace_file)?;
            let unactivated = codex::installation_advisory(&raw)?;
            (Some(m), unactivated)
        } else {
            (None, Vec::new())
        };
        Ok(LoadedMarketplaces {
            claude,
            codex,
            codex_unactivated,
        })
    }
}

impl MarketplacePair for RemoteClone<'_> {
    fn load(&self, cfg: &Config) -> Result<LoadedMarketplaces> {
        let tmp = tempfile::Builder::new()
            .prefix("skillctl-reset-")
            .tempdir()
            .context("creating a temp dir for the shallow clone")?;
        let checkout =
            Utf8Path::from_path(tmp.path()).context("temp dir path is not valid UTF-8")?;
        let loaded = self.load_from(cfg, checkout)?;
        drop(tmp); // remove the clone now that both files are read
        Ok(loaded)
    }
}

/// Everything pre-flight parsed/validated, shared by `sync` and `reset`.
struct Preflight {
    repo_root: Utf8PathBuf,
    claude_mkt: Option<Marketplace>,
    codex_mkt: Option<Marketplace>,
    /// Codex plugins that will register but not auto-install (not
    /// `INSTALLED_BY_DEFAULT`). Empty when Codex is disabled.
    codex_unactivated: Vec<String>,
}

/// Detect the repo, prove `origin` matches the configured remote, parse both
/// marketplace files, run Claude's authoritative validator, and apply Codex's
/// known shallow rule — all before any state is touched. `check_remote` is
/// skipped by `reset` (it deliberately points away from this worktree).
fn preflight(
    runner: &dyn CommandRunner,
    cwd: &Utf8Path,
    cfg: &Config,
    check_remote: bool,
) -> Result<Preflight> {
    let repo_root = git::repo_root(runner, cwd)?;

    if check_remote {
        let origin = git::origin_url(runner, &repo_root)?;
        let want = git::canonical_remote_key(&cfg.repo.remote);
        let got = git::canonical_remote_key(&origin);
        if want.is_none() || got.is_none() || want != got {
            bail!(
                "origin remote does not match configured remote — refusing to sync.\n  \
                 configured: {}\n  origin:     {origin}",
                cfg.repo.remote
            );
        }
    }

    let claude_mkt = if cfg.targets.claude.enabled {
        let (mkt, _) = read_market(&repo_root, &cfg.targets.claude.marketplace_file)?;
        let out = runner.run("claude", &["plugin", "validate", repo_root.as_str()], None)?;
        if !out.success() {
            let detail = if out.stdout.trim().is_empty() {
                out.stderr.trim()
            } else {
                out.stdout.trim()
            };
            bail!("`claude plugin validate` rejected the marketplace:\n{detail}");
        }
        Some(mkt)
    } else {
        None
    };

    let mut codex_unactivated = Vec::new();
    let codex_mkt = if cfg.targets.codex.enabled {
        let (mkt, raw) = read_market(&repo_root, &cfg.targets.codex.marketplace_file)?;
        codex::validate_marketplace_json(&raw)
            .with_context(|| format!("in {}", cfg.targets.codex.marketplace_file))?;
        codex_unactivated = codex::installation_advisory(&raw)
            .with_context(|| format!("in {}", cfg.targets.codex.marketplace_file))?;
        Some(mkt)
    } else {
        None
    };

    Ok(Preflight {
        repo_root,
        claude_mkt,
        codex_mkt,
        codex_unactivated,
    })
}

struct Applied {
    codex_name: Option<String>,
    claude_name: Option<String>,
    plugins: Vec<String>,
}

/// Point both runtimes' marketplace at `source` (a worktree path for `sync`,
/// an `owner/repo` for `reset`) and reinstall every Claude plugin. Codex is
/// mutated first so an unanticipated Codex rejection aborts before Claude is
/// touched — there is no rollback, so this ordering is what prevents a
/// split brain. Codex's `remove` is unconditional (a different source under
/// the same name is otherwise refused) and its exit 1 when absent is
/// tolerated; Claude's `remove` is non-idempotent so it is gated on a
/// presence check, and Claude's `remove` orphans installed plugins so every
/// plugin is (re)installed afterwards.
fn apply_marketplace(
    runner: &dyn CommandRunner,
    source: &str,
    claude_mkt: Option<&Marketplace>,
    codex_mkt: Option<&Marketplace>,
    scope: &str,
    reporter: &dyn Reporter,
) -> Result<Applied> {
    let codex_name = if let Some(m) = codex_mkt {
        let _ = runner.run("codex", &["plugin", "marketplace", "remove", &m.name], None)?; // exit 1 (absent) tolerated
        let add = runner.run("codex", &["plugin", "marketplace", "add", source], None)?;
        if !add.success() {
            bail!(
                "`codex plugin marketplace add` failed: {}",
                add.stderr.trim()
            );
        }
        reporter.event(Event::Marketplace {
            runtime: "Codex",
            name: &m.name,
        });
        Some(m.name.clone())
    } else {
        None
    };

    let mut plugins = Vec::new();
    let claude_name = if let Some(m) = claude_mkt {
        if claude_marketplace_present(runner, &m.name)? {
            let rm = runner.run(
                "claude",
                &["plugin", "marketplace", "remove", &m.name],
                None,
            )?;
            if !rm.success() {
                bail!(
                    "`claude plugin marketplace remove {}` failed: {}",
                    m.name,
                    rm.stderr.trim()
                );
            }
        }
        let add = runner.run("claude", &["plugin", "marketplace", "add", source], None)?;
        if !add.success() {
            bail!(
                "`claude plugin marketplace add` failed: {}",
                add.stderr.trim()
            );
        }
        reporter.event(Event::Marketplace {
            runtime: "Claude",
            name: &m.name,
        });

        // Each `claude plugin install` is independent and purely additive, so
        // fan them out across a bounded pool of scoped threads rather than
        // paying the CLI cold-start serially. A shared atomic cursor caps
        // in-flight installs at `MAX_INSTALL_CONCURRENCY` regardless of plugin
        // count; this is the *only* parallel region — Codex and Claude's
        // marketplace mutations stay sequential, so the Codex-before-Claude
        // split-brain ordering is untouched. Every failure is collected (not
        // bailed on the first) so one bad plugin can't mask the rest, and the
        // aggregate is sorted so the error message is deterministic.
        const MAX_INSTALL_CONCURRENCY: usize = 4;
        let workers = m.plugins.len().clamp(1, MAX_INSTALL_CONCURRENCY);
        let next = std::sync::atomic::AtomicUsize::new(0);
        let failures: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(p) = m.plugins.get(i) else { break };
                    let spec = format!("{p}@{}", m.name);
                    match runner.run(
                        "claude",
                        &["plugin", "install", &spec, "--scope", scope],
                        None,
                    ) {
                        Ok(out) if out.success() => {
                            reporter.event(Event::Plugin { name: p });
                        }
                        Ok(out) => failures
                            .lock()
                            .unwrap()
                            .push((spec, out.stderr.trim().to_string())),
                        Err(e) => failures.lock().unwrap().push((spec, e.to_string())),
                    }
                });
            }
        });

        let mut failures = failures.into_inner().unwrap();
        if !failures.is_empty() {
            failures.sort();
            let detail = failures
                .iter()
                .map(|(spec, err)| format!("  {spec}: {err}"))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "{} plugin install{} failed:\n{detail}",
                failures.len(),
                if failures.len() == 1 { "" } else { "s" }
            );
        }
        plugins = m.plugins.clone();
        Some(m.name.clone())
    } else {
        None
    };

    Ok(Applied {
        codex_name,
        claude_name,
        plugins,
    })
}

/// `skillctl sync`: point Codex then Claude at this worktree and install every
/// plugin. All validation happens before any mutation; Codex is mutated before
/// Claude so an unanticipated Codex rejection can never leave a split brain.
pub fn sync(
    runner: &dyn CommandRunner,
    cwd: &Utf8Path,
    codex_config: &Utf8Path,
    cfg: &Config,
    reporter: &dyn Reporter,
) -> Result<SyncReport> {
    ensure_any_target(cfg)?;
    let pre = preflight(runner, cwd, cfg, true)?;
    let repo_root = pre.repo_root;
    let codex_unactivated = pre.codex_unactivated;
    let src = repo_root.as_str();

    // Announce the target *now* — after validation, before the first
    // mutation — so the worktree is on screen even if a later step fails.
    let st = git::work_state(runner, &repo_root)?;
    let target = git::owner_repo(&cfg.repo.remote).unwrap_or_else(|| cfg.repo.remote.clone());
    reporter.event(Event::Target {
        action: "Syncing",
        target: &target,
        root: &repo_root,
        branch: &st.branch,
        commit: &st.commit,
        dirty: st.dirty,
    });

    let Applied {
        codex_name,
        claude_name,
        plugins,
    } = apply_marketplace(
        runner,
        src,
        pre.claude_mkt.as_ref(),
        pre.codex_mkt.as_ref(),
        cfg.targets.claude.scope.as_str(),
        reporter,
    )?;

    // ---- Post-sync assertion (loud): both runtimes must now resolve the
    // marketplace name to *this* worktree, else the name-identity contract
    // skillctl relies on is broken. ----
    if let Some(name) = &claude_name {
        let entries = claude_list(runner)?;
        let entry = entries.iter().find(|e| &e.name == name).with_context(|| {
            format!(
                "name-identity contract broken (Claude): marketplace \
                 \"{name}\" is not registered after sync"
            )
        })?;
        let got = claude::entry_source(entry).unwrap_or_default();
        if !same_path(&got, src) {
            bail!(
                "name-identity contract broken (Claude): \"{name}\" points \
                 at {got}, expected {repo_root}"
            );
        }
    }
    if let Some(name) = &codex_name {
        let entry = codex::read_marketplace(codex_config, name)?.with_context(|| {
            format!(
                "name-identity contract broken (Codex): marketplace \
                     \"{name}\" is not registered after sync"
            )
        })?;
        if !same_path(&entry.source, src) {
            bail!(
                "name-identity contract broken (Codex): \"{name}\" points \
                 at {}, expected {repo_root}",
                entry.source
            );
        }
    }

    Ok(SyncReport {
        repo_root,
        claude_name,
        codex_name,
        plugins,
        codex_unactivated_plugins: codex_unactivated,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetReport {
    pub source: String,
    pub claude_name: Option<String>,
    pub codex_name: Option<String>,
    pub plugins: Vec<String>,
    /// Codex plugins that registered but won't auto-install because their
    /// `policy.installation` is not `INSTALLED_BY_DEFAULT` — surfaced as a
    /// post-reset advisory (skillctl cannot install Codex plugins itself).
    pub codex_unactivated_plugins: Vec<String>,
}

/// `skillctl reset`: snap both runtimes back to the configured marketplace's
/// default branch. It shallow-clones the configured remote (it never reads
/// the local worktree), so it works from *any* directory and can never
/// reinstall an uncommitted local plugin set — the plugin list always
/// matches what the runtimes resolve when pointed at the remote URL.
/// Codex-first, like `sync`. Claude's `marketplace remove` orphans its
/// installed plugins, so every plugin is reinstalled afterwards. A recovery
/// command: no `origin` check, no validators, no git worktree required.
pub fn reset(
    runner: &dyn CommandRunner,
    cfg: &Config,
    reporter: &dyn Reporter,
) -> Result<ResetReport> {
    let pair = RemoteClone {
        runner,
        remote: &cfg.repo.remote,
    };
    reset_with(runner, cfg, &pair, reporter)
}

/// `reset` with the marketplace source injected, so tests exercise the
/// orchestration without real git or a network (see `StubPair`).
fn reset_with(
    runner: &dyn CommandRunner,
    cfg: &Config,
    source_pair: &dyn MarketplacePair,
    reporter: &dyn Reporter,
) -> Result<ResetReport> {
    ensure_any_target(cfg)?;
    // Validate the remote shape up-front (single source of the bad-remote
    // message) — before any temp dir or `git clone` is attempted.
    let source = git::marketplace_source(&cfg.repo.remote).with_context(|| {
        format!(
            "configured remote is not a recognizable git remote: {}",
            cfg.repo.remote
        )
    })?;

    let LoadedMarketplaces {
        claude: claude_mkt,
        codex: codex_mkt,
        codex_unactivated,
    } = source_pair.load(cfg)?;

    reporter.event(Event::ResetTarget { source: &source });

    let Applied {
        codex_name,
        claude_name,
        plugins,
    } = apply_marketplace(
        runner,
        &source,
        claude_mkt.as_ref(),
        codex_mkt.as_ref(),
        cfg.targets.claude.scope.as_str(),
        reporter,
    )?;

    Ok(ResetReport {
        source,
        claude_name,
        codex_name,
        plugins,
        codex_unactivated_plugins: codex_unactivated,
    })
}

/// The git-repo-dependent half of `status`. `None` when cwd is not inside a
/// git worktree — `status` must still work from anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSnapshot {
    pub repo: git::RepoState,
    pub remote_matches: bool,
    pub default_branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub configured_remote: String,
    /// `None` ⇒ not inside a git repo; worktree/origin/match rows are skipped.
    pub snapshot: Option<RepoSnapshot>,
    /// Whether config manages each runtime — drives the `(not managed)` row.
    pub claude_enabled: bool,
    pub codex_enabled: bool,
    pub claude_name: Option<String>,
    pub codex_name: Option<String>,
    /// Where each runtime currently resolves its marketplace name, if known.
    pub claude_pointed_at: Option<String>,
    pub codex_pointed_at: Option<String>,
}

/// `skillctl status`: a fully live snapshot (no state file). Best-effort —
/// missing marketplace files or an unreadable runtime degrade individual
/// fields to `None` rather than failing the whole command. When cwd is not
/// inside a git repo the worktree/origin half is simply absent
/// (`snapshot: None`); the configured remote and where each runtime points
/// are still shown.
pub fn status(
    runner: &dyn CommandRunner,
    cwd: &Utf8Path,
    codex_config: &Utf8Path,
    cfg: &Config,
) -> Result<StatusReport> {
    // Being outside a git repo is not an error here — degrade to `None`. An
    // in-repo `git::state` failure (e.g. no `origin`) still propagates,
    // preserving today's in-repo behavior.
    let snapshot = match git::repo_root(runner, cwd) {
        Ok(repo_root) => {
            let repo = git::state(runner, &repo_root)?;
            let default_branch = git::default_branch(runner, &repo_root);
            let remote_matches = {
                let want = git::canonical_remote_key(&cfg.repo.remote);
                let got = git::canonical_remote_key(&repo.origin_url);
                want.is_some() && want == got
            };
            Some(RepoSnapshot {
                repo,
                remote_matches,
                default_branch,
            })
        }
        Err(_) => None,
    };

    let (claude_name, codex_name) = resolve_names(runner, codex_config, cfg, snapshot.as_ref());

    let claude_pointed_at = claude_name.as_ref().and_then(|name| {
        claude_list(runner)
            .ok()?
            .iter()
            .find(|e| &e.name == name)
            .and_then(claude::entry_source)
    });
    let codex_pointed_at = codex_name.as_ref().and_then(|name| {
        codex::read_marketplace(codex_config, name)
            .ok()
            .flatten()
            .map(|e| e.source)
    });

    Ok(StatusReport {
        configured_remote: cfg.repo.remote.clone(),
        snapshot,
        claude_enabled: cfg.targets.claude.enabled,
        codex_enabled: cfg.targets.codex.enabled,
        claude_name,
        codex_name,
        claude_pointed_at,
        codex_pointed_at,
    })
}

/// Resolve each runtime's marketplace name. In a repo: from the worktree's
/// marketplace files (unchanged — keeps in-repo output byte-identical).
/// Outside a repo there is no file, so match a *registered* marketplace
/// whose source resolves to the configured remote and take its name.
/// Best-effort: an unmatched runtime degrades to `None`.
fn resolve_names(
    runner: &dyn CommandRunner,
    codex_config: &Utf8Path,
    cfg: &Config,
    snapshot: Option<&RepoSnapshot>,
) -> (Option<String>, Option<String>) {
    if let Some(snap) = snapshot {
        let name_of = |rel: &Utf8Path| read_market(&snap.repo.root, rel).ok().map(|(m, _)| m.name);
        let claude = cfg
            .targets
            .claude
            .enabled
            .then(|| name_of(&cfg.targets.claude.marketplace_file))
            .flatten();
        let codex = cfg
            .targets
            .codex
            .enabled
            .then(|| name_of(&cfg.targets.codex.marketplace_file))
            .flatten();
        return (claude, codex);
    }

    let want = git::canonical_remote_key(&cfg.repo.remote);
    let claude = cfg
        .targets
        .claude
        .enabled
        .then(|| {
            want.as_ref()?;
            claude_list(runner).ok()?.into_iter().find_map(|e| {
                let src = claude::entry_source(&e)?;
                let key = git::canonical_remote_key(&src).or_else(|| github_owner_repo_key(&src));
                (key == want).then_some(e.name)
            })
        })
        .flatten();
    let codex = cfg
        .targets
        .codex
        .enabled
        .then(|| {
            codex::find_marketplace_by_source(codex_config, &cfg.repo.remote)
                .ok()
                .flatten()
        })
        .flatten();
    (claude, codex)
}

fn claude_list(runner: &dyn CommandRunner) -> Result<Vec<claude::ClaudeEntry>> {
    let out = runner.run("claude", &["plugin", "marketplace", "list", "--json"], None)?;
    if !out.success() {
        bail!(
            "`claude plugin marketplace list --json` failed: {}",
            out.stderr.trim()
        );
    }
    claude::parse_marketplace_list(&out.stdout)
}

fn claude_marketplace_present(runner: &dyn CommandRunner, name: &str) -> Result<bool> {
    Ok(claude_list(runner)?.iter().any(|e| e.name == name))
}

/// Best-effort canonical key for a Claude github entry whose source is a bare
/// `owner/repo` (Claude resolves those against github.com).
/// [`git::canonical_remote_key`] needs a host and returns `None` for a bare
/// slug — this fills that gap, used only by the outside-a-repo `status` match.
fn github_owner_repo_key(s: &str) -> Option<String> {
    let s = s.trim().trim_end_matches('/');
    (s.matches('/').count() == 1 && !s.contains(':'))
        .then(|| format!("github.com/{}", s.to_lowercase()))
}

/// The canonical "do these two paths point at the same worktree" rule,
/// tolerating only a trailing-slash difference. The single source of truth
/// for both the post-sync identity assertion (here) and the `status`
/// renderer's "→ this worktree" decision (`output::pointed`).
pub fn same_path(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

#[cfg(test)]
impl CommandOutput {
    /// Successful (exit 0) with the given stdout.
    pub fn ok(stdout: impl Into<String>) -> Self {
        CommandOutput {
            status: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }
    /// Failed with the given exit code and stderr.
    pub fn fail(status: i32, stderr: impl Into<String>) -> Self {
        CommandOutput {
            status,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }
}

/// A `CommandRunner` for tests: records every invocation in order and replays
/// scripted responses (first matching rule wins; unmatched calls succeed with
/// empty output). Lets orchestration tests assert the exact command sequence
/// without any real `git`/`claude`/`codex`.
#[cfg(test)]
pub mod fake {
    use super::*;
    use camino::Utf8PathBuf;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RecordedCall {
        pub program: String,
        pub args: Vec<String>,
        pub cwd: Option<Utf8PathBuf>,
    }

    impl RecordedCall {
        /// Convenience for assertions: `"git rev-parse --show-toplevel"`.
        pub fn line(&self) -> String {
            std::iter::once(self.program.clone())
                .chain(self.args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" ")
        }
    }

    // `Send + Sync` so a `RecordingRunner` satisfies the `CommandRunner:
    // Send + Sync` bound and can be shared across the install-fan-out threads.
    type Matcher = Box<dyn Fn(&str, &[&str]) -> bool + Send + Sync>;

    struct Rule {
        pred: Matcher,
        /// Successive responses; the last one is reused once exhausted.
        /// `Mutex` (not `RefCell`) so the runner stays `Sync`.
        responses: Mutex<VecDeque<CommandOutput>>,
    }

    pub struct RecordingRunner {
        rules: Vec<Rule>,
        calls: Mutex<Vec<RecordedCall>>,
    }

    impl RecordingRunner {
        pub fn new() -> Self {
            RecordingRunner {
                rules: Vec::new(),
                calls: Mutex::new(Vec::new()),
            }
        }

        /// Script a single response for calls matching `pred(program, args)`.
        pub fn on(
            self,
            pred: impl Fn(&str, &[&str]) -> bool + Send + Sync + 'static,
            out: CommandOutput,
        ) -> Self {
            self.on_seq(pred, vec![out])
        }

        /// Script successive responses for matching calls (call #1 → first,
        /// #2 → second, …; the last response repeats once exhausted).
        pub fn on_seq(
            mut self,
            pred: impl Fn(&str, &[&str]) -> bool + Send + Sync + 'static,
            outs: Vec<CommandOutput>,
        ) -> Self {
            self.rules.push(Rule {
                pred: Box::new(pred),
                responses: Mutex::new(outs.into()),
            });
            self
        }

        /// Convenience: match by program plus a substring present in any arg.
        pub fn on_arg(
            self,
            program: &'static str,
            arg_substr: &'static str,
            out: CommandOutput,
        ) -> Self {
            self.on(
                move |p, a| p == program && a.iter().any(|x| x.contains(arg_substr)),
                out,
            )
        }

        pub fn lines(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(RecordedCall::line)
                .collect()
        }
    }

    impl CommandRunner for RecordingRunner {
        fn run(
            &self,
            program: &str,
            args: &[&str],
            cwd: Option<&Utf8Path>,
        ) -> Result<CommandOutput> {
            self.calls.lock().unwrap().push(RecordedCall {
                program: program.to_string(),
                args: args.iter().map(|s| s.to_string()).collect(),
                cwd: cwd.map(|c| c.to_path_buf()),
            });
            for rule in &self.rules {
                if (rule.pred)(program, args) {
                    let mut q = rule.responses.lock().unwrap();
                    let out = if q.len() > 1 {
                        q.pop_front().unwrap()
                    } else {
                        q.front().cloned().unwrap_or_else(|| CommandOutput::ok(""))
                    };
                    return Ok(out);
                }
            }
            Ok(CommandOutput::ok(""))
        }
    }
}

#[cfg(test)]
mod orchestration_tests {
    use super::fake::RecordingRunner;
    use super::*;
    use std::collections::HashSet;

    fn tmp() -> (tempfile::TempDir, Utf8PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = Utf8PathBuf::from_path_buf(d.path().to_path_buf()).unwrap();
        (d, p)
    }

    const CLAUDE_MKT: &str = r#"{ "name": "M",
        "plugins": [ {"name":"p1"}, {"name":"p2"} ] }"#;
    const CODEX_MKT: &str = r#"{ "name": "M", "plugins": [
        { "name": "p1", "policy": { "authentication": "ON_INSTALL" } },
        { "name": "p2", "policy": { "authentication": "ON_INSTALL" } }
    ] }"#;
    const CLAUDE_MKT3: &str = r#"{ "name": "M",
        "plugins": [ {"name":"p1"}, {"name":"p2"}, {"name":"p3"} ] }"#;
    const CODEX_MKT3: &str = r#"{ "name": "M", "plugins": [
        { "name": "p1", "policy": { "authentication": "ON_INSTALL" } },
        { "name": "p2", "policy": { "authentication": "ON_INSTALL" } },
        { "name": "p3", "policy": { "authentication": "ON_INSTALL" } }
    ] }"#;

    /// A temp repo with both marketplace files written, plus a Config and a
    /// codex config path. `repo` is also used as `cwd`.
    struct Fix {
        _dir: tempfile::TempDir,
        repo: Utf8PathBuf,
        codex_cfg: Utf8PathBuf,
        cfg: Config,
    }

    fn fixture() -> Fix {
        let dir = tempfile::tempdir().unwrap();
        let repo = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(repo.join(".claude-plugin")).unwrap();
        std::fs::create_dir_all(repo.join(".agents/plugins")).unwrap();
        std::fs::write(repo.join(".claude-plugin/marketplace.json"), CLAUDE_MKT).unwrap();
        std::fs::write(repo.join(".agents/plugins/marketplace.json"), CODEX_MKT).unwrap();
        let codex_cfg = repo.join("codex-config.toml");
        let cfg = Config::new("git@github.com:co/agent-mkt.git", true, true);
        Fix {
            _dir: dir,
            repo,
            codex_cfg,
            cfg,
        }
    }

    /// Runner pre-scripted for a clean happy-path sync of `fixture()`.
    fn happy_runner(repo: &Utf8Path) -> RecordingRunner {
        let listed = format!(
            r#"[{{ "name": "M", "source": "{r}", "installLocation": "{r}" }}]"#,
            r = repo
        );
        RecordingRunner::new()
            .on_arg("git", "--show-toplevel", CommandOutput::ok(repo.as_str()))
            .on_arg(
                "git",
                "get-url",
                CommandOutput::ok("git@github.com:co/agent-mkt.git"),
            )
            .on_arg("claude", "validate", CommandOutput::ok("ok"))
            .on(
                |p, a| p == "claude" && a.contains(&"list"),
                CommandOutput::ok(listed),
            )
    }

    fn mutating(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .filter(|l| {
                l.contains("marketplace remove")
                    || l.contains("marketplace add")
                    || l.contains("plugin install")
            })
            .cloned()
            .collect()
    }

    #[test]
    fn sync_preflight_remote_mismatch_aborts_with_zero_mutations() {
        let f = fixture();
        let r = RecordingRunner::new()
            .on_arg("git", "--show-toplevel", CommandOutput::ok(f.repo.as_str()))
            .on_arg(
                "git",
                "get-url",
                CommandOutput::ok("git@github.com:someone/UNRELATED.git"),
            );
        let err = sync(
            &r,
            &f.repo,
            &f.codex_cfg,
            &f.cfg,
            &crate::output::NoopReporter,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.to_lowercase().contains("remote"),
            "error should mention remote mismatch: {err}"
        );
        assert!(
            mutating(&r.lines()).is_empty(),
            "no mutation may occur on pre-flight failure, saw: {:?}",
            mutating(&r.lines())
        );
    }

    #[test]
    fn sync_preflight_bad_codex_policy_aborts_with_zero_mutations() {
        let f = fixture();
        std::fs::write(
            f.repo.join(".agents/plugins/marketplace.json"),
            r#"{ "name": "M", "plugins": [
                { "name": "p1", "policy": { "authentication": "NONE" } } ] }"#,
        )
        .unwrap();
        let r = happy_runner(&f.repo);
        let err = format!(
            "{:#}",
            sync(
                &r,
                &f.repo,
                &f.codex_cfg,
                &f.cfg,
                &crate::output::NoopReporter
            )
            .unwrap_err()
        );
        assert!(err.contains("authentication"), "{err}");
        assert!(
            mutating(&r.lines()).is_empty(),
            "no mutation on bad-policy pre-flight: {:?}",
            mutating(&r.lines())
        );
    }

    #[test]
    fn sync_preflight_aborts_when_claude_validate_fails() {
        let f = fixture();
        let r = RecordingRunner::new()
            .on_arg("git", "--show-toplevel", CommandOutput::ok(f.repo.as_str()))
            .on_arg(
                "git",
                "get-url",
                CommandOutput::ok("git@github.com:co/agent-mkt.git"),
            )
            .on_arg(
                "claude",
                "validate",
                CommandOutput::fail(1, "marketplace.json: invalid"),
            );
        let err = sync(
            &r,
            &f.repo,
            &f.codex_cfg,
            &f.cfg,
            &crate::output::NoopReporter,
        )
        .unwrap_err()
        .to_string();
        assert!(err.to_lowercase().contains("validat"), "{err}");
        assert!(mutating(&r.lines()).is_empty());
    }

    /// Pretend `codex plugin marketplace add` ran: write the config.toml
    /// end-state the post-sync assertion will read back.
    fn fake_codex_added(codex_cfg: &Utf8Path, name: &str, source: &str) {
        std::fs::write(
            codex_cfg,
            format!("[marketplaces.{name}]\nsource_type = \"local\"\nsource = \"{source}\"\n"),
        )
        .unwrap();
    }

    fn agent_lines(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .filter(|l| l.starts_with("claude ") || l.starts_with("codex "))
            .cloned()
            .collect()
    }

    // The Claude plugin-install loop now fans out across scoped threads, so the
    // *relative order of the install calls is nondeterministic*. These helpers
    // let the order-sensitive tests pin everything that is still deterministic
    // (the split-brain backbone, presence gating, the post-assert `list`) while
    // asserting the installs by set membership plus a bracket invariant.

    /// Agent calls with `plugin install` removed — order is still fully
    /// deterministic and pins the Codex-before-Claude ordering.
    fn backbone(lines: &[String]) -> Vec<String> {
        agent_lines(lines)
            .into_iter()
            .filter(|l| !l.contains("plugin install"))
            .collect()
    }

    /// The set of `plugin install` calls (sequence is nondeterministic).
    /// Set membership alone can't catch a *duplicate* install, so pair it
    /// with [`install_count`] — a cursor off-by-one that double-claims an
    /// index would slip past `installs` but not the count.
    fn installs(lines: &[String]) -> HashSet<String> {
        agent_lines(lines)
            .into_iter()
            .filter(|l| l.contains("plugin install"))
            .collect()
    }

    /// How many `plugin install` calls were made (un-deduplicated): proves
    /// each plugin was installed exactly once across the fan-out.
    fn install_count(lines: &[String]) -> usize {
        agent_lines(lines)
            .iter()
            .filter(|l| l.contains("plugin install"))
            .count()
    }

    /// Every install must run strictly after the Claude `marketplace add`
    /// (the parallel region must stay inside that bracket). `before` is an
    /// agent line every install must also precede — sync's post-assert
    /// `list`; reset has no trailing list and passes `None`.
    fn assert_installs_bracketed(lines: &[String], after: &str, before: Option<&str>) {
        let agent = agent_lines(lines);
        let after_idx = agent
            .iter()
            .position(|l| l.as_str() == after)
            .unwrap_or_else(|| panic!("expected {after:?} in {agent:?}"));
        let before_idx = before.map(|b| {
            agent
                .iter()
                .rposition(|l| l.as_str() == b)
                .unwrap_or_else(|| panic!("expected {b:?} in {agent:?}"))
        });
        for (i, l) in agent.iter().enumerate() {
            if l.contains("plugin install") {
                assert!(
                    i > after_idx,
                    "install {l:?} must run after {after:?}: {agent:?}"
                );
                if let Some(bi) = before_idx {
                    assert!(
                        i < bi,
                        "install {l:?} must run before {before:?}: {agent:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn sync_happy_path_runs_codex_then_claude_in_exact_order() {
        let f = fixture();
        fake_codex_added(&f.codex_cfg, "M", f.repo.as_str());
        let r = happy_runner(&f.repo);

        let report = sync(
            &r,
            &f.repo,
            &f.codex_cfg,
            &f.cfg,
            &crate::output::NoopReporter,
        )
        .unwrap();

        let repo = f.repo.as_str();
        let lines = r.lines();
        // Deterministic backbone: Codex fully before Claude (split-brain
        // ordering), presence gating, and the trailing post-assert `list`.
        assert_eq!(
            backbone(&lines),
            vec![
                format!("claude plugin validate {repo}"),
                "codex plugin marketplace remove M".to_string(),
                format!("codex plugin marketplace add {repo}"),
                "claude plugin marketplace list --json".to_string(),
                "claude plugin marketplace remove M".to_string(),
                format!("claude plugin marketplace add {repo}"),
                "claude plugin marketplace list --json".to_string(),
            ]
        );
        // Both installs happened, each exactly once (order nondeterministic
        // across the fan-out; the count guards against a cursor double-claim).
        assert_eq!(install_count(&lines), 2, "each plugin installed once");
        assert_eq!(
            installs(&lines),
            HashSet::from([
                "claude plugin install p1@M --scope user".to_string(),
                "claude plugin install p2@M --scope user".to_string(),
            ])
        );
        // …and strictly between the Claude `marketplace add` and the
        // post-sync identity assertion's `list`.
        assert_installs_bracketed(
            &lines,
            &format!("claude plugin marketplace add {repo}"),
            Some("claude plugin marketplace list --json"),
        );
        assert_eq!(report.repo_root, f.repo);
        assert_eq!(report.claude_name.as_deref(), Some("M"));
        assert_eq!(report.codex_name.as_deref(), Some("M"));
        assert_eq!(report.plugins, vec!["p1", "p2"]);
        // CODEX_MKT sets no `policy.installation` ⇒ Codex registers the
        // marketplace but won't auto-install these; the advisory surfaces
        // them and the sync still succeeds (non-fatal).
        assert_eq!(report.codex_unactivated_plugins, vec!["p1", "p2"]);
    }

    #[test]
    fn sync_codex_advisory_is_empty_when_all_installed_by_default() {
        let f = fixture();
        std::fs::write(
            f.repo.join(".agents/plugins/marketplace.json"),
            r#"{ "name": "M", "plugins": [
                { "name": "p1", "policy": { "authentication": "ON_INSTALL",
                  "installation": "INSTALLED_BY_DEFAULT" } },
                { "name": "p2", "policy": { "authentication": "ON_INSTALL",
                  "installation": "INSTALLED_BY_DEFAULT" } } ] }"#,
        )
        .unwrap();
        fake_codex_added(&f.codex_cfg, "M", f.repo.as_str());
        let r = happy_runner(&f.repo);

        let report = sync(
            &r,
            &f.repo,
            &f.codex_cfg,
            &f.cfg,
            &crate::output::NoopReporter,
        )
        .unwrap();

        assert!(
            report.codex_unactivated_plugins.is_empty(),
            "INSTALLED_BY_DEFAULT ⇒ no advisory, got: {:?}",
            report.codex_unactivated_plugins
        );
    }

    #[test]
    fn sync_tolerates_codex_remove_exit_1_when_marketplace_absent() {
        let f = fixture();
        fake_codex_added(&f.codex_cfg, "M", f.repo.as_str());
        let r = happy_runner(&f.repo).on(
            |p, a| p == "codex" && a.contains(&"remove"),
            CommandOutput::fail(1, "marketplace not found"),
        );
        // A non-zero codex remove must NOT abort the sync.
        let report = sync(
            &r,
            &f.repo,
            &f.codex_cfg,
            &f.cfg,
            &crate::output::NoopReporter,
        )
        .unwrap();
        assert_eq!(report.codex_name.as_deref(), Some("M"));
        assert!(r
            .lines()
            .contains(&format!("codex plugin marketplace add {}", f.repo)));
    }

    #[test]
    fn sync_skips_claude_remove_when_marketplace_not_yet_present() {
        let f = fixture();
        fake_codex_added(&f.codex_cfg, "M", f.repo.as_str());
        let present = format!(
            r#"[{{ "name": "M", "installLocation": "{r}" }}]"#,
            r = f.repo
        );
        let r = RecordingRunner::new()
            .on_arg("git", "--show-toplevel", CommandOutput::ok(f.repo.as_str()))
            .on_arg(
                "git",
                "get-url",
                CommandOutput::ok("git@github.com:co/agent-mkt.git"),
            )
            .on_arg("claude", "validate", CommandOutput::ok("ok"))
            // 1st list (presence) = empty; 2nd list (post-assert) = present.
            .on_seq(
                |p, a| p == "claude" && a.contains(&"list"),
                vec![CommandOutput::ok("[]"), CommandOutput::ok(present)],
            );

        let report = sync(
            &r,
            &f.repo,
            &f.codex_cfg,
            &f.cfg,
            &crate::output::NoopReporter,
        )
        .unwrap();

        assert_eq!(report.claude_name.as_deref(), Some("M"));
        assert!(
            !r.lines()
                .iter()
                .any(|l| l == "claude plugin marketplace remove M"),
            "remove must be skipped when absent: {:?}",
            r.lines()
        );
        assert!(r
            .lines()
            .contains(&format!("claude plugin marketplace add {}", f.repo)));
    }

    #[test]
    fn sync_post_assert_fails_loud_when_claude_points_elsewhere() {
        let f = fixture();
        fake_codex_added(&f.codex_cfg, "M", f.repo.as_str());
        let r = RecordingRunner::new()
            .on_arg("git", "--show-toplevel", CommandOutput::ok(f.repo.as_str()))
            .on_arg(
                "git",
                "get-url",
                CommandOutput::ok("git@github.com:co/agent-mkt.git"),
            )
            .on_arg("claude", "validate", CommandOutput::ok("ok"))
            .on(
                |p, a| p == "claude" && a.contains(&"list"),
                CommandOutput::ok(r#"[{ "name": "M", "installLocation": "/some/STALE/path" }]"#),
            );
        let err = sync(
            &r,
            &f.repo,
            &f.codex_cfg,
            &f.cfg,
            &crate::output::NoopReporter,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("name-identity contract broken"),
            "expected loud identity failure, got: {err}"
        );
    }

    #[test]
    fn sync_streams_a_trail_that_survives_a_mid_sequence_failure() {
        let f = fixture();
        // Claude install of the *second* plugin fails.
        let r = RecordingRunner::new()
            .on_arg("git", "--show-toplevel", CommandOutput::ok(f.repo.as_str()))
            .on_arg(
                "git",
                "get-url",
                CommandOutput::ok("git@github.com:co/agent-mkt.git"),
            )
            .on_arg("claude", "validate", CommandOutput::ok("ok"))
            .on(
                |p, a| p == "claude" && a.contains(&"list"),
                CommandOutput::ok("[]"),
            )
            .on(
                |p, a| {
                    p == "claude" && a.contains(&"install") && a.iter().any(|x| x.contains("p2@"))
                },
                CommandOutput::fail(1, "boom"),
            );
        let rep = crate::output::RecordingReporter::default();

        let err = sync(&r, &f.repo, &f.codex_cfg, &f.cfg, &rep)
            .expect_err("sync should fail when plugin 2 fails to install")
            .to_string();
        // The failure is aggregated and names the offending plugin + stderr.
        assert!(
            err.contains("p2@M"),
            "error should name the failed plugin: {err}"
        );
        assert!(
            err.contains("boom"),
            "error should include the install stderr: {err}"
        );

        let trail = rep.lines.lock().unwrap();
        // The completed steps remain visible; p2 never got a line.
        assert!(trail.iter().any(|l| l == "Codex   ~ M"), "{trail:?}");
        assert!(trail.iter().any(|l| l == "Claude  ~ M"), "{trail:?}");
        assert!(trail.iter().any(|l| l == "+ p1"), "{trail:?}");
        assert!(!trail.iter().any(|l| l == "+ p2"), "{trail:?}");
    }

    #[test]
    fn sync_runs_all_installs_even_when_one_fails() {
        let f = fixture();
        // p1 fails; p2 must still be attempted and succeed — proving the
        // fan-out collects failures instead of bailing on the first one.
        let r = happy_runner(&f.repo).on(
            |p, a| p == "claude" && a.contains(&"install") && a.iter().any(|x| x.contains("p1@")),
            CommandOutput::fail(1, "p1 exploded"),
        );
        let rep = crate::output::RecordingReporter::default();

        let err = sync(&r, &f.repo, &f.codex_cfg, &f.cfg, &rep)
            .expect_err("a failed install must fail the sync")
            .to_string();

        assert!(err.contains("p1@M"), "{err}");
        assert!(err.contains("p1 exploded"), "{err}");
        let trail = rep.lines.lock().unwrap();
        assert!(
            trail.iter().any(|l| l == "+ p2"),
            "p2 must still install when p1 fails: {trail:?}"
        );
        assert!(!trail.iter().any(|l| l == "+ p1"), "{trail:?}");
    }

    #[test]
    fn sync_aggregates_multiple_install_failures() {
        let f = fixture();
        std::fs::write(f.repo.join(".claude-plugin/marketplace.json"), CLAUDE_MKT3).unwrap();
        std::fs::write(f.repo.join(".agents/plugins/marketplace.json"), CODEX_MKT3).unwrap();
        let r = happy_runner(&f.repo)
            .on(
                |p, a| {
                    p == "claude" && a.contains(&"install") && a.iter().any(|x| x.contains("p1@"))
                },
                CommandOutput::fail(1, "first boom"),
            )
            .on(
                |p, a| {
                    p == "claude" && a.contains(&"install") && a.iter().any(|x| x.contains("p3@"))
                },
                CommandOutput::fail(2, "third boom"),
            );

        let err = sync(
            &r,
            &f.repo,
            &f.codex_cfg,
            &f.cfg,
            &crate::output::NoopReporter,
        )
        .expect_err("two failed installs must fail the sync")
        .to_string();

        assert!(
            err.contains("2 plugin installs failed"),
            "should aggregate the count: {err}"
        );
        let p1 = err.find("p1@M").expect("p1 named in error");
        let p3 = err.find("p3@M").expect("p3 named in error");
        assert!(
            p1 < p3,
            "aggregated failures must be sorted for a deterministic message: {err}"
        );
        assert!(
            err.contains("first boom") && err.contains("third boom"),
            "both stderrs must surface: {err}"
        );
    }

    fn status_runner(repo: &Utf8Path, origin: &str) -> RecordingRunner {
        let listed = format!(
            r#"[{{ "name": "M", "source": "{r}", "installLocation": "{r}" }}]"#,
            r = repo
        );
        RecordingRunner::new()
            .on_arg("git", "--show-toplevel", CommandOutput::ok(repo.as_str()))
            .on_arg("git", "get-url", CommandOutput::ok(origin))
            .on_arg("git", "--abbrev-ref", CommandOutput::ok("pr-123"))
            .on_arg("git", "rev-parse", CommandOutput::ok("abc1234"))
            .on_arg("git", "--porcelain", CommandOutput::ok(" M x\n"))
            .on_arg("git", "symbolic-ref", CommandOutput::ok("origin/main"))
            .on(
                |p, a| p == "claude" && a.contains(&"list"),
                CommandOutput::ok(listed),
            )
    }

    #[test]
    fn status_reports_match_names_and_pointed_at_sources() {
        let f = fixture();
        fake_codex_added(&f.codex_cfg, "M", f.repo.as_str());
        let r = status_runner(&f.repo, "git@github.com:co/agent-mkt.git");

        let s = status(&r, &f.repo, &f.codex_cfg, &f.cfg).unwrap();

        let snap = s.snapshot.as_ref().expect("in a repo → snapshot present");
        assert!(snap.remote_matches);
        assert_eq!(snap.repo.branch, "pr-123");
        assert!(snap.repo.dirty);
        assert_eq!(snap.default_branch, "main");
        assert_eq!(s.claude_name.as_deref(), Some("M"));
        assert_eq!(s.codex_name.as_deref(), Some("M"));
        assert_eq!(s.claude_pointed_at.as_deref(), Some(f.repo.as_str()));
        assert_eq!(s.codex_pointed_at.as_deref(), Some(f.repo.as_str()));
    }

    #[test]
    fn status_flags_a_remote_mismatch() {
        let f = fixture();
        let r = status_runner(&f.repo, "git@github.com:other/unrelated.git");
        let s = status(&r, &f.repo, &f.codex_cfg, &f.cfg).unwrap();
        assert!(!s.snapshot.as_ref().expect("in a repo").remote_matches);
    }

    /// Stub [`MarketplacePair`]: hands `reset_with` canned marketplaces so the
    /// shallow-clone + filesystem step never needs real git or a network.
    struct StubPair {
        claude: Option<Marketplace>,
        codex: Option<Marketplace>,
        codex_unactivated: Vec<String>,
    }
    impl MarketplacePair for StubPair {
        fn load(&self, _cfg: &Config) -> Result<LoadedMarketplaces> {
            Ok(LoadedMarketplaces {
                claude: self.claude.clone(),
                codex: self.codex.clone(),
                codex_unactivated: self.codex_unactivated.clone(),
            })
        }
    }
    fn mkt(name: &str, plugins: &[&str]) -> Marketplace {
        Marketplace {
            name: name.into(),
            plugins: plugins.iter().map(|s| s.to_string()).collect(),
        }
    }
    fn both_mkts() -> StubPair {
        StubPair {
            claude: Some(mkt("M", &["p1", "p2"])),
            codex: Some(mkt("M", &["p1", "p2"])),
            codex_unactivated: Vec::new(),
        }
    }

    #[test]
    fn status_outside_a_repo_degrades_gracefully() {
        // `git rev-parse --show-toplevel` fails: not in a repo. status must
        // NOT error — it drops the worktree half and resolves names/pointers
        // from the live registries via the configured remote.
        let f = fixture();
        fake_codex_added(&f.codex_cfg, "M", "git@github.com:co/agent-mkt.git");
        let r = RecordingRunner::new()
            .on_arg(
                "git",
                "--show-toplevel",
                CommandOutput::fail(128, "fatal: not a git repository"),
            )
            .on(
                |p, a| p == "claude" && a.contains(&"list"),
                // A github entry (no installLocation) → entry_source = repo.
                CommandOutput::ok(
                    r#"[{ "name": "M", "source": "github", "repo": "co/agent-mkt" }]"#,
                ),
            );

        let s = status(&r, &f.repo, &f.codex_cfg, &f.cfg).unwrap();

        assert!(s.snapshot.is_none(), "no snapshot outside a repo");
        assert_eq!(s.configured_remote, "git@github.com:co/agent-mkt.git");
        assert_eq!(s.claude_name.as_deref(), Some("M"));
        assert_eq!(s.codex_name.as_deref(), Some("M"));
        assert_eq!(s.claude_pointed_at.as_deref(), Some("co/agent-mkt"));
        assert_eq!(
            s.codex_pointed_at.as_deref(),
            Some("git@github.com:co/agent-mkt.git")
        );
    }

    #[test]
    fn reset_points_both_runtimes_at_the_remote_url_and_reinstalls() {
        let f = fixture();
        // Claude already has "M" registered → conditional remove must fire.
        let r = RecordingRunner::new().on(
            |p, a| p == "claude" && a.contains(&"list"),
            CommandOutput::ok(r#"[{ "name": "M", "source": "github" }]"#),
        );

        let report = reset_with(&r, &f.cfg, &both_mkts(), &crate::output::NoopReporter).unwrap();

        assert_eq!(report.source, "git@github.com:co/agent-mkt.git");
        assert_eq!(report.plugins, vec!["p1", "p2"]);
        let lines = r.lines();
        assert_eq!(
            backbone(&lines),
            vec![
                "codex plugin marketplace remove M".to_string(),
                "codex plugin marketplace add git@github.com:co/agent-mkt.git".to_string(),
                "claude plugin marketplace list --json".to_string(),
                "claude plugin marketplace remove M".to_string(),
                "claude plugin marketplace add git@github.com:co/agent-mkt.git".to_string(),
            ]
        );
        assert_eq!(install_count(&lines), 2, "each plugin installed once");
        assert_eq!(
            installs(&lines),
            HashSet::from([
                "claude plugin install p1@M --scope user".to_string(),
                "claude plugin install p2@M --scope user".to_string(),
            ])
        );
        // reset has no post-assert `list`; installs are simply after the add.
        assert_installs_bracketed(
            &lines,
            "claude plugin marketplace add git@github.com:co/agent-mkt.git",
            None,
        );
    }

    #[test]
    fn reset_accepts_a_nested_gitlab_remote() {
        // A nested GitLab group/subgroup/repo remote — the >3-path-segment
        // case the old `owner/repo` slug parser rejected. Passed through whole.
        let url = "git@gitlab.com:company/team/agent-marketplace.git";
        let mut f = fixture();
        f.cfg = Config::new(url, true, true);
        let r = RecordingRunner::new().on(
            |p, a| p == "claude" && a.contains(&"list"),
            CommandOutput::ok("[]"),
        );

        let report = reset_with(&r, &f.cfg, &both_mkts(), &crate::output::NoopReporter).unwrap();

        assert_eq!(report.source, url);
        let lines = agent_lines(&r.lines());
        assert!(
            lines.contains(&format!("codex plugin marketplace add {url}")),
            "{lines:?}"
        );
        assert!(
            lines.contains(&format!("claude plugin marketplace add {url}")),
            "{lines:?}"
        );
    }

    #[test]
    fn reset_makes_no_git_calls_so_it_works_without_a_repo() {
        // reset is a recovery command: it derives its target from config and
        // (in production) shallow-clones the remote. With the source stubbed
        // it must touch git *zero* times — proving it can't depend on cwd,
        // an `origin` remote, or being inside a worktree at all.
        let f = fixture();
        let r = RecordingRunner::new().on(
            |p, a| p == "claude" && a.contains(&"list"),
            CommandOutput::ok("[]"),
        );
        assert!(reset_with(&r, &f.cfg, &both_mkts(), &crate::output::NoopReporter).is_ok());
        assert!(
            !r.lines().iter().any(|l| l.starts_with("git ")),
            "reset must make no git calls, saw: {:?}",
            r.lines()
        );
    }

    #[test]
    fn reset_clone_command_is_shaped_correctly() {
        // The real `RemoteClone` read path: a scripted-success `git clone`
        // plus a pre-populated checkout dir (the fake runner writes no
        // files, so `load_from` is pointed at a dir we populate ourselves).
        let (_d, dir) = tmp();
        std::fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
        std::fs::create_dir_all(dir.join(".agents/plugins")).unwrap();
        std::fs::write(dir.join(".claude-plugin/marketplace.json"), CLAUDE_MKT).unwrap();
        std::fs::write(dir.join(".agents/plugins/marketplace.json"), CODEX_MKT).unwrap();
        let cfg = Config::new("git@github.com:co/agent-mkt.git", true, true);
        let r = RecordingRunner::new().on(
            |p, a| p == "git" && a.contains(&"clone"),
            CommandOutput::ok(""),
        );

        let clone = RemoteClone {
            runner: &r,
            remote: "git@github.com:co/agent-mkt.git",
        };
        let loaded = clone.load_from(&cfg, &dir).unwrap();

        assert_eq!(
            r.lines(),
            vec![format!(
                "git clone --depth 1 -- git@github.com:co/agent-mkt.git {dir}"
            )]
        );
        assert_eq!(loaded.claude.unwrap().plugins, vec!["p1", "p2"]);
        assert_eq!(loaded.codex.unwrap().name, "M");
        // CODEX_MKT declares no `policy.installation` ⇒ every plugin is
        // available-but-not-installed; the advisory must surface them.
        assert_eq!(loaded.codex_unactivated, vec!["p1", "p2"]);
    }

    #[test]
    fn reset_surfaces_a_clear_error_when_clone_fails() {
        let (_d, dir) = tmp();
        let cfg = Config::new("git@github.com:co/agent-mkt.git", true, true);
        let r = RecordingRunner::new().on(
            |p, a| p == "git" && a.contains(&"clone"),
            CommandOutput::fail(128, "fatal: Could not read from remote repository"),
        );
        let clone = RemoteClone {
            runner: &r,
            remote: "git@github.com:co/agent-mkt.git",
        };
        let err = clone.load_from(&cfg, &dir).unwrap_err().to_string();
        assert!(err.contains("git clone"), "{err}");
        assert!(err.contains("git@github.com:co/agent-mkt.git"), "{err}");
        assert!(
            err.contains("Could not read from remote repository"),
            "{err}"
        );
    }

    #[test]
    fn reset_rejects_an_unrecognizable_remote_before_cloning() {
        // A bad remote must fail at validation, before any temp dir or clone.
        let mut f = fixture();
        f.cfg = Config::new("not-a-remote", true, true);
        let r = RecordingRunner::new();
        let err = reset(&r, &f.cfg, &crate::output::NoopReporter)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a recognizable git remote"), "{err}");
        assert!(
            !r.lines().iter().any(|l| l.contains("git clone")),
            "no clone may be attempted for a bad remote: {:?}",
            r.lines()
        );
    }

    #[test]
    fn init_hard_errors_outside_a_git_repo() {
        let (_d, repo) = tmp();
        let r = RecordingRunner::new().on_arg(
            "git",
            "--show-toplevel",
            CommandOutput::fail(128, "nope"),
        );
        let err = init(
            &r,
            &repo,
            Utf8Path::new("/cfg.toml"),
            false,
            TargetSelection::Auto,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not inside a git repository"), "{err}");
    }

    #[test]
    fn init_refuses_to_overwrite_existing_config_without_force() {
        let (_d, repo) = tmp();
        let cfg_path = repo.join("config.toml");
        std::fs::write(&cfg_path, "stale").unwrap();
        let r = RecordingRunner::new()
            .on_arg("git", "--show-toplevel", CommandOutput::ok(repo.as_str()))
            .on_arg(
                "git",
                "get-url",
                CommandOutput::ok("git@github.com:co/r.git"),
            );
        let err = init(&r, &repo, &cfg_path, false, TargetSelection::Auto)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--force"), "{err}");
        assert_eq!(std::fs::read_to_string(&cfg_path).unwrap(), "stale");
    }

    /// Scripts just `git` for a clean `init`. A `<cli> --version` probe is
    /// left unscripted so the runner's unmatched-ok default makes the CLI
    /// "present"; a test scripts an explicit failing rule to make one absent.
    fn init_runner(repo: &Utf8Path, origin: &str) -> RecordingRunner {
        RecordingRunner::new()
            .on_arg("git", "--show-toplevel", CommandOutput::ok(repo.as_str()))
            .on_arg("git", "get-url", CommandOutput::ok(origin))
            .on_arg("git", "symbolic-ref", CommandOutput::ok("origin/main"))
    }

    #[test]
    fn init_auto_enables_only_runtimes_that_ship_a_marketplace_file() {
        let (_d, repo) = tmp();
        std::fs::create_dir_all(repo.join(".claude-plugin")).unwrap();
        std::fs::write(repo.join(".claude-plugin/marketplace.json"), "{}").unwrap();
        let cfg_path = repo.join("sub/config.toml");
        // Both CLIs present, but only the Claude marketplace file exists.
        let r = init_runner(&repo, "git@github.com:co/agent-mkt.git");

        let report = init(&r, &repo, &cfg_path, false, TargetSelection::Auto).unwrap();

        assert_eq!(report.remote, "git@github.com:co/agent-mkt.git");
        assert!(report.claude.enabled);
        assert!(!report.codex.enabled);
        assert_eq!(
            report.codex.skip_reason.as_deref(),
            Some("no .agents/plugins/marketplace.json")
        );
        assert_eq!(
            Config::load(&cfg_path).unwrap(),
            Config::new("git@github.com:co/agent-mkt.git", true, false)
        );
    }

    #[test]
    fn init_auto_skips_a_runtime_not_on_path() {
        let (_d, repo) = tmp();
        for f in [".claude-plugin", ".agents/plugins"] {
            std::fs::create_dir_all(repo.join(f)).unwrap();
        }
        std::fs::write(repo.join(".claude-plugin/marketplace.json"), "{}").unwrap();
        std::fs::write(repo.join(".agents/plugins/marketplace.json"), "{}").unwrap();
        let cfg_path = repo.join("config.toml");
        // Codex CLI is absent even though the repo ships its file.
        let r = init_runner(&repo, "git@github.com:co/r.git").on_arg(
            "codex",
            "--version",
            CommandOutput::fail(127, "command not found"),
        );

        let report = init(&r, &repo, &cfg_path, false, TargetSelection::Auto).unwrap();

        assert!(report.claude.enabled);
        assert!(!report.codex.enabled);
        assert_eq!(report.codex.skip_reason.as_deref(), Some("not on PATH"));
    }

    #[test]
    fn init_codex_only_overrides_detection_even_with_no_file_or_cli() {
        let (_d, repo) = tmp();
        let cfg_path = repo.join("config.toml");
        // No marketplace files, codex CLI absent — the override still wins,
        // surfacing the missing file as a warning (file_present == false).
        let r = init_runner(&repo, "git@github.com:co/r.git").on_arg(
            "codex",
            "--version",
            CommandOutput::fail(127, "command not found"),
        );

        let report = init(&r, &repo, &cfg_path, false, TargetSelection::CodexOnly).unwrap();

        assert!(report.codex.enabled);
        assert!(!report.codex.file_present);
        assert!(!report.claude.enabled);
        assert_eq!(
            report.claude.skip_reason.as_deref(),
            Some("excluded by --codex-only")
        );
        assert_eq!(
            Config::load(&cfg_path).unwrap(),
            Config::new("git@github.com:co/r.git", false, true)
        );
    }

    #[test]
    fn init_errs_when_auto_detection_finds_no_runtime() {
        let (_d, repo) = tmp();
        let cfg_path = repo.join("config.toml");
        // No marketplace files at all → both targets skip → hard error.
        let r = init_runner(&repo, "git@github.com:co/r.git");
        let err = init(&r, &repo, &cfg_path, false, TargetSelection::Auto)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no runtimes to manage"), "{err}");
        assert!(!cfg_path.exists(), "no config may be written on this error");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_runner_captures_status_and_stdout() {
        let out = RealCommandRunner.run("git", &["--version"], None).unwrap();
        assert!(out.success());
        assert!(out.stdout.contains("git"), "stdout was: {:?}", out.stdout);
    }

    #[test]
    fn real_runner_reports_nonzero_exit_without_erroring() {
        let out = RealCommandRunner
            .run(
                "git",
                &["rev-parse", "--show-toplevel"],
                Some(Utf8Path::new("/")),
            )
            .unwrap();
        assert!(!out.success());
    }
}
