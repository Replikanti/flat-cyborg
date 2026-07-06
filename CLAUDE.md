# CLAUDE.md

## What this is

An asynchronous PTY wrapper in Rust (public, MIT OR Apache-2.0). It wraps an interactive "target CLI" in a pseudo-terminal so the target sees a real TTY, intercepts both I/O streams, optionally jitters input like a human typist, sanitizes the ANSI output stream, emulates a 2D screen for full-screen TUIs, and detects lifecycle state (RUNNING / CONFIRMATION_PROMPT / IDLE) with a watchdog. Primary real-world use: driving LLM CLIs (claude, codex) programmatically while keeping them on their interactive/TTY code path.

## Dependency philosophy (hard rule)

Minimal tree: `pty-process` → `rustix` (+ bitflags, linux-raw-sys). **No tokio** (std threads, deliberately), no serde, no `rand` (SplitMix64 is self-rolled in `jitter.rs`), hand-written VT parser, shell-out to curl/sha256sum for self-update. No new crates without a strong reason — self-roll small things. License policy in `deny.toml`: **only** MIT / Apache-2.0 / Apache-2.0 WITH LLVM-exception; `cargo deny check` is the CI gate.

## Source map (`src/`)

```
lib.rs         crate root, re-exports
error.rs       Error/Result (Pty + Io variants)
pty.rs         PtySession: spawn target in PTY, reader/writer threads + mpsc,
               InputHandle, terminate() (process-group kill), wait_with_timeout
ansi/          parser.rs (byte VT parser, UTF-8/CSI/OSC), strip.rs (AnsiStripper),
               sanitize.rs (line-oriented Canvas/Sanitizer), detect.rs
               (is_confirmation_prompt, is_approval_menu), mod.rs (re-exports)
screen.rs      2D grid emulator for full-screen TUIs: alt-screen, cursor
               addressing, wrap/scroll, DECSTBM scroll regions + SU/SD (#34),
               scrollback (10k), content-diff settle hash. Known limits in
               the module doc (no IL/DL/ICH/DCH/ECH, no REP/DECAWM, CJK = 1 cell)
jitter.rs      per-char human-like delays, SplitMix64 PRNG, type_command
wrapper.rs     Wrapper/WrapperConfig/State/Outcome — wait_until_idle state
               machine (line mode: prompt token + silence; --tui: settled
               screen), watchdog (exec_timeout → Ctrl+C → grace → SIGKILL)
extract.rs     --extract reply extraction (see invariants below)
terminal.rs    RawModeGuard (host raw mode, restore on Drop)
update.rs      self-update: curl/wget + sha256 fail-closed, rename-aside replace
main.rs        CLI: orchestrator (--cmd/--cmd-file), capture (piped stdin),
               interactive (TTY passthrough); subcommands update/version
```

Integration tests: `tests/acceptance.rs`, `tests/cli.rs` (+ `tests/fixtures/`). User docs: `docs/USAGE.md` — update it whenever flags change.

## Build, test, CI

```bash
cargo fmt --check && cargo clippy -- -D warnings && cargo test   # = CI "check" job
cargo deny check                 # license/advisory gate (CI "audit" job, with cargo-audit)
scripts/smoke-local.sh           # smoke tests against the built binary (also in CI)
scripts/smoke-llm.sh             # manual smoke against a real LLM CLI (not in CI)
```

Both CI jobs (`check`, `audit`) are REQUIRED by branch protection; no path filters, everything runs on every PR.

## Workflow conventions

- Trunk-based: short feature branch per change, PR into protected `main`, squash-merge. Never push directly to main.
- **Never merge red CI.** Run fmt+clippy+test and verify green BEFORE committing — do not batch the test run with commit+push in one step.
- Shell fixtures invoked via `sh -c` must be **dash-safe** (CI `sh` is dash; local is bash): dash `printf` does not understand `\xHH` escapes — write multibyte glyphs literally (`printf '● x'`), never as byte escapes. A test that passes locally but fails in CI → suspect the shell first.
- Release: bump `version` in Cargo.toml (+ CHANGELOG.md entry) via a release PR; after merge, tag `vX.Y.Z <merge-sha>` and push the tag — `release.yml` builds linux/macos × x86_64/aarch64 and publishes 8 assets (4 binaries + 4 `.sha256`) to this repo's Releases. A release without the pushed tag is not done.
- This is a PUBLIC repo: no internal hostnames, no absolute paths from private machines, no client names in committed content, issues, or PR text.

## Architecture invariants (learned the hard way)

- **`--extract` is the sole reply extractor** and is a sentinel-first hybrid (`extract.rs::choose_reply`): (1) sentinel-wrap the prompt and try `extract_between` (self-validating); (2) on missing markers with a known CLI, per-CLI structural screen extraction accepted only if `looks_clean` passes (rejects chrome glyphs, box-drawing rules, banner substrings, >400-char lines, leaked `FCB_` fragments); (3) otherwise warn and print nothing — NEVER emit UI chrome. Do **not** reintroduce `--profile` / `--response-marker` (removed in #24; they only worked for short single-line replies). `--extract` stays opt-in: it injects a "wrap your reply between markers" instruction, which only makes sense for an LLM target.
- The structural fallback is per-CLI/chrome-based and may need updating when a target CLI's UI changes. If extraction degrades: re-capture raw ANSI, feed it through the real `Screen`, and extend the emulator (that is how DECSTBM #34 and seam dedup #38 landed). Diagnostic trick: truncate the capture at `\x1b[?1049l` and dump `full_text()` to inspect the seam.
- Agentic runs (the target runs its own multi-step tools) need a LARGE `--idle-ms` (12000+): agents go quiet between steps and a short idle window truncates the capture mid-run.
- `--auto-approve` bypasses the target's own safety gates — strictly OPT-IN, keep default OFF.
- Keep the tool LLM-agnostic: new CLI support goes through the generic mechanisms (sentinels, screen emulator, `looks_clean`), not per-tool hardcoding.
