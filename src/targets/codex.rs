//! Codex target: marketplace add/remove, structural pre-flight, config.toml.
//!
//! Codex's quirks, all local to this adapter: `marketplace remove` is
//! unconditional and its exit 1 when absent is tolerated (a different source
//! under the same name is otherwise refused); there is no per-plugin install
//! command, so `apply` only re-points the marketplace and `validate` surfaces
//! the plugins Codex won't auto-install as advisories.

use super::{Marketplace, Target, Validated};
use crate::command::CommandRunner;
use crate::config::Runtime;
use crate::output::{Event, Reporter};
use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;

/// A `[marketplaces.<name>]` record from `~/.codex/config.toml` — Codex's
/// only state surface (it has no `list` command).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CodexEntry {
    #[serde(default)]
    pub source_type: String,
    #[serde(default)]
    pub source: String,
}

/// Parse every `[marketplaces.<name>]` from the Codex config at
/// `config_path`. A missing config file is not an error — it just means
/// "nothing registered" (`Ok(None)`). The single read+parse seam shared by
/// [`read_marketplace`] and [`find_marketplace_by_source`].
fn read_all(
    config_path: &Utf8Path,
) -> Result<Option<std::collections::HashMap<String, CodexEntry>>> {
    let raw = match std::fs::read_to_string(config_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {config_path}")),
    };

    #[derive(Deserialize)]
    struct CodexConfig {
        #[serde(default)]
        marketplaces: std::collections::HashMap<String, CodexEntry>,
    }

    let cfg: CodexConfig =
        toml::from_str(&raw).with_context(|| format!("parsing {config_path}"))?;
    Ok(Some(cfg.marketplaces))
}

/// Read `[marketplaces.<name>]` from the Codex config at `config_path`.
/// A missing config file is not an error — it just means "not registered".
pub fn read_marketplace(config_path: &Utf8Path, name: &str) -> Result<Option<CodexEntry>> {
    Ok(read_all(config_path)?.and_then(|m| m.get(name).cloned()))
}

/// Find the registered marketplace whose `source` resolves to the same
/// canonical remote as `remote`, returning its `<name>`. Used by `status`
/// when run outside a git repo, where there is no local marketplace file to
/// read the name from — Codex has no `list` command, so its only state
/// surface is `~/.codex/config.toml`. Missing config ⇒ `Ok(None)` (same
/// contract as [`read_marketplace`]). If several entries match, the
/// lexicographically-first name is returned so the rendered status is
/// deterministic.
pub fn find_marketplace_by_source(config_path: &Utf8Path, remote: &str) -> Result<Option<String>> {
    let want = crate::git::canonical_remote_key(remote);
    if want.is_none() {
        return Ok(None);
    }
    let Some(marketplaces) = read_all(config_path)? else {
        return Ok(None);
    };
    let mut hits: Vec<&String> = marketplaces
        .iter()
        .filter(|(_, e)| crate::git::canonical_remote_key(&e.source) == want)
        .map(|(name, _)| name)
        .collect();
    hits.sort();
    Ok(hits.first().map(|s| s.to_string()))
}

#[derive(Deserialize)]
struct RawCodexMarketplace {
    #[serde(default)]
    plugins: Vec<RawCodexPlugin>,
}

#[derive(Deserialize)]
struct RawCodexPlugin {
    name: Option<String>,
    policy: Option<RawPolicy>,
}

#[derive(Deserialize)]
struct RawPolicy {
    authentication: Option<String>,
    installation: Option<String>,
}

/// Codex's only real validator is its *destructive* `add`. Before we let it
/// near global state we pre-empt the one rule we've empirically pinned down:
/// `policy.authentication`, when present, must be `ON_INSTALL` or `ON_USE`
/// (Codex hard-rejects e.g. `NONE`). Deliberately shallow — `add` remains the
/// authority for everything else.
pub fn validate_marketplace_json(json: &str) -> Result<()> {
    const ALLOWED: [&str; 2] = ["ON_INSTALL", "ON_USE"];

    let raw: RawCodexMarketplace =
        serde_json::from_str(json).context("parsing Codex marketplace.json")?;

    for plugin in &raw.plugins {
        let Some(policy) = &plugin.policy else {
            continue;
        };
        let Some(auth) = &policy.authentication else {
            continue;
        };
        if !ALLOWED.contains(&auth.as_str()) {
            let who = plugin.name.as_deref().unwrap_or("<unnamed>");
            bail!(
                "Codex plugin \"{who}\": policy.authentication = \"{auth}\" \
                 is invalid (allowed: ON_INSTALL | ON_USE)"
            );
        }
    }
    Ok(())
}

/// Codex registers a marketplace but only *auto-installs* a plugin when its
/// `policy.installation` is `INSTALLED_BY_DEFAULT`. With the default
/// `AVAILABLE` (or absent, or any other value) Codex leaves the plugin
/// merely *available* — registered/enabled but not installed, so its skills
/// never load. skillctl has no way to install them (Codex has no per-plugin
/// install command, and `marketplace upgrade` rejects local marketplaces),
/// so the honest thing is to name them loudly. Returns the plugin names that
/// will be *available but not installed*; `NOT_AVAILABLE` plugins are
/// deliberately hidden by the author and are not reported.
pub fn installation_advisory(json: &str) -> Result<Vec<String>> {
    let raw: RawCodexMarketplace =
        serde_json::from_str(json).context("parsing Codex marketplace.json")?;

    let mut not_installed = Vec::new();
    for (i, plugin) in raw.plugins.iter().enumerate() {
        let installation = plugin
            .policy
            .as_ref()
            .and_then(|p| p.installation.as_deref());
        match installation {
            Some("INSTALLED_BY_DEFAULT") | Some("NOT_AVAILABLE") => {}
            // None ⇒ defaults to AVAILABLE; "AVAILABLE" or any unknown value
            // ⇒ not auto-installed.
            _ => {
                let name = plugin
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("plugins[{i}]"));
                not_installed.push(name);
            }
        }
    }
    Ok(not_installed)
}

pub struct CodexTarget<'a> {
    pub runner: &'a dyn CommandRunner,
    pub marketplace_file: Utf8PathBuf,
    /// `~/.codex/config.toml` — Codex's only state surface (no `list` command).
    pub config_path: Utf8PathBuf,
}

impl CodexTarget<'_> {
    /// Read + structurally parse the marketplace once, returning the parsed
    /// form and the raw JSON (the auth-policy validator and the advisory both
    /// need the raw text). Kept private so the raw text never leaves this
    /// adapter.
    fn parsed(&self, dir: &Utf8Path) -> Result<(Marketplace, String)> {
        let path = dir.join(&self.marketplace_file);
        let raw = std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
        let marketplace = Marketplace::parse(&raw).with_context(|| format!("in {path}"))?;
        Ok((marketplace, raw))
    }

    /// The advisory step shared by `read` and `validate`, working off an
    /// already-parsed marketplace so neither re-reads the file.
    fn advise(&self, dir: &Utf8Path, marketplace: Marketplace, raw: &str) -> Result<Validated> {
        let advisories = installation_advisory(raw)
            .with_context(|| format!("in {}", dir.join(&self.marketplace_file)))?;
        Ok(Validated {
            marketplace,
            advisories,
        })
    }
}

impl Target for CodexTarget<'_> {
    fn runtime(&self) -> Runtime {
        Runtime::Codex
    }

    fn read(&self, dir: &Utf8Path) -> Result<Validated> {
        // Structural only (reset's path): parse + advisory, no auth-policy
        // validator. The raw JSON is parsed but never leaves this method.
        let (marketplace, raw) = self.parsed(dir)?;
        self.advise(dir, marketplace, &raw)
    }

    fn validate(&self, dir: &Utf8Path) -> Result<Validated> {
        // Authoritative (sync's path): the structural read plus Codex's pinned
        // auth-policy rule — the one thing we pre-empt before its destructive
        // `add`. One read+parse, reused for all three checks.
        let (marketplace, raw) = self.parsed(dir)?;
        validate_marketplace_json(&raw)
            .with_context(|| format!("in {}", dir.join(&self.marketplace_file)))?;
        self.advise(dir, marketplace, &raw)
    }

    fn apply(
        &self,
        source: &str,
        mkt: &Marketplace,
        reporter: &dyn Reporter,
    ) -> Result<Vec<String>> {
        // `remove` is unconditional and its exit 1 (absent) is tolerated:
        // Codex refuses a re-`add` of the same name pointing at a different
        // source, so it must always be cleared first.
        let _ = self.runner.run(
            "codex",
            &["plugin", "marketplace", "remove", &mkt.name],
            None,
        )?;
        let add = self
            .runner
            .run("codex", &["plugin", "marketplace", "add", source], None)?;
        if !add.success() {
            bail!(
                "`codex plugin marketplace add` failed: {}",
                add.stderr.trim()
            );
        }
        reporter.event(Event::Marketplace {
            runtime: Runtime::Codex.label(),
            name: &mkt.name,
        });
        // Codex has no per-plugin install command — it auto-installs per the
        // marketplace's own policy. Nothing more for skillctl to do, and no
        // plugins for skillctl to claim it installed.
        Ok(Vec::new())
    }

    fn pointed_at(&self, name: &str) -> Result<Option<String>> {
        Ok(read_marketplace(&self.config_path, name)?.map(|e| e.source))
    }

    fn registered_name_for(&self, remote: &str) -> Result<Option<String>> {
        find_marketplace_by_source(&self.config_path, remote)
    }

    fn marketplace_name(&self, repo_root: &Utf8Path) -> Option<String> {
        let raw = std::fs::read_to_string(repo_root.join(&self.marketplace_file)).ok()?;
        Marketplace::parse(&raw).ok().map(|m| m.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_with_auth(auth: &str) -> String {
        format!(
            r#"{{ "name": "m", "plugins": [
                {{ "name": "p", "policy": {{ "installation": "AVAILABLE", "authentication": "{auth}" }} }}
            ] }}"#
        )
    }

    #[test]
    fn reads_marketplace_block_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let p = camino::Utf8PathBuf::from_path_buf(dir.path().join("config.toml")).unwrap();
        std::fs::write(
            &p,
            r#"
model = "o3"

[marketplaces.skillctl-probe-mkt]
last_updated = "2026-05-09T21:56:27Z"
source_type = "local"
source = "/work/wt"

[marketplaces.other]
source_type = "git"
source = "co/other"
"#,
        )
        .unwrap();

        let e = read_marketplace(&p, "skillctl-probe-mkt").unwrap().unwrap();
        assert_eq!(e.source_type, "local");
        assert_eq!(e.source, "/work/wt");

        assert!(read_marketplace(&p, "absent").unwrap().is_none());
    }

    #[test]
    fn missing_config_file_means_not_registered() {
        let p = Utf8Path::new("/no/such/codex.toml");
        assert!(read_marketplace(p, "anything").unwrap().is_none());
        // The source-match lookup has the same missing-file contract.
        assert!(find_marketplace_by_source(p, "git@github.com:co/m.git")
            .unwrap()
            .is_none());
    }

    #[test]
    fn find_marketplace_by_source_matches_on_canonical_key() {
        let dir = tempfile::tempdir().unwrap();
        let p = camino::Utf8PathBuf::from_path_buf(dir.path().join("config.toml")).unwrap();
        std::fs::write(
            &p,
            r#"
[marketplaces.agent-mkt]
source_type = "git"
source = "git@github.com:co/agent-mkt.git"

[marketplaces.unrelated]
source_type = "git"
source = "git@github.com:other/thing.git"
"#,
        )
        .unwrap();

        // Cross-scheme: an https remote canonicalizes to the same key as the
        // stored scp-like ssh source.
        assert_eq!(
            find_marketplace_by_source(&p, "https://github.com/co/agent-mkt")
                .unwrap()
                .as_deref(),
            Some("agent-mkt")
        );
        // A remote that matches nothing registered.
        assert!(find_marketplace_by_source(&p, "git@github.com:co/none.git")
            .unwrap()
            .is_none());
        // An unrecognizable remote can't form a key → no match.
        assert!(find_marketplace_by_source(&p, "not-a-remote")
            .unwrap()
            .is_none());
    }

    #[test]
    fn accepts_on_install_and_on_use() {
        validate_marketplace_json(&json_with_auth("ON_INSTALL")).unwrap();
        validate_marketplace_json(&json_with_auth("ON_USE")).unwrap();
    }

    #[test]
    fn rejects_unknown_authentication_value() {
        let err = validate_marketplace_json(&json_with_auth("NONE"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("authentication"), "unexpected error: {err}");
        assert!(
            err.contains("ON_INSTALL"),
            "should name allowed values: {err}"
        );
    }

    #[test]
    fn installation_advisory_lists_only_not_auto_installed_plugins() {
        let json = r#"{ "name": "M", "plugins": [
            { "name": "p1", "policy": { "installation": "AVAILABLE" } },
            { "name": "p2", "policy": { "installation": "INSTALLED_BY_DEFAULT" } },
            { "name": "p3" },
            { "name": "p4", "policy": { "installation": "NOT_AVAILABLE" } },
            { "name": "p5", "policy": { "installation": "WAT" } }
        ] }"#;
        // p1 (AVAILABLE) + p3 (absent ⇒ defaults to AVAILABLE) + p5 (unknown)
        // are available-but-not-installed. p2 auto-installs; p4 is hidden.
        assert_eq!(
            installation_advisory(json).unwrap(),
            vec!["p1".to_string(), "p3".to_string(), "p5".to_string()]
        );
    }

    #[test]
    fn installation_advisory_empty_when_all_installed_by_default() {
        let json = r#"{ "name": "M", "plugins": [
            { "name": "p1", "policy": { "installation": "INSTALLED_BY_DEFAULT" } },
            { "name": "p2", "policy": { "installation": "INSTALLED_BY_DEFAULT" } }
        ] }"#;
        assert!(installation_advisory(json).unwrap().is_empty());
    }

    use crate::command::fake::RecordingRunner;
    use crate::command::CommandOutput;
    use crate::output::RecordingReporter;

    fn mkt(name: &str) -> Marketplace {
        Marketplace {
            name: name.into(),
            plugins: vec!["p1".into(), "p2".into()],
        }
    }

    #[test]
    fn apply_tolerates_remove_exit_1_then_adds_and_emits_no_installs() {
        let r = RecordingRunner::new().on(
            |p, a| p == "codex" && a.contains(&"remove"),
            CommandOutput::fail(1, "marketplace not found"),
        );
        let t = CodexTarget {
            runner: &r,
            marketplace_file: ".agents/plugins/marketplace.json".into(),
            config_path: "/no/codex.toml".into(),
        };
        let rep = RecordingReporter::default();

        t.apply("git@github.com:co/m.git", &mkt("M"), &rep).unwrap();

        let lines = r.lines();
        assert!(
            lines.contains(&"codex plugin marketplace add git@github.com:co/m.git".to_string()),
            "exit-1 remove must NOT abort the add: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("plugin install")),
            "Codex has no per-plugin install: {lines:?}"
        );
        assert!(rep.lines.lock().unwrap().iter().any(|l| l == "Codex   ~ M"));
    }

    #[test]
    fn validate_surfaces_the_auto_install_advisory() {
        let dir = tempfile::tempdir().unwrap();
        let d = camino::Utf8Path::from_path(dir.path()).unwrap();
        std::fs::create_dir_all(d.join(".agents/plugins")).unwrap();
        std::fs::write(
            d.join(".agents/plugins/marketplace.json"),
            r#"{ "name": "M", "plugins": [
                { "name": "p1", "policy": { "authentication": "ON_INSTALL" } },
                { "name": "p2", "policy": { "authentication": "ON_INSTALL" } } ] }"#,
        )
        .unwrap();
        let r = RecordingRunner::new();
        let t = CodexTarget {
            runner: &r,
            marketplace_file: ".agents/plugins/marketplace.json".into(),
            config_path: "/no/codex.toml".into(),
        };

        let v = t.validate(d).unwrap();

        assert_eq!(v.marketplace.name, "M");
        // No `policy.installation` ⇒ available-but-not-installed; surfaced.
        assert_eq!(v.advisories, vec!["p1".to_string(), "p2".to_string()]);
    }
}
