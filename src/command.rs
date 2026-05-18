//! The single seam between skillctl and the outside world: every `git`,
//! `claude`, and `codex` invocation goes through [`CommandRunner`]. Production
//! uses [`RealCommandRunner`]; tests inject a recording fake so the
//! orchestration logic (ordering, exit-code tolerance, presence checks) is
//! verifiable without touching real global state.
//!
//! Orchestration here is **uniform over the [`Target`] seam**: it iterates the
//! managed runtimes in split-brain order ([`Config::managed`]) and calls the
//! trait. It never matches on which runtime it is — that lives in the
//! adapters (`targets::{claude,codex}`).

use crate::config::{Config, Runtime};
use crate::git;
use crate::output::{Event, Reporter};
use crate::targets::{self, Target, Validated};
use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use std::collections::BTreeMap;

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
/// scoped threads that fan out Claude's plugin installs.
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
/// file; `Only(r)` is an explicit hard override that ignores detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetSelection {
    Auto,
    Only(Runtime),
}

/// Per-runtime outcome of `init`'s detection, carried to the renderer.
/// `skip_reason` is `Some` iff the runtime was *not* managed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetOutcome {
    pub managed: bool,
    pub file_present: bool,
    pub skip_reason: Option<String>,
}

impl TargetOutcome {
    fn managed(file_present: bool) -> Self {
        Self {
            managed: true,
            file_present,
            skip_reason: None,
        }
    }

    fn skipped(file_present: bool, why: impl Into<String>) -> Self {
        Self {
            managed: false,
            file_present,
            skip_reason: Some(why.into()),
        }
    }

    /// Auto-detection rule: manage a runtime only when its CLI is on `PATH`
    /// *and* its marketplace file is in the repo.
    fn detected(det: targets::Detection, no_file: impl Into<String>) -> Self {
        if !det.on_path {
            Self::skipped(det.file_present, "not on PATH")
        } else if !det.file_present {
            Self::skipped(det.file_present, no_file)
        } else {
            Self::managed(det.file_present)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitReport {
    pub repo_root: Utf8PathBuf,
    pub remote: String,
    pub default_branch: String,
    /// Every runtime, in split-brain order — managed and skipped alike (the
    /// renderer shows the skipped ones' reasons).
    pub outcomes: BTreeMap<Runtime, TargetOutcome>,
    pub config_path: Utf8PathBuf,
}

impl InitReport {
    pub fn outcome(&self, r: Runtime) -> &TargetOutcome {
        &self.outcomes[&r]
    }
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

    // Uniform over `Runtime::ALL` — `init` never names a concrete runtime; the
    // per-runtime facts come from `Runtime`/the detection probe.
    let mut outcomes = BTreeMap::new();
    for rt in Runtime::ALL {
        let det = targets::detect(runner, &repo_root, rt);
        let outcome = match selection {
            TargetSelection::Auto => {
                TargetOutcome::detected(det, format!("no {}", rt.default_marketplace_file()))
            }
            TargetSelection::Only(only) if rt == only => TargetOutcome::managed(det.file_present),
            TargetSelection::Only(only) => TargetOutcome::skipped(
                det.file_present,
                format!("excluded by --{}-only", only.key()),
            ),
        };
        outcomes.insert(rt, outcome);
    }

    if outcomes.values().all(|o| !o.managed) {
        bail!(
            "no runtimes to manage here: neither `claude` nor `codex` is on \
             PATH with its marketplace file in this repo.\n  \
             install a runtime and re-run, or scope explicitly with \
             --claude-only / --codex-only"
        );
    }

    let managed: Vec<Runtime> = outcomes
        .iter()
        .filter(|(_, o)| o.managed)
        .map(|(r, _)| *r)
        .collect();
    Config::new(remote.clone(), managed).save(config_path)?;

    Ok(InitReport {
        repo_root,
        remote,
        default_branch,
        outcomes,
        config_path: config_path.to_path_buf(),
    })
}

/// `sync`/`reset` are no-ops with nothing managed; fail loudly instead of
/// silently doing nothing.
fn ensure_any_target(cfg: &Config) -> Result<()> {
    if cfg.targets.is_empty() {
        bail!(
            "no runtimes managed in config — re-run `skillctl init` \
             (optionally with --claude-only / --codex-only)"
        );
    }
    Ok(())
}

/// Build the managed targets, already in split-brain order ([`Config::managed`]
/// is `Runtime`-ordered, Codex before Claude).
fn managed_targets<'a>(
    cfg: &Config,
    runner: &'a dyn CommandRunner,
    codex_config: &Utf8Path,
) -> Vec<Box<dyn Target + 'a>> {
    cfg.managed()
        .map(|(r, t)| targets::make(r, t, runner, codex_config))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    pub repo_root: Utf8PathBuf,
    /// Marketplace name each managed runtime was pointed at.
    pub names: BTreeMap<Runtime, String>,
    /// Plugins skillctl (re)installed — Claude's set; empty for a Codex-only
    /// sync (Codex has no per-plugin install).
    pub plugins: Vec<String>,
    /// Codex plugins that registered but won't auto-install (advisory; sync
    /// still succeeds). Empty when Codex is unmanaged.
    pub codex_unactivated_plugins: Vec<String>,
}

impl SyncReport {
    /// The marketplace name a runtime was pointed at. Test-only ergonomics —
    /// the renderer reads `plugins`/`codex_unactivated_plugins`, not this.
    #[cfg(test)]
    pub fn name(&self, r: Runtime) -> Option<&str> {
        self.names.get(&r).map(String::as_str)
    }
}

/// `skillctl sync`: point each managed runtime at this worktree and install
/// every plugin. All validation happens before any mutation; runtimes are
/// mutated in split-brain order so an unanticipated rejection can never leave
/// a split brain.
pub fn sync(
    runner: &dyn CommandRunner,
    cwd: &Utf8Path,
    codex_config: &Utf8Path,
    cfg: &Config,
    reporter: &dyn Reporter,
) -> Result<SyncReport> {
    ensure_any_target(cfg)?;
    let repo_root = git::repo_root(runner, cwd)?;

    // Prove `origin` matches the configured remote before anything else.
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

    let targets = managed_targets(cfg, runner, codex_config);

    // Pre-flight: authoritative validation of every managed runtime, before
    // any mutation. A failure here changes nothing.
    let mut validated: BTreeMap<Runtime, Validated> = BTreeMap::new();
    for t in &targets {
        validated.insert(t.runtime(), t.validate(&repo_root)?);
    }

    // Announce the target now — after validation, before the first mutation —
    // so the worktree is on screen even if a later step fails.
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

    // Mutate, in split-brain order.
    let mut plugins = Vec::new();
    for t in &targets {
        let mkt = &validated[&t.runtime()].marketplace;
        plugins.extend(t.apply(repo_root.as_str(), mkt, reporter)?);
    }

    // Post-sync assertion (loud): every runtime must now resolve the
    // marketplace name to *this* worktree, else the name-identity contract
    // skillctl relies on is broken.
    for t in &targets {
        let name = &validated[&t.runtime()].marketplace.name;
        let got = t.pointed_at(name)?.with_context(|| {
            format!(
                "name-identity contract broken ({}): marketplace \
                 \"{name}\" is not registered after sync",
                t.runtime().label()
            )
        })?;
        if !same_path(&got, repo_root.as_str()) {
            bail!(
                "name-identity contract broken ({}): \"{name}\" points \
                 at {got}, expected {repo_root}",
                t.runtime().label()
            );
        }
    }

    Ok(SyncReport {
        repo_root,
        names: names_of(&validated),
        plugins,
        codex_unactivated_plugins: advisories_of(&validated),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetReport {
    pub source: String,
    pub plugins: Vec<String>,
    pub codex_unactivated_plugins: Vec<String>,
}

/// Marketplace name per runtime, from the pre-flight results.
fn names_of(v: &BTreeMap<Runtime, Validated>) -> BTreeMap<Runtime, String> {
    v.iter()
        .map(|(r, val)| (*r, val.marketplace.name.clone()))
        .collect()
}

/// All post-mutation advisories (only Codex produces any). Aggregated without
/// naming a runtime — orchestration stays uniform over the seam.
fn advisories_of(v: &BTreeMap<Runtime, Validated>) -> Vec<String> {
    v.values().flat_map(|val| val.advisories.clone()).collect()
}

/// Source of the default-branch checkout `reset` reads. Production
/// shallow-clones the configured remote into a temp dir (removed after the
/// per-target reads); tests stub it so the clone + filesystem step never
/// needs real git or a network.
trait CheckoutSource {
    /// Run `read` with a directory holding the remote's default-branch
    /// checkout, returning whatever it produced.
    fn with_checkout(
        &self,
        read: &mut dyn FnMut(&Utf8Path) -> Result<BTreeMap<Runtime, Validated>>,
    ) -> Result<BTreeMap<Runtime, Validated>>;
}

/// Production [`CheckoutSource`]: `git clone --depth 1 -- <remote> <tmp>` (no
/// `--branch` ⇒ the remote's default branch; host-agnostic; ssh or https),
/// then read every target's marketplace out of the checkout. The `TempDir` is
/// removed on drop, *after* the reads.
struct RemoteClone<'a> {
    runner: &'a dyn CommandRunner,
    remote: &'a str,
}

impl RemoteClone<'_> {
    /// `git clone` into `dir`, then run `read` against it. Split from
    /// [`CheckoutSource::with_checkout`] so a test can drive this read path
    /// against a pre-populated dir with a scripted-success fake `git clone`.
    fn clone_then<T>(
        &self,
        dir: &Utf8Path,
        read: impl FnOnce(&Utf8Path) -> Result<T>,
    ) -> Result<T> {
        let out = self.runner.run(
            "git",
            &["clone", "--depth", "1", "--", self.remote, dir.as_str()],
            None,
        )?;
        if !out.success() {
            bail!(
                "git clone of the configured remote failed (could not fetch {}): {}",
                self.remote,
                out.stderr.trim()
            );
        }
        read(dir)
    }
}

impl CheckoutSource for RemoteClone<'_> {
    fn with_checkout(
        &self,
        read: &mut dyn FnMut(&Utf8Path) -> Result<BTreeMap<Runtime, Validated>>,
    ) -> Result<BTreeMap<Runtime, Validated>> {
        let tmp = tempfile::Builder::new()
            .prefix("skillctl-reset-")
            .tempdir()
            .context("creating a temp dir for the shallow clone")?;
        let dir = Utf8Path::from_path(tmp.path()).context("temp dir path is not valid UTF-8")?;
        let loaded = self.clone_then(dir, |d| read(d));
        drop(tmp); // remove the clone now that every file is read
        loaded
    }
}

/// `skillctl reset`: snap every managed runtime back to the configured
/// marketplace's default branch. It shallow-clones the configured remote (it
/// never reads the local worktree), so it works from *any* directory and can
/// never reinstall an uncommitted local plugin set. Split-brain order, like
/// `sync`. A recovery command: no `origin` check, **no authoritative
/// validators** (it only structurally reads + the Codex advisory), no git
/// worktree required.
pub fn reset(
    runner: &dyn CommandRunner,
    codex_config: &Utf8Path,
    cfg: &Config,
    reporter: &dyn Reporter,
) -> Result<ResetReport> {
    let src = RemoteClone {
        runner,
        remote: &cfg.repo.remote,
    };
    reset_with(runner, codex_config, cfg, &src, reporter)
}

/// `reset` with the checkout source injected, so tests exercise the
/// orchestration without real git or a network (see `StubCheckout`).
fn reset_with(
    runner: &dyn CommandRunner,
    codex_config: &Utf8Path,
    cfg: &Config,
    source: &dyn CheckoutSource,
    reporter: &dyn Reporter,
) -> Result<ResetReport> {
    ensure_any_target(cfg)?;
    // Validate the remote shape up-front (single source of the bad-remote
    // message) — before any temp dir or `git clone` is attempted.
    let url = git::marketplace_source(&cfg.repo.remote).with_context(|| {
        format!(
            "configured remote is not a recognizable git remote: {}",
            cfg.repo.remote
        )
    })?;

    let targets = managed_targets(cfg, runner, codex_config);

    let validated = source.with_checkout(&mut |dir| {
        let mut m = BTreeMap::new();
        for t in &targets {
            m.insert(t.runtime(), t.read(dir)?);
        }
        Ok(m)
    })?;

    reporter.event(Event::ResetTarget { source: &url });

    let mut plugins = Vec::new();
    for t in &targets {
        let mkt = &validated[&t.runtime()].marketplace;
        plugins.extend(t.apply(&url, mkt, reporter)?);
    }

    Ok(ResetReport {
        source: url,
        plugins,
        codex_unactivated_plugins: advisories_of(&validated),
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

/// Where one runtime stands: the marketplace name skillctl associates with it
/// and where that name currently resolves. Both best-effort (`None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetStatus {
    pub name: Option<String>,
    pub pointed_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub configured_remote: String,
    /// `None` ⇒ not inside a git repo; worktree/origin/match rows are skipped.
    pub snapshot: Option<RepoSnapshot>,
    /// Managed runtimes only (mirrors `Config`); an absent runtime renders as
    /// `(not managed)`.
    pub targets: BTreeMap<Runtime, TargetStatus>,
}

impl StatusReport {
    pub fn target(&self, r: Runtime) -> Option<&TargetStatus> {
        self.targets.get(&r)
    }
    /// Test-only ergonomics; the renderer goes through [`target`](Self::target).
    #[cfg(test)]
    pub fn name(&self, r: Runtime) -> Option<&str> {
        self.targets.get(&r).and_then(|t| t.name.as_deref())
    }
    #[cfg(test)]
    pub fn pointed_at(&self, r: Runtime) -> Option<&str> {
        self.targets.get(&r).and_then(|t| t.pointed_at.as_deref())
    }
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
    // in-repo `git::state` failure (e.g. no `origin`) still propagates.
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

    let targets = managed_targets(cfg, runner, codex_config);
    let mut tmap = BTreeMap::new();
    for t in &targets {
        // In a repo: the name from the worktree's marketplace file. Outside a
        // repo there is no file, so match a *registered* marketplace whose
        // source resolves to the configured remote.
        let name = match &snapshot {
            Some(snap) => t.marketplace_name(&snap.repo.root),
            None => t.registered_name_for(&cfg.repo.remote).ok().flatten(),
        };
        let pointed_at = name.as_ref().and_then(|n| t.pointed_at(n).ok().flatten());
        tmap.insert(t.runtime(), TargetStatus { name, pointed_at });
    }

    Ok(StatusReport {
        configured_remote: cfg.repo.remote.clone(),
        snapshot,
        targets: tmap,
    })
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
        let cfg = Config::new(
            "git@github.com:co/agent-mkt.git",
            [Runtime::Codex, Runtime::Claude],
        );
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

    // The Claude plugin-install loop fans out across scoped threads, so the
    // *relative order of the install calls is nondeterministic*. These helpers
    // pin everything that is still deterministic (the split-brain backbone,
    // presence gating, the post-assert `list`) while asserting the installs by
    // set membership plus a bracket invariant.

    /// Agent calls with `plugin install` removed — order is still fully
    /// deterministic and pins the Codex-before-Claude ordering.
    fn backbone(lines: &[String]) -> Vec<String> {
        agent_lines(lines)
            .into_iter()
            .filter(|l| !l.contains("plugin install"))
            .collect()
    }

    /// The set of `plugin install` calls (sequence is nondeterministic).
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
        // Deterministic backbone: Claude's authoritative validator (pre-flight,
        // Codex's is silent), then Codex fully before Claude (split-brain
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
        assert_eq!(install_count(&lines), 2, "each plugin installed once");
        assert_eq!(
            installs(&lines),
            HashSet::from([
                "claude plugin install p1@M --scope user".to_string(),
                "claude plugin install p2@M --scope user".to_string(),
            ])
        );
        assert_installs_bracketed(
            &lines,
            &format!("claude plugin marketplace add {repo}"),
            Some("claude plugin marketplace list --json"),
        );
        assert_eq!(report.repo_root, f.repo);
        assert_eq!(report.name(Runtime::Claude), Some("M"));
        assert_eq!(report.name(Runtime::Codex), Some("M"));
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
        assert_eq!(report.name(Runtime::Codex), Some("M"));
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

        assert_eq!(report.name(Runtime::Claude), Some("M"));
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
        assert!(
            err.contains("p2@M"),
            "error should name the failed plugin: {err}"
        );
        assert!(
            err.contains("boom"),
            "error should include the install stderr: {err}"
        );

        let trail = rep.lines.lock().unwrap();
        assert!(trail.iter().any(|l| l == "Codex   ~ M"), "{trail:?}");
        assert!(trail.iter().any(|l| l == "Claude  ~ M"), "{trail:?}");
        assert!(trail.iter().any(|l| l == "+ p1"), "{trail:?}");
        assert!(!trail.iter().any(|l| l == "+ p2"), "{trail:?}");
    }

    #[test]
    fn sync_runs_all_installs_even_when_one_fails() {
        let f = fixture();
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
        assert_eq!(s.name(Runtime::Claude), Some("M"));
        assert_eq!(s.name(Runtime::Codex), Some("M"));
        assert_eq!(s.pointed_at(Runtime::Claude), Some(f.repo.as_str()));
        assert_eq!(s.pointed_at(Runtime::Codex), Some(f.repo.as_str()));
    }

    #[test]
    fn status_flags_a_remote_mismatch() {
        let f = fixture();
        let r = status_runner(&f.repo, "git@github.com:other/unrelated.git");
        let s = status(&r, &f.repo, &f.codex_cfg, &f.cfg).unwrap();
        assert!(!s.snapshot.as_ref().expect("in a repo").remote_matches);
    }

    /// Stub [`CheckoutSource`]: hands `reset_with` a pre-populated directory
    /// so the shallow-clone + filesystem step never needs real git.
    struct StubCheckout {
        dir: Utf8PathBuf,
    }
    impl CheckoutSource for StubCheckout {
        fn with_checkout(
            &self,
            read: &mut dyn FnMut(&Utf8Path) -> Result<BTreeMap<Runtime, Validated>>,
        ) -> Result<BTreeMap<Runtime, Validated>> {
            read(&self.dir)
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
        assert_eq!(s.name(Runtime::Claude), Some("M"));
        assert_eq!(s.name(Runtime::Codex), Some("M"));
        assert_eq!(s.pointed_at(Runtime::Claude), Some("co/agent-mkt"));
        assert_eq!(
            s.pointed_at(Runtime::Codex),
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
        let src = StubCheckout {
            dir: f.repo.clone(),
        };

        let report =
            reset_with(&r, &f.codex_cfg, &f.cfg, &src, &crate::output::NoopReporter).unwrap();

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
        assert_installs_bracketed(
            &lines,
            "claude plugin marketplace add git@github.com:co/agent-mkt.git",
            None,
        );
    }

    #[test]
    fn reset_accepts_a_nested_gitlab_remote() {
        // A nested GitLab group/subgroup/repo remote — passed through whole.
        let url = "git@gitlab.com:company/team/agent-marketplace.git";
        let mut f = fixture();
        f.cfg = Config::new(url, [Runtime::Codex, Runtime::Claude]);
        let r = RecordingRunner::new().on(
            |p, a| p == "claude" && a.contains(&"list"),
            CommandOutput::ok("[]"),
        );
        let src = StubCheckout {
            dir: f.repo.clone(),
        };

        let report =
            reset_with(&r, &f.codex_cfg, &f.cfg, &src, &crate::output::NoopReporter).unwrap();

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
        // reset is a recovery command: with the checkout stubbed it must
        // touch git *zero* times — proving it can't depend on cwd, an
        // `origin` remote, or being inside a worktree at all.
        let f = fixture();
        let r = RecordingRunner::new().on(
            |p, a| p == "claude" && a.contains(&"list"),
            CommandOutput::ok("[]"),
        );
        let src = StubCheckout {
            dir: f.repo.clone(),
        };
        assert!(reset_with(&r, &f.codex_cfg, &f.cfg, &src, &crate::output::NoopReporter).is_ok());
        assert!(
            !r.lines().iter().any(|l| l.starts_with("git ")),
            "reset must make no git calls, saw: {:?}",
            r.lines()
        );
    }

    #[test]
    fn reset_clone_command_is_shaped_correctly() {
        // The real `RemoteClone` read path: a scripted-success `git clone`
        // plus a pre-populated checkout dir.
        let (_d, dir) = tmp();
        std::fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
        std::fs::create_dir_all(dir.join(".agents/plugins")).unwrap();
        std::fs::write(dir.join(".claude-plugin/marketplace.json"), CLAUDE_MKT).unwrap();
        std::fs::write(dir.join(".agents/plugins/marketplace.json"), CODEX_MKT).unwrap();
        let cfg = Config::new(
            "git@github.com:co/agent-mkt.git",
            [Runtime::Codex, Runtime::Claude],
        );
        let r = RecordingRunner::new().on(
            |p, a| p == "git" && a.contains(&"clone"),
            CommandOutput::ok(""),
        );
        let codex_cfg = Utf8Path::new("/no/codex.toml");
        let targets = managed_targets(&cfg, &r, codex_cfg);

        let clone = RemoteClone {
            runner: &r,
            remote: "git@github.com:co/agent-mkt.git",
        };
        let loaded = clone
            .clone_then(&dir, |d| {
                let mut m = BTreeMap::new();
                for t in &targets {
                    m.insert(t.runtime(), t.read(d)?);
                }
                Ok(m)
            })
            .unwrap();

        assert_eq!(
            r.lines(),
            vec![format!(
                "git clone --depth 1 -- git@github.com:co/agent-mkt.git {dir}"
            )]
        );
        assert_eq!(
            loaded[&Runtime::Claude].marketplace.plugins,
            vec!["p1", "p2"]
        );
        assert_eq!(loaded[&Runtime::Codex].marketplace.name, "M");
        // CODEX_MKT declares no `policy.installation` ⇒ every plugin is
        // available-but-not-installed; the advisory must surface them.
        assert_eq!(
            loaded[&Runtime::Codex].advisories,
            vec!["p1".to_string(), "p2".to_string()]
        );
    }

    #[test]
    fn reset_surfaces_a_clear_error_when_clone_fails() {
        let (_d, dir) = tmp();
        let r = RecordingRunner::new().on(
            |p, a| p == "git" && a.contains(&"clone"),
            CommandOutput::fail(128, "fatal: Could not read from remote repository"),
        );
        let clone = RemoteClone {
            runner: &r,
            remote: "git@github.com:co/agent-mkt.git",
        };
        let err = clone
            .clone_then(&dir, |_d| -> Result<()> { Ok(()) })
            .unwrap_err()
            .to_string();
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
        f.cfg = Config::new("not-a-remote", [Runtime::Codex, Runtime::Claude]);
        let r = RecordingRunner::new();
        let err = reset(&r, &f.codex_cfg, &f.cfg, &crate::output::NoopReporter)
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
        assert!(report.outcome(Runtime::Claude).managed);
        assert!(!report.outcome(Runtime::Codex).managed);
        assert_eq!(
            report.outcome(Runtime::Codex).skip_reason.as_deref(),
            Some("no .agents/plugins/marketplace.json")
        );
        assert_eq!(
            Config::load(&cfg_path).unwrap(),
            Config::new("git@github.com:co/agent-mkt.git", [Runtime::Claude])
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

        assert!(report.outcome(Runtime::Claude).managed);
        assert!(!report.outcome(Runtime::Codex).managed);
        assert_eq!(
            report.outcome(Runtime::Codex).skip_reason.as_deref(),
            Some("not on PATH")
        );
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

        let report = init(
            &r,
            &repo,
            &cfg_path,
            false,
            TargetSelection::Only(Runtime::Codex),
        )
        .unwrap();

        assert!(report.outcome(Runtime::Codex).managed);
        assert!(!report.outcome(Runtime::Codex).file_present);
        assert!(!report.outcome(Runtime::Claude).managed);
        assert_eq!(
            report.outcome(Runtime::Claude).skip_reason.as_deref(),
            Some("excluded by --codex-only")
        );
        assert_eq!(
            Config::load(&cfg_path).unwrap(),
            Config::new("git@github.com:co/r.git", [Runtime::Codex])
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
