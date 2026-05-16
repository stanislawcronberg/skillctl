//! The single seam between skillctl and the outside world: every `git`,
//! `claude`, and `codex` invocation goes through [`CommandRunner`]. Production
//! uses [`RealCommandRunner`]; tests inject a recording fake so the
//! orchestration logic (ordering, exit-code tolerance, presence checks) is
//! verifiable without touching real global state.

use crate::config::Config;
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

pub trait CommandRunner {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitReport {
    pub repo_root: Utf8PathBuf,
    pub remote: String,
    pub default_branch: String,
    pub claude_file_present: bool,
    pub codex_file_present: bool,
    pub config_path: Utf8PathBuf,
}

/// `skillctl init`: detect the repo, refuse to clobber an existing config
/// unless `force`, and write a default config keyed to `origin`.
pub fn init(
    runner: &dyn CommandRunner,
    cwd: &Utf8Path,
    config_path: &Utf8Path,
    force: bool,
    default_branch_override: Option<&str>,
) -> Result<InitReport> {
    let repo_root = git::repo_root(runner, cwd)?;

    if config_path.exists() && !force {
        bail!(
            "skillctl config already exists at {config_path} — \
             pass --force to overwrite"
        );
    }

    let remote = git::origin_url(runner, &repo_root)?;
    let default_branch = match default_branch_override {
        Some(b) => b.to_string(),
        None => git::default_branch(runner, &repo_root),
    };

    let cfg = Config::default_for(remote.clone());
    let claude_file_present = repo_root
        .join(&cfg.targets.claude.marketplace_file)
        .exists();
    let codex_file_present = repo_root.join(&cfg.targets.codex.marketplace_file).exists();

    cfg.save(config_path)?;

    Ok(InitReport {
        repo_root,
        remote,
        default_branch,
        claude_file_present,
        codex_file_present,
        config_path: config_path.to_path_buf(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    pub repo_root: Utf8PathBuf,
    pub claude_name: Option<String>,
    pub codex_name: Option<String>,
    pub plugins: Vec<String>,
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

/// Everything pre-flight parsed/validated, shared by `sync` and `reset`.
struct Preflight {
    repo_root: Utf8PathBuf,
    claude_mkt: Option<Marketplace>,
    codex_mkt: Option<Marketplace>,
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

    let codex_mkt = if cfg.targets.codex.enabled {
        let (mkt, raw) = read_market(&repo_root, &cfg.targets.codex.marketplace_file)?;
        codex::validate_marketplace_json(&raw)
            .with_context(|| format!("in {}", cfg.targets.codex.marketplace_file))?;
        Some(mkt)
    } else {
        None
    };

    Ok(Preflight {
        repo_root,
        claude_mkt,
        codex_mkt,
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
        for p in &m.plugins {
            let spec = format!("{p}@{}", m.name);
            let inst = runner.run(
                "claude",
                &["plugin", "install", &spec, "--scope", scope],
                None,
            )?;
            if !inst.success() {
                bail!(
                    "`claude plugin install {spec}` failed: {}",
                    inst.stderr.trim()
                );
            }
            reporter.event(Event::Plugin { name: p });
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
    let pre = preflight(runner, cwd, cfg, true)?;
    let repo_root = pre.repo_root;
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
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetReport {
    pub repo_root: Utf8PathBuf,
    pub owner_repo: String,
    pub claude_name: Option<String>,
    pub codex_name: Option<String>,
    pub plugins: Vec<String>,
}

/// `skillctl reset`: point both runtimes back at the configured repo's default
/// branch (`owner/repo`, which both runtimes track at its default branch).
/// Codex-first, like `sync`. Claude's `marketplace remove` orphans its
/// installed plugins, so every plugin is reinstalled afterwards. Recovery
/// command: it does *not* check the worktree's `origin` or run validators.
pub fn reset(
    runner: &dyn CommandRunner,
    cwd: &Utf8Path,
    cfg: &Config,
    reporter: &dyn Reporter,
) -> Result<ResetReport> {
    let repo_root = git::repo_root(runner, cwd)?;
    let owner_repo = git::owner_repo(&cfg.repo.remote).with_context(|| {
        format!(
            "configured remote is not an owner/repo remote: {}",
            cfg.repo.remote
        )
    })?;

    let claude_mkt = cfg
        .targets
        .claude
        .enabled
        .then(|| read_market(&repo_root, &cfg.targets.claude.marketplace_file))
        .transpose()?
        .map(|(m, _)| m);
    let codex_mkt = cfg
        .targets
        .codex
        .enabled
        .then(|| read_market(&repo_root, &cfg.targets.codex.marketplace_file))
        .transpose()?
        .map(|(m, _)| m);

    let st = git::work_state(runner, &repo_root)?;
    reporter.event(Event::Target {
        action: "Resetting",
        target: &owner_repo,
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
        &owner_repo,
        claude_mkt.as_ref(),
        codex_mkt.as_ref(),
        cfg.targets.claude.scope.as_str(),
        reporter,
    )?;

    Ok(ResetReport {
        repo_root,
        owner_repo,
        claude_name,
        codex_name,
        plugins,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub configured_remote: String,
    pub repo: git::RepoState,
    pub remote_matches: bool,
    pub default_branch: String,
    pub claude_name: Option<String>,
    pub codex_name: Option<String>,
    /// Where each runtime currently resolves its marketplace name, if known.
    pub claude_pointed_at: Option<String>,
    pub codex_pointed_at: Option<String>,
}

/// `skillctl status`: a fully live snapshot (no state file). Best-effort —
/// missing marketplace files or an unreadable runtime degrade individual
/// fields to `None` rather than failing the whole command.
pub fn status(
    runner: &dyn CommandRunner,
    cwd: &Utf8Path,
    codex_config: &Utf8Path,
    cfg: &Config,
) -> Result<StatusReport> {
    let repo_root = git::repo_root(runner, cwd)?;
    let repo = git::state(runner, &repo_root)?;
    let default_branch = git::default_branch(runner, &repo_root);

    let remote_matches = {
        let want = git::canonical_remote_key(&cfg.repo.remote);
        let got = git::canonical_remote_key(&repo.origin_url);
        want.is_some() && want == got
    };

    // Best-effort name detection from the worktree's marketplace files.
    let name_of = |rel: &Utf8Path| read_market(&repo_root, rel).ok().map(|(m, _)| m.name);
    let claude_name = cfg
        .targets
        .claude
        .enabled
        .then(|| name_of(&cfg.targets.claude.marketplace_file))
        .flatten();
    let codex_name = cfg
        .targets
        .codex
        .enabled
        .then(|| name_of(&cfg.targets.codex.marketplace_file))
        .flatten();

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
        repo,
        remote_matches,
        default_branch,
        claude_name,
        codex_name,
        claude_pointed_at,
        codex_pointed_at,
    })
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

/// Compare two filesystem paths for the post-sync identity check, tolerating
/// only a trailing-slash difference.
fn same_path(a: &str, b: &str) -> bool {
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
    use std::cell::RefCell;
    use std::collections::VecDeque;

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

    type Matcher = Box<dyn Fn(&str, &[&str]) -> bool>;

    struct Rule {
        pred: Matcher,
        /// Successive responses; the last one is reused once exhausted.
        responses: RefCell<VecDeque<CommandOutput>>,
    }

    pub struct RecordingRunner {
        rules: Vec<Rule>,
        calls: RefCell<Vec<RecordedCall>>,
    }

    impl RecordingRunner {
        pub fn new() -> Self {
            RecordingRunner {
                rules: Vec::new(),
                calls: RefCell::new(Vec::new()),
            }
        }

        /// Script a single response for calls matching `pred(program, args)`.
        pub fn on(
            self,
            pred: impl Fn(&str, &[&str]) -> bool + 'static,
            out: CommandOutput,
        ) -> Self {
            self.on_seq(pred, vec![out])
        }

        /// Script successive responses for matching calls (call #1 → first,
        /// #2 → second, …; the last response repeats once exhausted).
        pub fn on_seq(
            mut self,
            pred: impl Fn(&str, &[&str]) -> bool + 'static,
            outs: Vec<CommandOutput>,
        ) -> Self {
            self.rules.push(Rule {
                pred: Box::new(pred),
                responses: RefCell::new(outs.into()),
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
            self.calls.borrow().iter().map(RecordedCall::line).collect()
        }
    }

    impl CommandRunner for RecordingRunner {
        fn run(
            &self,
            program: &str,
            args: &[&str],
            cwd: Option<&Utf8Path>,
        ) -> Result<CommandOutput> {
            self.calls.borrow_mut().push(RecordedCall {
                program: program.to_string(),
                args: args.iter().map(|s| s.to_string()).collect(),
                cwd: cwd.map(|c| c.to_path_buf()),
            });
            for rule in &self.rules {
                if (rule.pred)(program, args) {
                    let mut q = rule.responses.borrow_mut();
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
        let cfg = Config::default_for("git@github.com:co/agent-mkt.git");
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
        assert_eq!(
            agent_lines(&r.lines()),
            vec![
                format!("claude plugin validate {repo}"),
                "codex plugin marketplace remove M".to_string(),
                format!("codex plugin marketplace add {repo}"),
                "claude plugin marketplace list --json".to_string(),
                "claude plugin marketplace remove M".to_string(),
                format!("claude plugin marketplace add {repo}"),
                "claude plugin install p1@M --scope user".to_string(),
                "claude plugin install p2@M --scope user".to_string(),
                "claude plugin marketplace list --json".to_string(),
            ]
        );
        assert_eq!(report.repo_root, f.repo);
        assert_eq!(report.claude_name.as_deref(), Some("M"));
        assert_eq!(report.codex_name.as_deref(), Some("M"));
        assert_eq!(report.plugins, vec!["p1", "p2"]);
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

        let res = sync(&r, &f.repo, &f.codex_cfg, &f.cfg, &rep);

        assert!(res.is_err(), "sync should fail on plugin 2");
        let trail = rep.lines.borrow();
        // The completed steps remain visible; p2 never got a line.
        assert!(trail.iter().any(|l| l == "Codex   ~ M"), "{trail:?}");
        assert!(trail.iter().any(|l| l == "Claude  ~ M"), "{trail:?}");
        assert!(trail.iter().any(|l| l == "+ p1"), "{trail:?}");
        assert!(!trail.iter().any(|l| l == "+ p2"), "{trail:?}");
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

        assert!(s.remote_matches);
        assert_eq!(s.repo.branch, "pr-123");
        assert!(s.repo.dirty);
        assert_eq!(s.default_branch, "main");
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
        assert!(!s.remote_matches);
    }

    #[test]
    fn reset_points_both_runtimes_at_owner_repo_and_reinstalls() {
        let f = fixture();
        // Claude already has "M" registered → conditional remove must fire.
        let r = RecordingRunner::new()
            .on_arg("git", "--show-toplevel", CommandOutput::ok(f.repo.as_str()))
            .on(
                |p, a| p == "claude" && a.contains(&"list"),
                CommandOutput::ok(r#"[{ "name": "M", "source": "github" }]"#),
            );

        let report = reset(&r, &f.repo, &f.cfg, &crate::output::NoopReporter).unwrap();

        assert_eq!(report.owner_repo, "co/agent-mkt");
        assert_eq!(report.plugins, vec!["p1", "p2"]);
        assert_eq!(
            agent_lines(&r.lines()),
            vec![
                "codex plugin marketplace remove M".to_string(),
                "codex plugin marketplace add co/agent-mkt".to_string(),
                "claude plugin marketplace list --json".to_string(),
                "claude plugin marketplace remove M".to_string(),
                "claude plugin marketplace add co/agent-mkt".to_string(),
                "claude plugin install p1@M --scope user".to_string(),
                "claude plugin install p2@M --scope user".to_string(),
            ]
        );
    }

    #[test]
    fn reset_does_not_check_origin_remote() {
        let f = fixture();
        // `git remote get-url origin` *fails* (no origin remote). reset is a
        // recovery command and must not depend on origin at all — it derives
        // its target from config, and the progress line uses `work_state`.
        let r = RecordingRunner::new()
            .on_arg("git", "--show-toplevel", CommandOutput::ok(f.repo.as_str()))
            .on_arg(
                "git",
                "get-url",
                CommandOutput::fail(2, "error: No such remote 'origin'"),
            )
            .on(
                |p, a| p == "claude" && a.contains(&"list"),
                CommandOutput::ok("[]"),
            );
        assert!(reset(&r, &f.repo, &f.cfg, &crate::output::NoopReporter).is_ok());
    }

    #[test]
    fn init_hard_errors_outside_a_git_repo() {
        let (_d, repo) = tmp();
        let r = RecordingRunner::new().on_arg(
            "git",
            "--show-toplevel",
            CommandOutput::fail(128, "nope"),
        );
        let err = init(&r, &repo, Utf8Path::new("/cfg.toml"), false, None)
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
        let err = init(&r, &repo, &cfg_path, false, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--force"), "{err}");
        assert_eq!(std::fs::read_to_string(&cfg_path).unwrap(), "stale");
    }

    #[test]
    fn init_writes_default_config_keyed_to_origin() {
        let (_d, repo) = tmp();
        std::fs::create_dir_all(repo.join(".claude-plugin")).unwrap();
        std::fs::write(repo.join(".claude-plugin/marketplace.json"), "{}").unwrap();
        let cfg_path = repo.join("sub/config.toml");
        let r = RecordingRunner::new()
            .on_arg("git", "--show-toplevel", CommandOutput::ok(repo.as_str()))
            .on_arg(
                "git",
                "get-url",
                CommandOutput::ok("git@github.com:co/agent-mkt.git"),
            )
            .on_arg("git", "symbolic-ref", CommandOutput::ok("origin/trunk"));

        let report = init(&r, &repo, &cfg_path, false, None).unwrap();

        assert_eq!(report.remote, "git@github.com:co/agent-mkt.git");
        assert_eq!(report.default_branch, "trunk");
        assert!(report.claude_file_present);
        assert!(!report.codex_file_present);
        let saved = Config::load(&cfg_path).unwrap();
        assert_eq!(
            saved,
            Config::default_for("git@github.com:co/agent-mkt.git")
        );
    }

    #[test]
    fn init_default_branch_override_wins() {
        let (_d, repo) = tmp();
        let cfg_path = repo.join("config.toml");
        let r = RecordingRunner::new()
            .on_arg("git", "--show-toplevel", CommandOutput::ok(repo.as_str()))
            .on_arg(
                "git",
                "get-url",
                CommandOutput::ok("git@github.com:co/r.git"),
            )
            .on_arg("git", "symbolic-ref", CommandOutput::ok("origin/main"));
        let report = init(&r, &repo, &cfg_path, false, Some("release")).unwrap();
        assert_eq!(report.default_branch, "release");
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
