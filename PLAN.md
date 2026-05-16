# skillctl — v0 Build Plan (grilled & verified)

> This supersedes the original lean plan. Every assumption marked ✅ below was
> empirically verified on this machine (`claude` 2.1.143, `codex` 0.130.0).
> Items in **Pre-build empirical checklist** are *assumptions pending
> verification*, not open design decisions — run them first.

## Goal

A small Rust CLI for a **marketplace maintainer**. Workflow: edit the
marketplace in worktree A (e.g. open in an IDE), run `skillctl sync`, then go
run Claude/Codex in unrelated test projects B/C and see the synced
skills/plugins there. Editing location ≠ testing location.

```bash
cd ~/worktrees/agent-marketplace-pr-123
skillctl sync     # point Claude + Codex at THIS worktree, install all plugins
# ...restart Claude/Codex, test in any other project...
skillctl reset    # point both back at the repo's default branch
```

## Assumptions / scope

- One marketplace Git repo, containing **both** marketplace definitions.
- Commands run from inside a worktree of that repo.
- Fixed paths (configurable layouts deferred): Claude
  `.claude-plugin/marketplace.json`, Codex `.agents/plugins/marketplace.json`.
- **Restart/reload of Claude & Codex is always required after sync/reset.** No
  runtime visibility verification, ever.
- No profiles, multiple repos, custom roots, hashing, `clean`, `verify`,
  interactive prompts, publishing, or dependency resolution in v0.

## Verified environment facts (do not re-derive)

- ✅ **Name identity:** both runtimes register a marketplace under the
  `name` field of its `marketplace.json`, **not** the directory basename.
  → `skillctl` reads `name` from the JSON and uses it for remove/assert.
- ✅ **Claude dirty capture:** `claude plugin marketplace add <dir>` makes no
  copy (`installLocation` = the live worktree path). `claude plugin install`
  copies the **live working tree** into
  `~/.claude/plugins/cache/<mkt>/<plugin>/<ver>/`, **including uncommitted
  edits and untracked files**.
- ✅ **Claude plugin orphaning:** `claude plugin marketplace remove` drops
  **all** that marketplace's installed plugins. Sync/reset must reinstall.
- ✅ **Non-idempotent removes:** `marketplace remove <absent>` → exit 1 +
  error, on **both** runtimes. Must presence-check or tolerate exit 1.
- ✅ **Codex has no `list` and no `validate`:** `codex plugin marketplace`
  exposes only `add` / `upgrade` / `remove`. Codex's only validator is its
  **destructive `add`** (it hard-rejected `policy.authentication: "NONE"`;
  allowed values `ON_INSTALL` | `ON_USE`).
- ✅ **Codex plugin records survive marketplace remove:**
  `[plugins."x@<name>"]` entries persist across `remove`+`add` (keyed by
  `plugin@stable-name`), so the user's enabled Codex plugins are preserved
  across a sync. Orphan records accumulate only if the name changes / a plugin
  is dropped → that is what a future `skillctl clean codex` would target.
- ✅ Neither `add` prompts interactively for a local path (non-interactive OK).
- ✅ Claude `marketplace add` has **no `--ref`**; GitHub-source registration
  has no ref field → Claude reset can only land on the repo's actual default
  branch. Codex `add` has `--ref` (but see checklist #2).
- ✅ **Codex add-over-existing (was checklist #1):** same name + **same**
  source = idempotent no-op (exit 0). Same name + **different** source (the
  real worktree-switch case) = `Error: ... already added from a different
  source; remove it before adding this source`, **exit 1, no state change**.
  → Codex sync **must** `remove`→`add` unconditionally; no atomic-add
  optimization. Failure is clean (refuses, no corruption).

## Pre-build empirical checklist (run against `test-marketplace/` first)

`test-marketplace/` is the kept throwaway fixture (git repo, intentionally
dirty: `DIRTY-EDIT-v2-UNCOMMITTED` + an untracked `untracked-skill/`). Back up
`~/.claude/plugins/known_marketplaces.json` and `~/.codex/config.toml`, use the
unique probe name `skillctl-probe-mkt`, restore after.

1. ~~Codex add-over-existing~~ — **RESOLVED** (see verified facts): different
   source under same name → exit 1, must `remove` first. Codex sync is
   unconditionally `remove`→`add`.
2. **Codex default-branch:** does `codex plugin marketplace add <owner/repo>`
   with **no `--ref`** land on the repo default branch? If yes, `--ref` is
   never used.
3. **Codex install-time dirty capture:** does Codex's plugin cache capture
   dirty/untracked? (Lower risk; only reachable via interactive `/plugins`.)
4. **Claude github ref:** confirm `claude plugin marketplace add owner/repo`
   ignores any `@ref` and tracks the repo default branch (close the loop).

## Config (`~/.config/skillctl/config.toml`)

XDG-style explicit path (**not** `dirs::config_dir()`, which on macOS is
`~/Library/Application Support`). No `default_branch` key (always the repo's
actual default branch). `remote` stored verbatim; `owner/repo` and a canonical
match-key are derived at runtime.

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

## Commands

### `skillctl init [--force] [--default-branch <b>]`
- Hard error if not inside a git repo.
- Detect repo root, `origin` remote, presence of both marketplace files.
- Detect default branch (`git symbolic-ref refs/remotes/origin/HEAD`,
  fallback `main`, `--default-branch` override) — **for `status` display
  only**, not persisted.
- Refuse to overwrite an existing config unless `--force`.

### `skillctl sync`
1. **Pre-flight (before any mutation):** detect repo root; canonicalize the
   `origin` URL and **hard-fail** if it ≠ canonicalized configured `remote`;
   parse both `marketplace.json` (must have `name` + `plugins[]`); Claude
   authoritative validation via `claude plugin validate <repo>`; Codex shallow
   structural check incl. known rule `policy.authentication ∈
   {ON_INSTALL, ON_USE}`. Abort cleanly on any failure — nothing removed.
2. **Codex first** (its `add` is the de-facto validator): presence-check via
   `~/.codex/config.toml [marketplaces.<name>]`; `remove` (tolerate exit 1 if
   absent) → `add <repo-root>`. Unconditional remove→add (resolved checklist
   #1: a different worktree path under the same name is rejected otherwise).
3. **Then Claude:** presence-check via `claude plugin marketplace list --json`
   → conditional `remove` → `add <repo-root>` → `install <plugin>@<name>
   --scope user` for **every** plugin in `marketplace.json`.
4. **Post-sync assertion (loud):** `list --json` / `config.toml` must show
   exactly one entry, name == json `name`, source == current worktree path.
   Mismatch → fail loudly ("name-identity contract broken").
5. Print restart/reload instructions.

### `skillctl reset`
Same Codex-first order. Codex: `remove` → `add <owner/repo>` (default branch;
`--ref` only if checklist #2 fails). Claude: `remove` → `add <owner/repo>`
(default branch) → **reinstall all** plugins (remove orphaned them). Print
restart/reload instructions.

### `skillctl status` (fully live, no state file)
Configured remote; repo root / branch / commit / dirty (live `git`);
canonical-remote match yes/no; detected Claude & Codex marketplace names;
currently-pointed-at source from `claude ... list --json` and Codex
`config.toml`; reminder that restart/reload is always required; explicit note
"reset → repo's default branch."

## Failure semantics
Sequential, fail-fast, **no rollback machinery** in v0. Codex-first ordering
guarantees an unanticipated Codex rejection aborts before Claude is mutated →
never split-brain. On mid-sequence failure, name the failed step and tell the
user to re-run `skillctl sync` / `skillctl reset`.

## Modules / crates
```
src/{main,cli,config,git,command,output}.rs
src/targets/{mod,claude,codex}.rs
```
(No `state.rs`.) Crates: `anyhow`, `camino`, `clap` (derive), `dirs`,
`serde`, `serde_json`, `toml`, `which`.

## v0 success case
```bash
cd ~/worktrees/agent-marketplace-pr-123
skillctl init && skillctl sync && skillctl status
# Codex then Claude pointed at this worktree; all plugins installed (live tree,
# incl. uncommitted edits); name-identity asserted; user told to restart.
skillctl reset
# Both back on the repo's default branch; user told to restart.
```

## Explicitly out of scope for v0
runtime verification, `verify`, diagnostic stamps, tree/cache hashing,
automatic Codex plugin enable/disable, app-server integration, profiles,
multiple repos, custom roots, deep frontmatter validation beyond pre-flight,
`clean`, interactive prompts, publishing, browsing/search, dependency
resolution, state/history persistence.
