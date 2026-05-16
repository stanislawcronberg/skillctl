//! Claude Code target: marketplace add/remove, plugin install, list read-back.

use anyhow::{Context, Result};
use serde::Deserialize;

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

/// The currently-pointed-at location for `name`, used by `status` and the
/// post-sync identity assertion: the worktree path for a local marketplace,
/// else the `owner/repo` for a github source.
pub fn entry_source(e: &ClaudeEntry) -> Option<String> {
    e.install_location
        .clone()
        .or_else(|| e.repo.clone())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(v.len(), 2);

        let a = v.iter().find(|e| e.name == "mkt-a").unwrap();
        assert_eq!(a.repo.as_deref(), Some("co/agent-mkt"));

        let b = v.iter().find(|e| e.name == "mkt-b").unwrap();
        assert_eq!(entry_source(b).as_deref(), Some("/work/wt"));
    }
}
