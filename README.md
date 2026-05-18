# skillctl

*For when it's a skill issue.*

Test the skills you're editing in a live **Claude Code** and **Codex** — then
snap both back — in one command.

You author agent **skills** in one repo but *use* them somewhere else, and a
skill only counts once a runtime actually loads it. `skillctl` closes that
loop: it points Claude and Codex at the marketplace worktree you're editing and
installs every plugin in it, so the skills you just changed go live. Test them
in any other project, then `skillctl reset` snaps both runtimes back to the
repo's default branch.

> **Restart required.** Claude and Codex only pick up marketplace changes on
> restart/reload. `skillctl` never inspects a running runtime, so `sync` and
> `reset` always remind you to restart both.

## Requirements

- A stable **Rust** toolchain to build — [rustup.rs](https://rustup.rs)
- **`claude`** and/or **`codex`** on your `PATH` — you don't need both
- A Git worktree of the marketplace repo that ships your skills — required by
  `init` and `sync`; `reset` and `status` run from any directory. `init`
  manages a runtime only when its marketplace file is present —
  `.claude-plugin/marketplace.json` for Claude,
  `.agents/plugins/marketplace.json` for Codex — so one is enough.

## Install

```bash
cargo install --path .   # builds and installs skillctl into ~/.cargo/bin
```

## Quickstart

Run `init` and `sync` from inside a worktree of your marketplace repo;
`reset` and `status` work from any directory.

```bash
cd ~/worktrees/agent-marketplace-pr-123
skillctl init      # once per machine: detect repo, write the config
skillctl sync      # point Codex + Claude at THIS worktree, install every plugin
# → restart Claude & Codex, then exercise the edited skills in any other project
skillctl reset     # snap both back to the default branch (runs from anywhere)
# → restart Claude & Codex again
```

## Commands

| Command | What it does |
|---|---|
| `init` | Detect the repo and write `~/.config/skillctl/config.toml`. By default a runtime is managed only when its CLI is on `PATH` **and** its marketplace file is in the repo, so a Codex-only (or Claude-only) machine just works. Flags: `--force` (overwrite), `--claude-only` / `--codex-only` (mutually exclusive — scope to one runtime regardless of detection). |
| `sync` | Validate everything **before touching anything**, then point Codex and Claude at the current worktree and install every plugin, putting your edited skills live. Claude captures the *live* working tree, including uncommitted edits. A pre-flight failure changes nothing. |
| `reset` | Point both runtimes back at the configured repo's default branch and reinstall every plugin. Runs from **any directory** — it shallow-clones the configured remote (never the local worktree), so it's a safe recovery command and never reinstalls uncommitted local plugins. |
| `status` | Live snapshot: configured remote, worktree branch/commit/dirty, whether `origin` matches, detected marketplace names, and where each runtime currently points. Works **outside a git repo** too — the worktree/origin/match rows are simply omitted. |

## Configuration

`skillctl init` writes `~/.config/skillctl/config.toml` (honoring
`$XDG_CONFIG_HOME`) — you rarely need to edit it by hand. Each target carries
an `enabled` flag set from detection (or `--claude-only` / `--codex-only`);
`sync`, `reset`, and `status` only touch enabled runtimes, and flipping
`enabled` by hand is a supported way to add or drop one without re-running
`init`:

```toml
[repo]
remote = "git@github.com:company/agent-marketplace.git"

[targets.claude]
enabled = true
scope = "user"
marketplace_file = ".claude-plugin/marketplace.json"

[targets.codex]
enabled = true
marketplace_file = ".agents/plugins/marketplace.json"
```

`sync` refuses to run if the worktree's `origin` doesn't match the configured
`remote` (compared canonically, so `git@`/`https`/`ssh` forms all match).

Any git host works — GitHub, GitLab (including nested groups/subgroups),
Bitbucket, or self-hosted — because `reset` registers the configured remote URL
as-is and both runtimes resolve a ref-less git URL to its default branch.

**Codex auto-install caveat.** Codex installs a plugin automatically only when
its `marketplace.json` entry sets `"policy": { "installation":
"INSTALLED_BY_DEFAULT" }`. With the default (`AVAILABLE`, or the field absent)
Codex registers the plugin but doesn't install it, so its skills never load —
and skillctl can't install Codex plugins itself. `sync` and `reset` therefore
print a warning naming those plugins. (`NOT_AVAILABLE` plugins are intentionally
hidden by the author and aren't reported.)

## License

[MIT](LICENSE) © stanislawcronberg
