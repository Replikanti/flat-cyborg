# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.10.2]: https://github.com/Replikanti/flat-cyborg/compare/v0.10.1...v0.10.2
[0.10.1]: https://github.com/Replikanti/flat-cyborg/compare/v0.10.0...v0.10.1
[0.10.0]: https://github.com/Replikanti/flat-cyborg/releases/tag/v0.10.0
