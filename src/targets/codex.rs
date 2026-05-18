//! Codex target: marketplace add/remove, structural pre-flight, config.toml.

use anyhow::{bail, Context, Result};
use camino::Utf8Path;
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
}
