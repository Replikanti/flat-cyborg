# Contributing

## Build and test

```sh
cargo build
cargo test                  # unit + integration tests
cargo fmt --check
cargo clippy -- -D warnings
scripts/smoke-local.sh      # end-to-end against sh/cat (deterministic, no network)
scripts/smoke-llm.sh        # opt-in: drives real claude/codex CLIs
```

CI runs `check` (fmt + clippy `-D warnings` + tests) and `audit`
(`cargo audit` + `cargo deny check`); both are required on `main`.

## Dependency philosophy (hard rule)

The tree stays minimal: **`pty-process` → `rustix`** and their transitive
handful. There is deliberately **no async runtime** (std threads instead), no
`rand` (a hand-rolled SplitMix64 does the jitter), and a hand-written VT parser
instead of a terminal crate. Network operations (self-update) shell out to
`curl`/`wget` + `sha256sum` rather than pulling in an HTTP stack.

Before adding a crate, ask whether fifty lines of code would do. If a new
dependency is genuinely warranted, it must be MIT / Apache-2.0 licensed —
`deny.toml` allows nothing else and `cargo deny check` gates CI.

## Workflow

Trunk-based development: a short-lived branch per change, PR into the protected
`main`, squash-merge, branches auto-deleted. CI must be fully green before
merge — no exceptions for "pre-existing" or "unrelated" failures.

Shell test fixtures must be **dash-safe**: CI's `sh` is dash, whose `printf`
does not understand `\xHH` escapes — write multibyte glyphs literally.

## Releases

Bump `Cargo.toml`, update `CHANGELOG.md`, merge the release PR, then tag
`vX.Y.Z` on the merge commit and push the tag — `release.yml` builds
Linux/macOS × x86_64/aarch64 binaries with checksums and publishes the GitHub
release.
