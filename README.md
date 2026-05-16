# skillctl

Point **Claude Code** and **Codex** at a marketplace worktree, then reset them
back — in one command.

If you maintain an agent-skills/plugins marketplace, you typically *edit* the
marketplace in one place and *test* the agents somewhere else entirely.
`skillctl` makes that loop fast: register the worktree you're editing with both
runtimes, install every plugin, go test in any other project, then snap both
runtimes back to the repo's default branch when you're done.

```bash
cd ~/worktrees/agent-marketplace-pr-123
skillctl sync      # point Claude + Codex at THIS worktree, install all plugins
# …restart Claude/Codex, then test in any other project…
skillctl reset     # point both back at the repo's default branch
```

> **Restart required.** Claude and Codex only pick up marketplace changes on
> restart/reload. `skillctl` never inspects a running runtime, so every `sync`
> and `reset` ends by reminding you to restart both.

## Requirements

- **Rust** 1.85+ (to build) — [rustup.rs](https://rustup.rs)
- **`claude`** CLI on your `PATH` (Claude Code)
- **`codex`** CLI on your `PATH`
- A Git checkout/worktree of a marketplace repo that contains **both**
  marketplace definitions:
  - `.claude-plugin/marketplace.json` (Claude)
  - `.agents/plugins/marketplace.json` (Codex)

## Install

From a clone of this repository:

```bash
cargo install --path .
```

This builds an optimized binary and drops `skillctl` into `~/.cargo/bin`
(make sure that's on your `PATH`).

To build without installing:

```bash
cargo build --release
./target/release/skillctl --help
```

## Usage

Run all commands from **inside a worktree of your marketplace repo**.

### `skillctl init`

Detects the repo and writes `~/.config/skillctl/config.toml`.

```bash
skillctl init                       # detect remote + marketplace files
skillctl init --force               # overwrite an existing config
skillctl init --default-branch dev  # override the displayed default branch
```

### `skillctl sync`

Validates everything **before touching anything**, then points Codex and Claude
at the current worktree and installs every plugin (Claude installs the *live*
working tree, including uncommitted edits). On any pre-flight failure nothing is
changed.

```bash
skillctl sync
```

### `skillctl reset`

Points both runtimes back at the configured repo's default branch
(`owner/repo`) and reinstalls every plugin.

```bash
skillctl reset
```

### `skillctl status`

A fully live snapshot — configured remote, worktree branch/commit/dirty,
whether `origin` matches the configured remote, the detected marketplace names,
and where each runtime currently points.

```bash
skillctl status
```

## Configuration

`skillctl init` writes `~/.config/skillctl/config.toml` (honoring
`$XDG_CONFIG_HOME`):

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

`skillctl` refuses to `sync` if the worktree's `origin` doesn't match the
configured `remote` (compared canonically, so `git@`/`https`/`ssh` forms all
match the same repo).

## Typical workflow

```bash
cd ~/worktrees/agent-marketplace-pr-123
skillctl init      # once per machine / when the remote changes
skillctl sync      # Codex + Claude now point at this worktree
# restart Claude & Codex, test the skills/plugins in any other project
skillctl reset     # both back on the repo's default branch
# restart Claude & Codex again
```

## License

TBD.
