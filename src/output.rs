//! User-facing rendering, uv-style: terse verb headers, dimmed timing,
//! colored `+`/`~`/`-` sigils, `✓`/`✗`, `hint:`/`warning:`.
//!
//! ANSI is produced unconditionally via `owo-colors`; `anstream` strips it at
//! the stream boundary when output is piped or `NO_COLOR` is set (exactly
//! uv's approach), so every renderer here stays testable as plain text.

use crate::command::{InitReport, StatusReport};
use camino::Utf8Path;
use owo_colors::OwoColorize;
#[cfg(test)]
use std::cell::RefCell;
use std::time::Duration;

/// Restart is *always* required — skillctl never inspects a running runtime.
pub const RESTART_NOTICE: &str = "not live until you restart Claude and Codex";

/// Human-readable elapsed time: `12ms`, `1.34s`, `1m 05s`.
///
/// Ported from uv (`astral-sh/uv`, `crates/uv/src/commands/mod.rs`),
/// dual-licensed MIT / Apache-2.0.
pub fn elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    let ms = d.subsec_millis();
    if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else if secs > 0 {
        format!("{}.{:02}s", secs, d.subsec_nanos() / 10_000_000)
    } else if ms > 0 {
        format!("{ms}ms")
    } else {
        format!("0.{:02}ms", d.subsec_nanos() / 10_000)
    }
}

// --- prefixed status lines (stderr) -----------------------------------------

pub fn warning(msg: &str) -> String {
    format!("{}{} {}", "warning".yellow().bold(), ":".bold(), msg)
}

pub fn hint(msg: &str) -> String {
    format!("{}{} {}", "hint".cyan().bold(), ":".bold(), msg)
}

/// cargo-/uv-style error report: `error: <top>` then each `Caused by:` cause
/// on its own indented, dimmed line.
pub fn error_report(err: &anyhow::Error) -> String {
    let mut out = format!("{}{} {}", "error".red().bold(), ":".bold(), err);
    for cause in err.chain().skip(1) {
        out.push_str(&format!("\n  {} {cause}", "Caused by:".dimmed()));
    }
    out
}

// --- streaming step events (the trail that survives a mid-sequence failure) --

/// What `sync`/`reset` announce as they progress. One semantic method keeps
/// the [`Reporter`] interface tiny while the formatting stays centralized.
pub enum Event<'a> {
    /// Printed once, after pre-flight, *before* the first mutation, so the
    /// target worktree is on screen even if a later step fails.
    Target {
        action: &'a str,
        target: &'a str,
        root: &'a Utf8Path,
        branch: &'a str,
        commit: &'a str,
        dirty: bool,
    },
    /// A runtime's marketplace was (re-)pointed.
    Marketplace { runtime: &'a str, name: &'a str },
    /// A plugin was installed.
    Plugin { name: &'a str },
}

pub fn render_event(e: &Event) -> String {
    match e {
        Event::Target {
            action,
            target,
            root,
            branch,
            commit,
            dirty,
        } => {
            let dirt = if *dirty {
                format!(" {}", "(dirty)".yellow())
            } else {
                String::new()
            };
            format!(
                "\n  {} {}\n   {} {}\n",
                action.bold(),
                target.bold(),
                "→".dimmed(),
                format_args!("{root} · {branch} {}{dirt}", commit.dimmed()),
            )
        }
        Event::Marketplace { runtime, name } => {
            format!("   {:<7} {} {name}", runtime, "~".yellow())
        }
        Event::Plugin { name } => {
            format!("           {} {name}", "+".green())
        }
    }
}

pub trait Reporter {
    fn event(&self, e: Event);
}

/// Production reporter: styles + flushes each event to stderr immediately, so
/// a failure mid-`sync` leaves a readable record of what already happened.
pub struct StderrReporter;
impl Reporter for StderrReporter {
    fn event(&self, e: Event) {
        anstream::eprintln!("{}", render_event(&e));
    }
}

/// For code paths / tests that don't care about progress.
#[cfg(test)]
pub struct NoopReporter;
#[cfg(test)]
impl Reporter for NoopReporter {
    fn event(&self, _e: Event) {}
}

/// Records the (de-styled) lines it was given, in order — used to prove the
/// streamed trail survives a mid-sequence failure.
#[cfg(test)]
#[derive(Default)]
pub struct RecordingReporter {
    pub lines: RefCell<Vec<String>>,
}
#[cfg(test)]
impl Reporter for RecordingReporter {
    fn event(&self, e: Event) {
        let plain = anstream::adapter::strip_str(&render_event(&e)).to_string();
        self.lines.borrow_mut().push(plain.trim().to_string());
    }
}

// --- summaries (stderr, on success) -----------------------------------------

pub fn sync_summary(plugins: usize, took: Duration) -> String {
    format!(
        "\n  {} {} in {}",
        "Synced".green().bold(),
        format_args!("{plugins} plugin{}", if plugins == 1 { "" } else { "s" }).bold(),
        elapsed(took).dimmed()
    )
}

pub fn reset_summary(owner_repo: &str, took: Duration) -> String {
    format!(
        "\n  {} {} in {}",
        "Reset".green().bold(),
        format_args!("→ {owner_repo} (default branch)").bold(),
        elapsed(took).dimmed()
    )
}

// --- status (stdout) --------------------------------------------------------

fn pointed(name: &Option<String>, src: &Option<String>, worktree: &str) -> String {
    match (name, src) {
        (Some(n), Some(s)) if s.trim_end_matches('/') == worktree.trim_end_matches('/') => {
            format!("{n} {} {}", "→".dimmed(), "this worktree".green())
        }
        (Some(n), Some(s)) => format!("{n} {} {}", "→".dimmed(), s.yellow()),
        (Some(n), None) => format!("{n} {} {}", "→".dimmed(), "(not registered)".dimmed()),
        (None, _) => format!("{}", "(no marketplace file detected)".dimmed()),
    }
}

/// Indent + a dimmed, fixed-width label so every value starts in one column
/// (uv keeps these aligned; `{:<W}` is padded *before* styling so ANSI codes
/// don't corrupt the width). The value column begins at `LABEL_COL`.
const LABEL_W: usize = 8;
const LABEL_COL: usize = 2 + LABEL_W + 1;

fn row(label: &str, value: impl std::fmt::Display) -> String {
    format!("  {} {value}\n", format!("{label:<LABEL_W$}").dimmed())
}

pub fn render_status(s: &StatusReport) -> String {
    let dirt = if s.repo.dirty {
        format!(" {}", "(dirty)".yellow())
    } else {
        String::new()
    };
    let match_line = if s.remote_matches {
        format!("{} matches configured remote", "✓".green())
    } else {
        format!("{} does NOT match configured remote", "✗".red().bold())
    };
    let wt = s.repo.root.as_str();
    let mut out = String::from("\n");
    out.push_str(&row(
        "worktree",
        format_args!("{wt} · {} {}{dirt}", s.repo.branch, s.repo.commit.dimmed()),
    ));
    out.push_str(&row("remote", &s.configured_remote));
    out.push_str(&row("origin", &s.repo.origin_url));
    out.push_str(&format!("{:LABEL_COL$}{match_line}\n", ""));
    out.push_str(&row(
        "Claude",
        pointed(&s.claude_name, &s.claude_pointed_at, wt),
    ));
    out.push_str(&row(
        "Codex",
        pointed(&s.codex_name, &s.codex_pointed_at, wt),
    ));
    out.push_str(&format!(
        "\n  {} ({})\n",
        "reset → default branch".dimmed(),
        s.default_branch
    ));
    out
}

// --- init (stdout) ----------------------------------------------------------

pub fn render_init(report: &InitReport) -> String {
    let mut out = format!(
        "\n  {} {}\n",
        "Wrote".green().bold(),
        report.config_path.as_str().bold(),
    );
    out.push_str(&row("repo", &report.repo_root));
    out.push_str(&row("remote", &report.remote));
    out.push_str(&row("branch", &report.default_branch));
    if !report.claude_file_present {
        out.push_str(&format!(
            "  {}\n",
            warning("Claude marketplace file not found at the configured path")
        ));
    }
    if !report.codex_file_present {
        out.push_str(&format!(
            "  {}\n",
            warning("Codex marketplace file not found at the configured path")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::RepoState;

    fn plain(s: &str) -> String {
        anstream::adapter::strip_str(s).to_string()
    }

    #[test]
    fn elapsed_is_human_readable() {
        assert_eq!(elapsed(Duration::from_millis(12)), "12ms");
        assert_eq!(elapsed(Duration::from_millis(1340)), "1.34s");
        assert_eq!(elapsed(Duration::from_secs(65)), "1m 05s");
    }

    #[test]
    fn event_sigils_are_plain_and_legible_when_destyled() {
        let m = plain(&render_event(&Event::Marketplace {
            runtime: "Codex",
            name: "skillctl-probe-mkt",
        }));
        assert_eq!(m, "   Codex   ~ skillctl-probe-mkt");
        let p = plain(&render_event(&Event::Plugin {
            name: "probe-plugin",
        }));
        assert_eq!(p.trim(), "+ probe-plugin");
    }

    #[test]
    fn target_event_shows_the_worktree_even_destyled() {
        let txt = plain(&render_event(&Event::Target {
            action: "Syncing",
            target: "co/agent-mkt",
            root: Utf8Path::new("/work/wt"),
            branch: "pr-123",
            commit: "abc1234",
            dirty: true,
        }));
        assert!(txt.contains("Syncing co/agent-mkt"), "{txt}");
        assert!(txt.contains("/work/wt · pr-123 abc1234"), "{txt}");
        assert!(txt.contains("(dirty)"), "{txt}");
    }

    #[test]
    fn error_report_unrolls_the_cause_chain() {
        let err = anyhow::anyhow!("policy.authentication \"NONE\" is invalid")
            .context("in /repo/.agents/plugins/marketplace.json")
            .context("`skillctl sync` failed");
        let txt = plain(&error_report(&err));
        assert!(txt.starts_with("error: `skillctl sync` failed"), "{txt}");
        assert!(
            txt.contains("Caused by: in /repo/.agents/plugins/marketplace.json"),
            "{txt}"
        );
        assert!(
            txt.contains("Caused by: policy.authentication \"NONE\" is invalid"),
            "{txt}"
        );
    }

    #[test]
    fn status_makes_a_remote_mismatch_loud_and_scannable() {
        let s = StatusReport {
            configured_remote: "git@github.com:co/agent-mkt.git".into(),
            repo: RepoState {
                root: "/work/wt".into(),
                branch: "pr-1".into(),
                commit: "abc1234".into(),
                dirty: true,
                origin_url: "git@github.com:other/x.git".into(),
            },
            remote_matches: false,
            default_branch: "main".into(),
            claude_name: Some("M".into()),
            codex_name: None,
            claude_pointed_at: Some("/work/wt".into()),
            codex_pointed_at: None,
        };
        let txt = plain(&render_status(&s));
        assert!(txt.contains("✗ does NOT match configured remote"), "{txt}");
        assert!(txt.contains("(dirty)"), "{txt}");
        assert!(txt.contains("M → this worktree"), "{txt}");
        assert!(txt.contains("(no marketplace file detected)"), "{txt}");
        assert!(txt.contains("reset → default branch (main)"), "{txt}");
        // Values line up in one column regardless of label length.
        for label in ["worktree", "remote", "origin", "Claude", "Codex"] {
            let line = txt
                .lines()
                .find(|l| l.trim_start().starts_with(label))
                .unwrap();
            assert_eq!(line[2..LABEL_COL].trim_end(), label, "line: {line:?}");
            assert!(
                !line[LABEL_COL..].starts_with(' '),
                "value not in aligned column for {label:?}: {line:?}"
            );
        }
    }

    #[test]
    fn status_flags_a_pointed_elsewhere_runtime() {
        let s = StatusReport {
            configured_remote: "r".into(),
            repo: RepoState {
                root: "/work/wt".into(),
                branch: "b".into(),
                commit: "c".into(),
                dirty: false,
                origin_url: "r".into(),
            },
            remote_matches: true,
            default_branch: "main".into(),
            claude_name: Some("M".into()),
            codex_name: None,
            claude_pointed_at: Some("/some/OTHER/path".into()),
            codex_pointed_at: None,
        };
        let txt = plain(&render_status(&s));
        assert!(txt.contains("✓ matches configured remote"), "{txt}");
        assert!(txt.contains("Claude   M → /some/OTHER/path"), "{txt}");
    }

    #[test]
    fn recording_reporter_keeps_an_ordered_destyled_trail() {
        let r = RecordingReporter::default();
        r.event(Event::Marketplace {
            runtime: "Codex",
            name: "M",
        });
        r.event(Event::Plugin { name: "p1" });
        assert_eq!(
            *r.lines.borrow(),
            vec!["Codex   ~ M".to_string(), "+ p1".to_string()]
        );
    }
}
