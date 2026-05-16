//! User-facing rendering. Restart/reload of Claude & Codex is *always*
//! required after a mutation — skillctl never verifies runtime visibility, so
//! every sync/reset ends with this reminder.

use crate::command::{ResetReport, StatusReport, SyncReport};

pub const RESTART_NOTICE: &str = "Restart / reload both Claude and Codex for this to take effect \
     (skillctl never verifies runtime visibility).";

fn pointed(label: &str, name: &Option<String>, src: &Option<String>) -> String {
    match (name, src) {
        (Some(n), Some(s)) => format!("  {label}: \"{n}\" → {s}"),
        (Some(n), None) => format!("  {label}: \"{n}\" → (not registered)"),
        (None, _) => format!("  {label}: (no marketplace file detected)"),
    }
}

pub fn render_status(s: &StatusReport) -> String {
    let mut out = String::new();
    out.push_str("skillctl status\n");
    out.push_str(&format!("  configured remote: {}\n", s.configured_remote));
    out.push_str(&format!(
        "  worktree: {} @ {} ({}){}\n",
        s.repo.root,
        s.repo.branch,
        s.repo.commit,
        if s.repo.dirty { ", dirty" } else { "" }
    ));
    out.push_str(&format!("  origin: {}\n", s.repo.origin_url));
    out.push_str(&format!(
        "  remote match: {}\n",
        if s.remote_matches { "yes" } else { "NO" }
    ));
    out.push_str(&pointed("Claude", &s.claude_name, &s.claude_pointed_at));
    out.push('\n');
    out.push_str(&pointed("Codex", &s.codex_name, &s.codex_pointed_at));
    out.push('\n');
    out.push_str(&format!(
        "  note: `skillctl reset` points both back at the repo's default \
         branch ({}).\n",
        s.default_branch
    ));
    out.push_str(RESTART_NOTICE);
    out.push('\n');
    out
}

pub fn render_sync(r: &SyncReport) -> String {
    let mut out = format!("Synced → {}\n", r.repo_root);
    if let Some(n) = &r.claude_name {
        out.push_str(&format!(
            "  Claude: \"{n}\" + {} plugin(s) installed\n",
            r.plugins.len()
        ));
    }
    if let Some(n) = &r.codex_name {
        out.push_str(&format!("  Codex: \"{n}\"\n"));
    }
    out.push_str(RESTART_NOTICE);
    out.push('\n');
    out
}

pub fn render_reset(r: &ResetReport) -> String {
    let mut out = format!("Reset → {} (default branch)\n", r.owner_repo);
    if let Some(n) = &r.claude_name {
        out.push_str(&format!(
            "  Claude: \"{n}\" + {} plugin(s) reinstalled\n",
            r.plugins.len()
        ));
    }
    if let Some(n) = &r.codex_name {
        out.push_str(&format!("  Codex: \"{n}\"\n"));
    }
    out.push_str(RESTART_NOTICE);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::RepoState;

    #[test]
    fn status_render_shows_match_reset_note_and_restart_reminder() {
        let s = StatusReport {
            configured_remote: "git@github.com:co/agent-mkt.git".into(),
            repo: RepoState {
                root: "/work/wt".into(),
                branch: "pr-1".into(),
                commit: "abc1234".into(),
                dirty: true,
                origin_url: "git@github.com:co/agent-mkt.git".into(),
            },
            remote_matches: false,
            default_branch: "main".into(),
            claude_name: Some("M".into()),
            codex_name: None,
            claude_pointed_at: Some("/work/wt".into()),
            codex_pointed_at: None,
        };
        let txt = render_status(&s);
        assert!(txt.contains("remote match: NO"), "{txt}");
        assert!(txt.contains("default branch (main)"), "{txt}");
        assert!(
            txt.contains("Restart / reload both Claude and Codex"),
            "{txt}"
        );
        assert!(txt.contains("Claude: \"M\" → /work/wt"), "{txt}");
        assert!(
            txt.contains("Codex: (no marketplace file detected)"),
            "{txt}"
        );
    }
}
