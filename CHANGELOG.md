# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.12.1] — 2026-07-15

### Fixed

- `--cmd` and `--cmd-file` are now mutually exclusive: using both together
  produces a clear error. Previously, when both were provided, the tool would
  silently prefer one over the other depending on parse order, creating an
  ambiguous user-error trap.
- `--cmd-file` content is now `.trim_end()`'d before use, matching the behavior
  of `--cmd` (which receives argv values already trimmed by the shell). This
  fixes whitespace-parity inconsistency where file-sourced prompts retained
  trailing newlines while argv-sourced ones did not.
- Added `#[derive(Debug)]` to `Args` and `Mode` for improved debugging support.

## [0.12.0] — 2026-07-15

### Changed

- `--no-jitter` burst input path now enforces a conservative `BURST_MAX_BYTES` size
  guardrail: prompts larger than ~4 KB sent via burst mode fail loudly with a clear
  error directing the user to `--paste-input`, instead of the previous behavior
  (silent mis-delivery to stdout/garbage replies). The guardrail is a precaution
  against prompt-shape-dependent delivery failures (issue #60); it does not guarantee
  reliability below the line. Large prompts must use `--paste-input` for deterministic
  delivery.
- Updated `bitflags` dependency to 2.13.0 to clear yanked-crate warnings.

## [0.11.0] — 2026-06-20

### Added

- `--cmd-file <PATH>` — read the prompt text from a file instead of an argv
  value. A multi-MB prompt passed as `--cmd <TEXT>` overflows `ARG_MAX`
  (`exec` fails E2BIG / "Argument list too long"); reading it from a file
  does not. Repeatable; selects orchestrator mode exactly like `--cmd`.
  Found driving the agentis dev-apprenticeship federation on a real repo,
  where agents build multi-MB contexts (Replikanti/agentis-colonies#1171).

## [0.10.2] — 2026-06-16

### Fixed

- `--extract-structural` now completes on a SETTLED screen instead of marker-gating
  IDLE. Strict `--extract` waits for the closing sentinel; if the model omits it
  (claude intermittently refuses the wrap protocol) the gate never opens, the wrapper
  burns the entire `--timeout-ms` and exits "no fenced reply", and a programmatic
  caller retries — observed as repeated ~700 s hangs that end in failure. With
  `--extract-structural` the IDLE gate is now `None` (a settled screen is idle, like
  `--tui`) and the reply is recovered marker-first → structural-fallback: fast AND
  marker-less-tolerant. Strict `--extract` (no `--extract-structural`) is unchanged.
  Fixes #55.

## [0.10.1] — 2026-06-16

### Fixed

- `--extract` returned an empty reply when driving claude's interactive TUI.
  Two compounding causes, both fixed (verified live against claude v2.1.178):
  - The sentinel idle-gate was applied to the pre-typing **readiness wait** as
    well as the reply wait. The closing marker cannot appear before the prompt
    is even typed, so that readiness wait never completed — it burned the whole
    watchdog, `send()` never ran, the prompt was never delivered, and the reply
    was empty. The gate is now scoped to the post-typing reply wait only.
  - Completion now fires the moment the closing sentinel marker appears, not
    only on screen silence: claude's idle TUI animates (rotating hints) and may
    never fall silent, so the silence-gated path alone would hit the watchdog.
  Fixes #53.

## [0.10.0]

- Reliable long-prompt delivery + sentinel-aware idle gate (#46).
- `--extract` is sentinel-strict by default; structural fallback is opt-in via
  `--extract-structural` (#50).
- `codex --extract`: single-line `wrap_command` + line-end fence integrity (#40).
- `--paste-input`: bracketed-paste input delivery, opt-in (#49).

[0.12.1]: https://github.com/Replikanti/flat-cyborg/compare/v0.12.0...v0.12.1
[0.12.0]: https://github.com/Replikanti/flat-cyborg/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/Replikanti/flat-cyborg/compare/v0.10.2...v0.11.0
[0.10.2]: https://github.com/Replikanti/flat-cyborg/compare/v0.10.1...v0.10.2
[0.10.1]: https://github.com/Replikanti/flat-cyborg/compare/v0.10.0...v0.10.1
[0.10.0]: https://github.com/Replikanti/flat-cyborg/releases/tag/v0.10.0
