# skillctl — agent instructions

## No backward compatibility

This is a pre-1.0 personal tool with no external users. **Do not preserve
backward compatibility** when changing the design. The on-disk
`config.toml` schema, the CLI surface, and every internal API may change
freely. Always prefer the cleanest design over a compatible one — no
migration shims, no deprecated aliases, no version gates, no "old format
still parses" fallbacks.

## Checks before declaring done

`cargo fmt`, `cargo clippy --all-targets`, and `cargo test` must all be
clean. A clippy warning is a failure, not a warning — fix it (prefer
`#[cfg(test)]` over `#[allow(dead_code)]` for test-only API).

## Architecture invariant

See `CONTEXT.md`. Orchestration in `command.rs` stays uniform over the
`Target` seam: never `match` on a concrete runtime there. A new per-runtime
quirk goes inside that runtime's adapter (`targets::{claude,codex}`), behind
the trait — not into `init`/`sync`/`reset`/`status`.
