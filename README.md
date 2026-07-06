# flat-cyborg

[![CI](https://github.com/Replikanti/flat-cyborg/actions/workflows/ci.yml/badge.svg)](https://github.com/Replikanti/flat-cyborg/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Replikanti/flat-cyborg)](https://github.com/Replikanti/flat-cyborg/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/Replikanti/flat-cyborg/total)](https://github.com/Replikanti/flat-cyborg/releases)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)


An asynchronous pseudo-terminal (PTY) wrapper, written in Rust, for
bidirectional I/O interception of interactive command-line applications.

flat-cyborg encapsulates an interactive **Target CLI** inside a virtual PTY so
the target detects a fully-featured TTY and launches in interactive mode rather
than headless mode. It intercepts both I/O streams, emulates human-like input
timing, and deterministically detects the target's lifecycle state by parsing
the output ANSI stream.

**Why it exists:** some CLIs only expose their full capability — or a different
billing model — in their interactive mode. The motivating case is AI coding
CLIs (Claude Code, codex): driving the *interactive* session programmatically
lets an automation pipeline run on a flat-rate subscription seat instead of a
metered API, and reach features the headless mode lacks. flat-cyborg makes that
interactive session scriptable: send a prompt, wait for the real answer, print
only the reply (`--extract`). It stays fully generic — any interactive CLI, no
app-specific code.

## Capabilities

- **PTY & process lifecycle** — spawns the target inside a master/slave PTY pair
  with a fixed geometry (120×40), `TERM=xterm-256color`, and inherited working
  directory and environment. The master end is multiplexed by dedicated
  reader/writer threads (no async runtime), so the caller's thread never blocks
  on PTY I/O.
- **Input jittering** — commands are decomposed into individual UTF-8 characters
  and written with pseudo-random inter-character delays (alphanumerics
  40–120 ms, punctuation/separators 150–300 ms), terminated with a carriage
  return (`\r`).
- **Output ANSI state machine** — a streaming parser strips ANSI escape
  sequences in real time. The wrapper classifies the sanitized stream into a
  `State` (`Running`, `ConfirmationPrompt`, `Idle`); confirmation prompts (e.g.
  `[y/n]`) are answered automatically through the input jitterer.
- **Safety watchdogs** — every operation runs under an execution timeout. On
  timeout the wrapper attempts graceful degradation: `Ctrl+C`, then `SIGKILL` of
  the whole process group after a grace period. The interactive front-end that
  puts the host terminal in raw mode restores it to canonical mode on exit or
  panic via an RAII guard.

## Status

Actively developed and in production use — it is the LLM-backend driver of the
[agentis-colonies](https://github.com/Replikanti/agentis-colonies) federations,
where it runs unattended around the clock. Changes land incrementally via
trunk-based development; releases are tagged with prebuilt binaries for
Linux/macOS × x86_64/aarch64. See the [releases page](https://github.com/Replikanti/flat-cyborg/releases)
and the issue tracker for current state.

## Installing

Install the latest release binary for your platform (Linux/macOS, x86_64 or
aarch64). The script verifies the SHA256 checksum and installs to
`/usr/local/bin` (override with `FLAT_CYBORG_INSTALL_DIR`):

```sh
curl -fsSL https://raw.githubusercontent.com/Replikanti/flat-cyborg/main/install.sh | sh
```

Prefer to inspect first? Download `install.sh`, read it, then run it — or grab a
binary and its `.sha256` directly from the [releases page](https://github.com/Replikanti/flat-cyborg/releases).

## Usage

```sh
# Run a program and print its ANSI-stripped output:
flat-cyborg -- sh -c 'printf "\033[32mhello\033[0m\n"'

# Drive an interactive shell non-interactively:
flat-cyborg --cmd 'echo hi' --cmd 'exit' -- sh -i

# Wrap an LLM CLI and capture just its reply:
flat-cyborg --tui --extract --idle-ms 4000 \
  --cmd 'Reply with one word: pineapple' -- claude

# Large prompt? Read it from a file (no ARG_MAX limit) and deliver it
# atomically via bracketed paste:
flat-cyborg --extract --paste-input --cmd-file prompt.txt -- claude
```

See the [**Usage Guide**](docs/USAGE.md) for the full reference — modes, every
option, `--tui` and `--extract`, large-prompt delivery (`--cmd-file`,
`--paste-input`, `--no-jitter`, `--wrap-input`), exit codes, self-update, and
troubleshooting. Security posture and vulnerability reporting:
[SECURITY.md](SECURITY.md).

Update an installed binary with `flat-cyborg update` (`flat-cyborg version` to
check the installed version).

## Building

```sh
cargo build
cargo test
```

End-to-end smoke tests: `scripts/smoke-local.sh` drives the built binary
against `sh`/`cat` targets (deterministic, no network — also run in CI), and
`scripts/smoke-llm.sh` drives real `claude`/`codex` CLIs to check
`--extract`/`--auto-approve` (opt-in; needs the CLIs installed and a trusted
working directory).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
