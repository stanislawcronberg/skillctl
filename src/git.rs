//! Git remote canonicalization and live repo inspection.

use crate::command::CommandRunner;
use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};

/// Branch / commit / dirty — everything `sync` and `reset` need to announce
/// the target. Deliberately excludes `origin`: `reset` is a recovery command
/// that must work in a worktree with no `origin` remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkState {
    pub branch: String,
    pub commit: String,
    pub dirty: bool,
}

/// Live snapshot of the worktree skillctl is operating on (adds `origin` on
/// top of [`WorkState`], for `status`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoState {
    pub root: Utf8PathBuf,
    pub branch: String,
    pub commit: String,
    pub dirty: bool,
    pub origin_url: String,
}

fn git(runner: &dyn CommandRunner, cwd: &Utf8Path, args: &[&str], what: &str) -> Result<String> {
    let out = runner.run("git", args, Some(cwd))?;
    if !out.success() {
        bail!("{what} failed: {}", out.stderr.trim());
    }
    Ok(out.stdout.trim().to_string())
}

/// Absolute repo root for `cwd`, or a hard error when `cwd` is not inside a
/// Git worktree (`init`/`sync`/`status` all refuse to proceed without this).
pub fn repo_root(runner: &dyn CommandRunner, cwd: &Utf8Path) -> Result<Utf8PathBuf> {
    let out = runner.run("git", &["rev-parse", "--show-toplevel"], Some(cwd))?;
    if !out.success() {
        bail!("not inside a git repository (cwd: {cwd})");
    }
    Ok(Utf8PathBuf::from(out.stdout.trim()))
}

/// `origin` URL, verbatim. Errors when the repo has no `origin` remote.
pub fn origin_url(runner: &dyn CommandRunner, repo: &Utf8Path) -> Result<String> {
    git(
        runner,
        repo,
        &["remote", "get-url", "origin"],
        "git remote get-url origin",
    )
    .context("repository has no `origin` remote")
}

/// Default branch via `origin/HEAD`, falling back to `main` when the symbolic
/// ref is absent (fresh clone / detached). Display-only; never persisted.
pub fn default_branch(runner: &dyn CommandRunner, repo: &Utf8Path) -> String {
    runner
        .run(
            "git",
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
            Some(repo),
        )
        .ok()
        .filter(|o| o.success())
        .map(|o| o.stdout.trim().to_string())
        .map(|s| s.rsplit('/').next().unwrap_or(&s).to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".to_string())
}

/// Branch / short-commit / dirty — no `origin` lookup.
pub fn work_state(runner: &dyn CommandRunner, repo: &Utf8Path) -> Result<WorkState> {
    let branch = git(
        runner,
        repo,
        &["rev-parse", "--abbrev-ref", "HEAD"],
        "git rev-parse --abbrev-ref HEAD",
    )?;
    let commit = git(
        runner,
        repo,
        &["rev-parse", "--short", "HEAD"],
        "git rev-parse --short HEAD",
    )?;
    let porcelain = git(
        runner,
        repo,
        &["status", "--porcelain"],
        "git status --porcelain",
    )?;
    Ok(WorkState {
        branch,
        commit,
        dirty: !porcelain.is_empty(),
    })
}

/// [`work_state`] plus `origin`, in one shot, for `status`.
pub fn state(runner: &dyn CommandRunner, repo: &Utf8Path) -> Result<RepoState> {
    let w = work_state(runner, repo)?;
    let origin = origin_url(runner, repo)?;
    Ok(RepoState {
        root: repo.to_path_buf(),
        branch: w.branch,
        commit: w.commit,
        dirty: w.dirty,
        origin_url: origin,
    })
}

#[cfg(test)]
mod git_ops_tests {
    use super::*;
    use crate::command::fake::RecordingRunner;
    use crate::command::CommandOutput;

    fn cwd() -> &'static Utf8Path {
        Utf8Path::new("/work/wt")
    }

    #[test]
    fn repo_root_trims_toplevel_output() {
        let r = RecordingRunner::new().on_arg(
            "git",
            "--show-toplevel",
            CommandOutput::ok("/work/wt\n"),
        );
        assert_eq!(repo_root(&r, cwd()).unwrap(), "/work/wt");
    }

    #[test]
    fn repo_root_hard_errors_outside_a_git_repo() {
        let r = RecordingRunner::new().on_arg(
            "git",
            "--show-toplevel",
            CommandOutput::fail(128, "fatal: not a git repository"),
        );
        let err = repo_root(&r, cwd()).unwrap_err().to_string();
        assert!(err.contains("not inside a git repository"), "{err}");
    }

    #[test]
    fn state_reports_dirty_when_porcelain_is_nonempty() {
        let r = RecordingRunner::new()
            .on_arg("git", "--abbrev-ref", CommandOutput::ok("pr-123\n"))
            .on_arg("git", "--short", CommandOutput::ok("abc1234\n"))
            .on_arg("git", "--porcelain", CommandOutput::ok(" M file.rs\n"))
            .on_arg(
                "git",
                "get-url",
                CommandOutput::ok("git@github.com:co/repo.git\n"),
            );
        let s = state(&r, cwd()).unwrap();
        assert_eq!(s.branch, "pr-123");
        assert_eq!(s.commit, "abc1234");
        assert!(s.dirty);
        assert_eq!(s.origin_url, "git@github.com:co/repo.git");
    }

    #[test]
    fn state_reports_clean_when_porcelain_is_empty() {
        let r = RecordingRunner::new()
            .on_arg("git", "--abbrev-ref", CommandOutput::ok("main"))
            .on_arg("git", "--short", CommandOutput::ok("deadbee"))
            .on_arg("git", "--porcelain", CommandOutput::ok("\n"))
            .on_arg("git", "get-url", CommandOutput::ok("https://x/y.git"));
        assert!(!state(&r, cwd()).unwrap().dirty);
    }

    #[test]
    fn default_branch_falls_back_to_main_without_origin_head() {
        let r = RecordingRunner::new().on_arg(
            "git",
            "symbolic-ref",
            CommandOutput::fail(128, "fatal: ref not a symbolic ref"),
        );
        assert_eq!(default_branch(&r, cwd()), "main");
    }

    #[test]
    fn default_branch_strips_remote_prefix() {
        let r = RecordingRunner::new().on_arg(
            "git",
            "symbolic-ref",
            CommandOutput::ok("origin/develop\n"),
        );
        assert_eq!(default_branch(&r, cwd()), "develop");
    }
}

/// Reduce any common Git remote URL form to a canonical `host/owner/repo`
/// match-key (lowercased, no `.git`, no trailing slash). Returns `None` when
/// the URL is not a recognizable host/owner/repo remote.
///
/// This is the key used to decide whether the worktree's `origin` matches the
/// configured `remote`, regardless of scheme (ssh/https/scp-like).
pub fn canonical_remote_key(url: &str) -> Option<String> {
    let url = url.trim();

    // Strip a scheme prefix (`https://`, `ssh://`, `git://`, ...) if present.
    let rest = match url.split_once("://") {
        Some((_scheme, rest)) => rest,
        None => url,
    };

    // Drop any `user@` userinfo (e.g. `git@github.com`).
    let rest = match rest.split_once('@') {
        Some((_user, rest)) => rest,
        None => rest,
    };

    // Split host from path at the first separator. scp-like syntax
    // (`host:owner/repo`) uses `:`; URL syntax (`host/owner/repo`) uses `/`.
    let sep = rest.find([':', '/']).filter(|&i| i + 1 < rest.len())?;
    let (host, path) = (&rest[..sep], &rest[sep + 1..]);

    let host = host.trim();
    let path = path
        .trim_matches('/')
        .strip_suffix(".git")
        .unwrap_or_else(|| path.trim_matches('/'));

    if host.is_empty() || path.is_empty() || !path.contains('/') {
        return None;
    }

    Some(format!("{host}/{path}").to_lowercase())
}

/// The `owner/repo` slug for a remote, used as the `add` source on `reset`
/// (both runtimes accept `owner/repo` and track its default branch).
pub fn owner_repo(url: &str) -> Option<String> {
    let key = canonical_remote_key(url)?; // host/owner/repo
    let mut segs = key.split('/');
    let _host = segs.next()?;
    let owner = segs.next()?;
    let repo = segs.next()?;
    if segs.next().is_some() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_repo_drops_host_and_git_suffix() {
        assert_eq!(
            owner_repo("git@github.com:company/agent-marketplace.git").as_deref(),
            Some("company/agent-marketplace")
        );
        assert_eq!(
            owner_repo("https://github.com/company/agent-marketplace").as_deref(),
            Some("company/agent-marketplace")
        );
    }

    #[test]
    fn equivalent_remote_forms_share_one_key() {
        let key = Some("github.com/company/agent-marketplace".to_string());
        assert_eq!(
            canonical_remote_key("git@github.com:company/agent-marketplace.git"),
            key
        );
        assert_eq!(
            canonical_remote_key("https://github.com/company/agent-marketplace.git"),
            key
        );
        assert_eq!(
            canonical_remote_key("https://github.com/company/agent-marketplace"),
            key
        );
        assert_eq!(
            canonical_remote_key("ssh://git@github.com/company/agent-marketplace.git"),
            key
        );
    }

    #[test]
    fn a_different_repo_yields_a_different_key() {
        assert_ne!(
            canonical_remote_key("git@github.com:company/agent-marketplace.git"),
            canonical_remote_key("git@github.com:company/other-repo.git")
        );
    }
}
