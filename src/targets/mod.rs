//! The **Target seam**: a uniform interface over every runtime skillctl
//! drives, plus the shared marketplace-definition model.
//!
//! Every per-runtime CLI specific — the mutation-free validators, the
//! marketplace remove/add/install dance, where a runtime currently points —
//! lives behind [`Target`], in [`claude::ClaudeTarget`] / [`codex::CodexTarget`].
//! Orchestration in [`crate::command`] iterates managed targets in `Runtime`
//! order and calls this trait; it never matches on which runtime it is. The
//! only place that names a concrete runtime is [`make`] — the seam's
//! construction point.

pub mod claude;
pub mod codex;

use crate::command::CommandRunner;
use crate::config::{Runtime, TargetCfg};
use crate::output::Reporter;
use anyhow::{bail, Context, Result};
use camino::Utf8Path;
use serde::Deserialize;

/// The structural essentials skillctl needs out of either runtime's
/// `marketplace.json`: the registered `name` (the identity both runtimes key
/// on) and the list of plugin names to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marketplace {
    pub name: String,
    pub plugins: Vec<String>,
}

#[derive(Deserialize)]
struct RawMarketplace {
    name: Option<String>,
    #[serde(default)]
    plugins: Vec<RawPlugin>,
}

#[derive(Deserialize)]
struct RawPlugin {
    name: Option<String>,
}

impl Marketplace {
    /// Parse the `name` + plugin names common to both runtimes' marketplace
    /// files. Rejects a missing/blank `name` and an empty plugin list — both
    /// make a sync meaningless.
    pub fn parse(json: &str) -> Result<Self> {
        let raw: RawMarketplace = serde_json::from_str(json).context("parsing marketplace.json")?;

        let name = raw
            .name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .context("marketplace.json is missing a top-level \"name\"")?;

        let mut plugins = Vec::with_capacity(raw.plugins.len());
        for (i, p) in raw.plugins.into_iter().enumerate() {
            let pname = p
                .name
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .with_context(|| format!("plugins[{i}] is missing a \"name\""))?;
            plugins.push(pname);
        }

        if plugins.is_empty() {
            bail!("marketplace.json \"{name}\" has no plugins to install");
        }

        Ok(Marketplace { name, plugins })
    }
}

/// `init`-time facts about one runtime: is its CLI on `PATH`, and does the
/// repo ship its marketplace file? The *decision* (auto-detect vs an explicit
/// `--*-only` override) stays in `init` — this is just the raw probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detection {
    pub on_path: bool,
    pub file_present: bool,
}

/// Everything one runtime's mutation-free pre-flight produced: the parsed
/// marketplace plus any informational advisories surfaced *after* a
/// successful sync/reset (Codex: plugins that register but won't
/// auto-install). Empty `advisories` for a runtime that has none.
#[derive(Debug)]
pub struct Validated {
    pub marketplace: Marketplace,
    pub advisories: Vec<String>,
}

/// A runtime skillctl manages. **The seam.** Every method is a whole phase or
/// a single query; the per-runtime asymmetries (Codex's tolerated exit-1
/// `remove`, Claude's presence-gated `remove` + parallel install fan-out,
/// Codex's lack of a per-plugin install) are documented contracts *inside*
/// each implementation, never the caller's concern.
pub trait Target {
    fn runtime(&self) -> Runtime;

    /// Structural read: parse this runtime's marketplace from `dir` and
    /// collect advisories — **no authoritative validator runs**. This is
    /// `reset`'s path: it is a recovery command that snaps back to the remote
    /// default branch and must not reject on a validator.
    fn read(&self, dir: &Utf8Path) -> Result<Validated>;

    /// Authoritative pre-flight: [`read`](Target::read) plus this runtime's
    /// own validator (Claude's `plugin validate`, Codex's pinned auth-policy
    /// rule). This is `sync`'s path. **Touches no global state** — a failure
    /// here changes nothing.
    fn validate(&self, dir: &Utf8Path) -> Result<Validated>;

    /// Mutate: point this runtime's marketplace at `source` and (re)install
    /// its plugins, streaming progress to `reporter`. Returns the plugin
    /// names this runtime actually installed (empty for a runtime, like
    /// Codex, with no per-plugin install). Called only after every managed
    /// target's pre-flight has succeeded, in split-brain order.
    fn apply(
        &self,
        source: &str,
        mkt: &Marketplace,
        reporter: &dyn Reporter,
    ) -> Result<Vec<String>>;

    /// Where this runtime currently resolves marketplace `name` (a worktree
    /// path for a local marketplace, else the `owner/repo`); `None` ⇒ not
    /// registered. Drives `status` and the post-sync identity assertion.
    fn pointed_at(&self, name: &str) -> Result<Option<String>>;

    /// `status` run outside a git repo: the name of a registered marketplace
    /// whose source resolves to `remote`, if any (there is no local file to
    /// read the name from).
    fn registered_name_for(&self, remote: &str) -> Result<Option<String>>;

    /// `status` run inside a repo: the marketplace name from the local file
    /// under `repo_root`. Best-effort — `None` if missing/unparseable, and no
    /// validator runs (status must stay fast and degrade gracefully).
    fn marketplace_name(&self, repo_root: &Utf8Path) -> Option<String>;
}

/// Construct the adapter for `runtime`. The single legitimate match on a
/// concrete runtime — the seam's construction point. Orchestration calls this
/// for each `(Runtime, &TargetCfg)` from [`crate::config::Config::managed`],
/// so the resulting list is already in split-brain order.
pub fn make<'a>(
    runtime: Runtime,
    tcfg: &TargetCfg,
    runner: &'a dyn CommandRunner,
    codex_config: &Utf8Path,
) -> Box<dyn Target + 'a> {
    match runtime {
        Runtime::Claude => Box::new(claude::ClaudeTarget {
            runner,
            marketplace_file: tcfg.marketplace_file.clone(),
        }),
        Runtime::Codex => Box::new(codex::CodexTarget {
            runner,
            marketplace_file: tcfg.marketplace_file.clone(),
            config_path: codex_config.to_path_buf(),
        }),
    }
}

/// Is `program` usable here? Routed through the [`CommandRunner`] seam (not a
/// `which` crate) so detection is unit-testable with the same fake the
/// orchestration tests use. A non-spawnable binary surfaces as `Err` from the
/// real runner; both that and a non-zero `--version` mean "not on PATH".
pub(crate) fn on_path(runner: &dyn CommandRunner, program: &str) -> bool {
    runner
        .run(program, &["--version"], None)
        .map(|o| o.success())
        .unwrap_or(false)
}

/// `init`-time probe for `runtime`, keyed purely off [`Runtime`] (program
/// name + default marketplace path) because `init` runs *before* any config
/// exists — there is no configured adapter to ask. `init` iterates
/// [`Runtime::ALL`] through this; it never names a concrete runtime itself.
pub fn detect(runner: &dyn CommandRunner, repo_root: &Utf8Path, runtime: Runtime) -> Detection {
    Detection {
        on_path: on_path(runner, runtime.program()),
        file_present: repo_root.join(runtime.default_marketplace_file()).exists(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE_JSON: &str = r#"{
      "name": "skillctl-probe-mkt",
      "plugins": [
        { "name": "probe-plugin", "source": "./plugins/probe-plugin" },
        { "name": "second-plugin", "source": "./plugins/second" }
      ]
    }"#;

    #[test]
    fn rejects_missing_name() {
        let err = Marketplace::parse(r#"{ "plugins": [{ "name": "p" }] }"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("name"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_empty_plugin_list() {
        let err = Marketplace::parse(r#"{ "name": "m", "plugins": [] }"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no plugins"), "unexpected error: {err}");
    }

    #[test]
    fn parses_name_and_plugin_names() {
        let m = Marketplace::parse(CLAUDE_JSON).unwrap();
        assert_eq!(m.name, "skillctl-probe-mkt");
        assert_eq!(m.plugins, vec!["probe-plugin", "second-plugin"]);
    }
}
