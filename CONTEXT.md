# skillctl — domain & architecture vocabulary

Use these terms exactly, in code and in review. They name the seams.

## Domain

- **Runtime** — an agent CLI that loads skills from a marketplace: **Claude
  Code** or **Codex**. A *closed set* skillctl drives; modelled as
  `enum Runtime { Codex, Claude }`.
- **Marketplace** — the `marketplace.json` a Runtime registers: a `name` plus
  a plugin list. The `name` is the identity key both Runtimes resolve.
- **Plugin** — an installable unit inside a Marketplace; it carries the skills.
- **Worktree** — the local git checkout of the marketplace repo whose skills
  you are editing. `sync` points Runtimes here.
- **Sync** — point each managed Target at the Worktree and (re)install
  plugins, after a mutation-free Preflight.
- **Reset** — point each managed Target back at the configured remote's
  default branch (shallow clone; needs no Worktree). A recovery command.
- **Preflight** — *all* validation (origin match, Marketplace parse, per-Runtime
  validators, advisories) completed **before any mutation**.
- **Split-brain ordering** — Codex is mutated fully before Claude, so a Codex
  rejection aborts before Claude is touched (there is no rollback). Encoded
  as the **declaration order of `enum Runtime`** — Codex first.

## Architecture

- **Target** — a Runtime that skillctl manages, expressed through the `Target`
  trait. This is **the seam**: every per-Runtime behaviour (validators, the
  marketplace remove/add/install dance, where it currently points) lives
  behind it, in `ClaudeTarget` / `CodexTarget`. Two real adapters — a real
  seam, not a hypothetical one.
- **Managed** — a Runtime is managed *iff* its entry is present in
  `config.toml`'s `targets` map. **Presence = managed**; there is no
  `enabled` flag, no disabled-but-present state.

## The rule

Orchestration (`command.rs`) iterates managed Targets in `Runtime` order and
calls the seam. It must **never `match` on which Runtime** it is, and must
never reach past the seam into a Runtime's CLI specifics. A new per-Runtime
quirk goes inside that Runtime's adapter, never into orchestration.
