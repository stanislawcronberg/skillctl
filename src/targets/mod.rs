//! Shared marketplace-definition model plus the per-runtime target modules.

pub mod claude;
pub mod codex;

use anyhow::{bail, Context, Result};
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
