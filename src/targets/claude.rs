//! Claude Code target: marketplace add/remove, plugin install, list read-back.
//!
//! Claude's quirks, all local to this adapter: `marketplace remove` is
//! non-idempotent so it is gated on a presence check; it orphans installed
//! plugins so every plugin is (re)installed afterwards; the installs are
//! independent and additive so they fan out across a bounded thread pool.

use super::{Marketplace, Target, Validated};
use crate::command::CommandRunner;
use crate::config::Runtime;
use crate::git;
use crate::output::{Event, Reporter};
use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;

/// Claude installs at this scope. Runtime-specific, so it is the adapter's
/// constant — never config (Codex has no analogue).
const SCOPE: &str = "user";

/// Each `claude plugin install` is independent and additive; fan them out
/// across a bounded pool rather than paying the CLI cold-start serially.
const MAX_INSTALL_CONCURRENCY: usize = 4;

pub struct ClaudeTarget<'a> {
    pub runner: &'a dyn CommandRunner,
    pub marketplace_file: Utf8PathBuf,
}

/// One entry of `claude plugin marketplace list --json`.
///
/// For a *local* marketplace, `install_location` is the live worktree path
/// (Claude makes no copy at registration time). For a *github* source,
/// `source == "github"` and `repo == "owner/repo"`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ClaudeEntry {
    pub name: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(rename = "installLocation", default)]
    pub install_location: Option<String>,
}

pub fn parse_marketplace_list(json: &str) -> Result<Vec<ClaudeEntry>> {
    serde_json::from_str(json).context("parsing `claude plugin marketplace list --json`")
}

/// The currently-pointed-at location for an entry: the worktree path for a
/// local marketplace, else the `owner/repo` for a github source.
pub fn entry_source(e: &ClaudeEntry) -> Option<String> {
    e.install_location
        .clone()
        .or_else(|| e.repo.clone())
        .filter(|s| !s.is_empty())
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

impl ClaudeTarget<'_> {
    fn list(&self) -> Result<Vec<ClaudeEntry>> {
        let out = self
            .runner
            .run("claude", &["plugin", "marketplace", "list", "--json"], None)?;
        if !out.success() {
            bail!(
                "`claude plugin marketplace list --json` failed: {}",
                out.stderr.trim()
            );
        }
        parse_marketplace_list(&out.stdout)
    }

    fn present(&self, name: &str) -> Result<bool> {
        Ok(self.list()?.iter().any(|e| e.name == name))
    }
}

impl Target for ClaudeTarget<'_> {
    fn runtime(&self) -> Runtime {
        Runtime::Claude
    }

    fn read(&self, dir: &Utf8Path) -> Result<Validated> {
        let path = dir.join(&self.marketplace_file);
        let raw = std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
        let marketplace = Marketplace::parse(&raw).with_context(|| format!("in {path}"))?;
        // Claude has no post-install advisory analogue.
        Ok(Validated {
            marketplace,
            advisories: Vec::new(),
        })
    }

    fn validate(&self, dir: &Utf8Path) -> Result<Validated> {
        let validated = self.read(dir)?;
        // Claude ships an authoritative validator — defer to it entirely.
        let out = self
            .runner
            .run("claude", &["plugin", "validate", dir.as_str()], None)?;
        if !out.success() {
            let detail = if out.stdout.trim().is_empty() {
                out.stderr.trim()
            } else {
                out.stdout.trim()
            };
            bail!("`claude plugin validate` rejected the marketplace:\n{detail}");
        }
        Ok(validated)
    }

    fn apply(
        &self,
        source: &str,
        mkt: &Marketplace,
        reporter: &dyn Reporter,
    ) -> Result<Vec<String>> {
        // `remove` is non-idempotent → gate on a presence check.
        if self.present(&mkt.name)? {
            let rm = self.runner.run(
                "claude",
                &["plugin", "marketplace", "remove", &mkt.name],
                None,
            )?;
            if !rm.success() {
                bail!(
                    "`claude plugin marketplace remove {}` failed: {}",
                    mkt.name,
                    rm.stderr.trim()
                );
            }
        }
        let add = self
            .runner
            .run("claude", &["plugin", "marketplace", "add", source], None)?;
        if !add.success() {
            bail!(
                "`claude plugin marketplace add` failed: {}",
                add.stderr.trim()
            );
        }
        reporter.event(Event::Marketplace {
            runtime: Runtime::Claude.label(),
            name: &mkt.name,
        });

        // `remove` orphaned the installed plugins, so (re)install every one.
        // Each install is independent and additive; fan out across a bounded
        // pool of scoped threads. A shared atomic cursor caps in-flight
        // installs regardless of plugin count. Every failure is collected
        // (not bailed on the first) so one bad plugin can't mask the rest,
        // and the aggregate is sorted for a deterministic message.
        let workers = mkt.plugins.len().clamp(1, MAX_INSTALL_CONCURRENCY);
        let next = std::sync::atomic::AtomicUsize::new(0);
        let failures: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(p) = mkt.plugins.get(i) else { break };
                    let spec = format!("{p}@{}", mkt.name);
                    match self.runner.run(
                        "claude",
                        &["plugin", "install", &spec, "--scope", SCOPE],
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
        Ok(mkt.plugins.clone())
    }

    fn pointed_at(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .list()?
            .iter()
            .find(|e| e.name == name)
            .and_then(entry_source))
    }

    fn registered_name_for(&self, remote: &str) -> Result<Option<String>> {
        let want = git::canonical_remote_key(remote);
        if want.is_none() {
            return Ok(None);
        }
        Ok(self.list()?.into_iter().find_map(|e| {
            let src = entry_source(&e)?;
            let key = git::canonical_remote_key(&src).or_else(|| github_owner_repo_key(&src));
            (key == want).then_some(e.name)
        }))
    }

    fn marketplace_name(&self, repo_root: &Utf8Path) -> Option<String> {
        let raw = std::fs::read_to_string(repo_root.join(&self.marketplace_file)).ok()?;
        Marketplace::parse(&raw).ok().map(|m| m.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::fake::RecordingRunner;
    use crate::command::CommandOutput;
    use crate::output::{NoopReporter, RecordingReporter};
    use std::collections::HashSet;

    fn mkt(name: &str, plugins: &[&str]) -> Marketplace {
        Marketplace {
            name: name.into(),
            plugins: plugins.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn target<'a>(r: &'a RecordingRunner) -> ClaudeTarget<'a> {
        ClaudeTarget {
            runner: r,
            marketplace_file: ".claude-plugin/marketplace.json".into(),
        }
    }

    #[test]
    fn parses_local_and_github_entries() {
        let json = r#"[
          { "name": "mkt-a", "source": "github",
            "repo": "co/agent-mkt",
            "installLocation": "/Users/x/.claude/plugins/marketplaces/mkt-a" },
          { "name": "mkt-b", "source": "/work/wt",
            "installLocation": "/work/wt" }
        ]"#;
        let v = parse_marketplace_list(json).unwrap();
        let b = v.iter().find(|e| e.name == "mkt-b").unwrap();
        assert_eq!(entry_source(b).as_deref(), Some("/work/wt"));
    }

    #[test]
    fn apply_skips_remove_when_absent_then_adds_and_installs_each_plugin() {
        // List is empty ⇒ the non-idempotent `remove` must be skipped.
        let r = RecordingRunner::new().on(
            |p, a| p == "claude" && a.contains(&"list"),
            CommandOutput::ok("[]"),
        );
        let rep = RecordingReporter::default();

        target(&r)
            .apply("/work/wt", &mkt("M", &["p1", "p2"]), &rep)
            .unwrap();

        let lines = r.lines();
        assert!(
            !lines
                .iter()
                .any(|l| l == "claude plugin marketplace remove M"),
            "remove must be skipped when absent: {lines:?}"
        );
        assert!(lines.contains(&"claude plugin marketplace add /work/wt".to_string()));
        // Both plugins installed exactly once (fan-out order is nondeterministic).
        let installs: HashSet<_> = lines
            .iter()
            .filter(|l| l.contains("plugin install"))
            .cloned()
            .collect();
        assert_eq!(
            installs,
            HashSet::from([
                "claude plugin install p1@M --scope user".to_string(),
                "claude plugin install p2@M --scope user".to_string(),
            ])
        );
        let trail = rep.lines.lock().unwrap();
        assert!(trail.iter().any(|l| l == "Claude  ~ M"), "{trail:?}");
    }

    #[test]
    fn apply_gates_remove_on_presence_when_already_registered() {
        let r = RecordingRunner::new().on(
            |p, a| p == "claude" && a.contains(&"list"),
            CommandOutput::ok(r#"[{ "name": "M", "source": "github" }]"#),
        );
        target(&r)
            .apply("/work/wt", &mkt("M", &["p1"]), &NoopReporter)
            .unwrap();
        assert!(
            r.lines()
                .contains(&"claude plugin marketplace remove M".to_string()),
            "remove must fire when already present: {:?}",
            r.lines()
        );
    }

    #[test]
    fn apply_aggregates_and_sorts_install_failures() {
        let r = RecordingRunner::new()
            .on(
                |p, a| p == "claude" && a.contains(&"list"),
                CommandOutput::ok("[]"),
            )
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
        let err = target(&r)
            .apply("/wt", &mkt("M", &["p1", "p2", "p3"]), &NoopReporter)
            .unwrap_err()
            .to_string();
        assert!(err.contains("2 plugin installs failed"), "{err}");
        let p1 = err.find("p1@M").expect("p1 named");
        let p3 = err.find("p3@M").expect("p3 named");
        assert!(p1 < p3, "aggregated failures must be sorted: {err}");
    }
}
