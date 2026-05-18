//! `~/.config/skillctl/config.toml` model, load/save, and path resolution.

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;

/// The runtimes skillctl can drive.
///
/// **Declaration order is the split-brain mutation order** (see `CONTEXT.md`):
/// Codex is mutated fully before Claude, so a Codex rejection aborts before
/// Claude is touched (there is no rollback). `Ord` follows declaration order,
/// so `BTreeMap` iteration and every [`Config::managed`] walk yield Codex
/// first — the ordering invariant lives here, in one place, not in
/// orchestration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Runtime {
    Codex,
    Claude,
}

impl Runtime {
    /// Every runtime, in split-brain order.
    pub const ALL: [Runtime; 2] = [Runtime::Codex, Runtime::Claude];

    /// The on-`PATH` CLI / subprocess program name.
    pub fn program(self) -> &'static str {
        match self {
            Runtime::Codex => "codex",
            Runtime::Claude => "claude",
        }
    }

    /// Human label for summaries, warnings and the streamed trail.
    pub fn label(self) -> &'static str {
        match self {
            Runtime::Codex => "Codex",
            Runtime::Claude => "Claude",
        }
    }

    /// The config-key / TOML-table name (`[targets.<key>]`).
    pub fn key(self) -> &'static str {
        match self {
            Runtime::Codex => "codex",
            Runtime::Claude => "claude",
        }
    }

    fn from_key(s: &str) -> Option<Runtime> {
        match s {
            "codex" => Some(Runtime::Codex),
            "claude" => Some(Runtime::Claude),
            _ => None,
        }
    }

    /// The fixed v0 marketplace-file location this runtime ships, relative to
    /// the repo root. One place so detection and `Config::new` can't drift.
    pub fn default_marketplace_file(self) -> &'static str {
        match self {
            Runtime::Claude => ".claude-plugin/marketplace.json",
            Runtime::Codex => ".agents/plugins/marketplace.json",
        }
    }
}

// Explicit string ser/de so a `BTreeMap<Runtime, _>` serializes to TOML
// `[targets.codex]` / `[targets.claude]` tables (unit-enum-as-map-key is not
// universally supported by the `toml` serializer; a string key always is) and
// an unrecognized runtime key fails the config with a clear message.
impl Serialize for Runtime {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.key())
    }
}

impl<'de> Deserialize<'de> for Runtime {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Runtime::from_key(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown runtime {s:?}")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub repo: RepoConfig,
    /// **Presence in this map is the "managed" decision** — there is no
    /// `enabled` flag. A runtime skillctl should not touch simply has no
    /// entry here. Keyed by [`Runtime`], so iteration is split-brain order.
    pub targets: BTreeMap<Runtime, TargetCfg>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConfig {
    /// Stored verbatim; the canonical match-key is derived at runtime.
    pub remote: String,
}

/// Per-runtime config. Symmetric across runtimes by design — anything
/// runtime-specific (e.g. Claude's install scope) is the adapter's concern,
/// not config's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetCfg {
    pub marketplace_file: Utf8PathBuf,
}

impl Config {
    pub fn from_toml(s: &str) -> Result<Self> {
        toml::from_str(s).context("parsing skillctl config.toml")
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("Config is always TOML-serializable")
    }

    /// The config skillctl writes on `init`: the configured remote plus one
    /// `TargetCfg` per *managed* runtime (fixed v0 marketplace-file paths).
    /// An unmanaged runtime has no entry at all — to add one later, add its
    /// `[targets.<runtime>]` table (or re-run `init`).
    pub fn new(remote: impl Into<String>, runtimes: impl IntoIterator<Item = Runtime>) -> Self {
        let targets = runtimes
            .into_iter()
            .map(|r| {
                (
                    r,
                    TargetCfg {
                        marketplace_file: r.default_marketplace_file().into(),
                    },
                )
            })
            .collect();
        Config {
            repo: RepoConfig {
                remote: remote.into(),
            },
            targets,
        }
    }

    /// The managed runtimes and their config, in split-brain order. The one
    /// place orchestration learns *which* runtimes to drive — it never names
    /// `claude`/`codex` by hand.
    pub fn managed(&self) -> impl Iterator<Item = (Runtime, &TargetCfg)> + '_ {
        self.targets.iter().map(|(r, t)| (*r, t))
    }

    /// Is `runtime` managed (present in the config)? Test-only ergonomics —
    /// production iterates [`managed`](Self::managed) rather than asking per
    /// runtime.
    #[cfg(test)]
    pub fn manages(&self, runtime: Runtime) -> bool {
        self.targets.contains_key(&runtime)
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

[targets.codex]
marketplace_file = ".agents/plugins/marketplace.json"

[targets.claude]
marketplace_file = ".claude-plugin/marketplace.json"
"#;

    #[test]
    fn parses_sample_and_survives_a_roundtrip() {
        let cfg = Config::from_toml(SAMPLE).unwrap();
        assert_eq!(
            cfg.repo.remote,
            "git@github.com:company/agent-marketplace.git"
        );
        assert!(cfg.manages(Runtime::Claude));
        assert!(cfg.manages(Runtime::Codex));
        assert_eq!(
            cfg.targets[&Runtime::Codex].marketplace_file,
            ".agents/plugins/marketplace.json"
        );

        let reparsed = Config::from_toml(&cfg.to_toml()).unwrap();
        assert_eq!(cfg, reparsed);
    }

    #[test]
    fn managed_iterates_codex_before_claude_split_brain_order() {
        let cfg = Config::new(
            "git@github.com:co/r.git",
            [Runtime::Claude, Runtime::Codex], // deliberately reversed
        );
        let order: Vec<Runtime> = cfg.managed().map(|(r, _)| r).collect();
        assert_eq!(
            order,
            vec![Runtime::Codex, Runtime::Claude],
            "managed() must always yield Codex before Claude"
        );
    }

    #[test]
    fn presence_is_the_managed_decision() {
        // A codex-only machine: the claude table is simply absent.
        let cfg = Config::new("git@github.com:co/r.git", [Runtime::Codex]);
        assert!(cfg.manages(Runtime::Codex));
        assert!(!cfg.manages(Runtime::Claude));
        let toml = cfg.to_toml();
        assert!(toml.contains("[targets.codex]"), "{toml}");
        assert!(
            !toml.contains("claude"),
            "an unmanaged runtime must leave no trace in config: {toml}"
        );
        // And it round-trips back to exactly one managed runtime.
        assert_eq!(Config::from_toml(&toml).unwrap(), cfg);
    }

    #[test]
    fn new_then_save_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("a/b/config.toml")).unwrap();
        let cfg = Config::new("git@github.com:co/repo.git", [Runtime::Claude]);
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
    fn unknown_runtime_key_is_a_clear_error() {
        let err = Config::from_toml(
            r#"
[repo]
remote = "git@github.com:co/r.git"

[targets.emacs]
marketplace_file = "x"
"#,
        )
        .unwrap_err();
        let err = format!("{err:#}"); // full cause chain, like the rest of the suite
        assert!(err.contains("emacs"), "should name the bad runtime: {err}");
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
