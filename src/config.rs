//! `~/.config/skillctl/config.toml` model, load/save, and path resolution.

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub repo: RepoConfig,
    pub targets: Targets,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConfig {
    /// Stored verbatim; the canonical match-key is derived at runtime.
    pub remote: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Targets {
    pub claude: ClaudeTarget,
    pub codex: CodexTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeTarget {
    pub enabled: bool,
    pub scope: String,
    pub marketplace_file: Utf8PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexTarget {
    pub enabled: bool,
    pub marketplace_file: Utf8PathBuf,
}

impl Config {
    pub fn from_toml(s: &str) -> Result<Self> {
        toml::from_str(s).context("parsing skillctl config.toml")
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("Config is always TOML-serializable")
    }

    /// The config skillctl writes on `init` for a freshly detected repo:
    /// fixed v0 marketplace-file paths, both targets enabled, Claude at
    /// user scope.
    pub fn default_for(remote: impl Into<String>) -> Self {
        Config {
            repo: RepoConfig {
                remote: remote.into(),
            },
            targets: Targets {
                claude: ClaudeTarget {
                    enabled: true,
                    scope: "user".to_string(),
                    marketplace_file: ".claude-plugin/marketplace.json".into(),
                },
                codex: CodexTarget {
                    enabled: true,
                    marketplace_file: ".agents/plugins/marketplace.json".into(),
                },
            },
        }
    }

    pub fn load(path: &Utf8Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("no skillctl config at {path} — run `skillctl init` first"))?;
        Self::from_toml(&raw)
    }

    pub fn save(&self, path: &Utf8Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {parent}"))?;
        }
        std::fs::write(path, self.to_toml()).with_context(|| format!("writing config to {path}"))
    }
}

/// Resolve the skillctl config path with an *explicit* XDG layout, never
/// `dirs::config_dir()` (which on macOS is `~/Library/Application Support`).
/// Honors `$XDG_CONFIG_HOME`, else `$HOME/.config`.
pub fn config_path(xdg_config_home: Option<&str>, home: &Utf8Path) -> Utf8PathBuf {
    let base = match xdg_config_home {
        Some(x) if !x.is_empty() => Utf8PathBuf::from(x),
        _ => home.join(".config"),
    };
    base.join("skillctl").join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[repo]
remote = "git@github.com:company/agent-marketplace.git"

[targets.claude]
enabled = true
scope = "user"
marketplace_file = ".claude-plugin/marketplace.json"

[targets.codex]
enabled = true
marketplace_file = ".agents/plugins/marketplace.json"
"#;

    #[test]
    fn parses_sample_and_survives_a_roundtrip() {
        let cfg = Config::from_toml(SAMPLE).unwrap();
        assert_eq!(
            cfg.repo.remote,
            "git@github.com:company/agent-marketplace.git"
        );
        assert!(cfg.targets.claude.enabled);
        assert_eq!(cfg.targets.claude.scope, "user");
        assert_eq!(
            cfg.targets.codex.marketplace_file,
            ".agents/plugins/marketplace.json"
        );

        let reparsed = Config::from_toml(&cfg.to_toml()).unwrap();
        assert_eq!(cfg, reparsed);
    }

    #[test]
    fn default_for_then_save_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a/b/config.toml")).unwrap();
        let cfg = Config::default_for("git@github.com:co/repo.git");
        cfg.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), cfg);
    }

    #[test]
    fn load_missing_config_explains_how_to_fix_it() {
        let err = Config::load(Utf8Path::new("/no/such/skillctl.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("skillctl init"), "unexpected error: {err}");
    }

    #[test]
    fn config_path_prefers_xdg_then_falls_back_to_home_dot_config() {
        let home = Utf8Path::new("/home/u");
        assert_eq!(
            config_path(Some("/custom/xdg"), home),
            "/custom/xdg/skillctl/config.toml"
        );
        assert_eq!(
            config_path(None, home),
            "/home/u/.config/skillctl/config.toml"
        );
        // An empty XDG var must be treated as unset, not as the filesystem root.
        assert_eq!(
            config_path(Some(""), home),
            "/home/u/.config/skillctl/config.toml"
        );
    }
}
