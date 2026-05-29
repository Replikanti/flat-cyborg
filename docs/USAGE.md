# flat-cyborg — Usage Guide

flat-cyborg wraps an interactive command-line program inside a pseudo-terminal
(PTY) so it believes it is attached to a real terminal. It intercepts both
input and output streams, types input with human-like timing, sanitizes the
ANSI output, and detects when the wrapped program is idle — so you can drive an
interactive CLI non-interactively, or capture its output cleanly.

- [Install](#install)
- [Quick start](#quick-start)
- [Modes](#modes)
- [Options](#options)
- [Driving an LLM CLI (claude, codex)](#driving-an-llm-cli-claude-codex)
- [Exit codes](#exit-codes)
- [Self-update](#self-update)
- [How it works](#how-it-works)
- [Limitations](#limitations)
- [Troubleshooting](#troubleshooting)

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/Replikanti/flat-cyborg/main/install.sh | sh
```

Installs the latest release binary for your platform (Linux/macOS, x86_64 or
aarch64) into `/usr/local/bin`, after verifying its SHA256 checksum. Override
the destination with `FLAT_CYBORG_INSTALL_DIR`:

```sh
curl -fsSL https://raw.githubusercontent.com/Replikanti/flat-cyborg/main/install.sh \
  | FLAT_CYBORG_INSTALL_DIR="$HOME/.local/bin" sh
```

Or build from source:

```sh
cargo build --release   # target/release/flat-cyborg
```

## Quick start

```sh
# Run a program to completion and print its ANSI-stripped output:
flat-cyborg -- sh -c 'printf "\033[32mhello\033[0m\n"'
# -> hello

# Type commands into an interactive shell, waiting for its prompt between them:
flat-cyborg --cmd 'echo one' --cmd 'echo two' --cmd 'exit' -- sh -i

# Wrap an interactive program transparently (your keystrokes drive it):
flat-cyborg -- bash
```

The general form is:

```
flat-cyborg [OPTIONS] -- <program> [args...]
```

Everything after `--` is the target program and its arguments. flat-cyborg's
own flags go before `--`.

## Modes

flat-cyborg picks a mode automatically:

| Mode | When | What it does |
|------|------|--------------|
| **Orchestrator** | one or more `--cmd` given | Types each command into the target (with human-like jitter), waits for the target to return to idle (or exit) between commands, then prints the captured output. |
| **Capture** | no `--cmd`, stdin is not a terminal (e.g. piped) | Runs the target to completion and prints its sanitized output. |
| **Interactive** | no `--cmd`, stdin is a terminal | Puts your terminal in raw mode and forwards your keystrokes to the target while mirroring its raw output — a transparent PTY passthrough. Restored to normal on exit. |

## Options

| Flag | Description |
|------|-------------|
| `--cmd <TEXT>` | Type `TEXT` into the target (repeatable). Selects orchestrator mode. |
| `--timeout-ms <N>` | Per-operation execution timeout before the watchdog intervenes (default 60000). |
| `--idle-ms <N>` | How long output must be silent before the target is considered idle (default 500). Raise it for slow or animated targets. |
| `--prompt <TOKEN>` | Trailing prompt token that marks idle (repeatable; defaults to common shell prompts `$ `, `# `, `> `, `% `). |
| `--no-confirm` | Do not auto-answer `[y/n]` confirmation prompts (by default they are answered `y`). |
| `--profile <NAME>` | Settings bundle for a known LLM CLI (see below). Currently sets `--response-marker`. |
| `--response-marker <S>` | Print only captured lines whose first non-blank content starts with `S`, with `S` stripped. Extracts an assistant's reply lines. |
| `--tui` | Full-screen TUI mode (see below). |
| `-h`, `--help` | Print help. |

### `--tui` mode

Most interactive CLIs are line-oriented (a shell, a REPL). Some are
**full-screen TUIs**: they use the terminal's alternate screen and absolute
cursor positioning to paint and repaint a 2D screen (e.g. an editor, or
Claude Code's UI). For those, pass `--tui`:

- output is captured through a 2D screen-grid emulator instead of a line log;
- the target is considered idle when the **screen content stops changing** for
  `--idle-ms` (there is no line prompt to match);
- the final rendered screen is printed.

A continuously-animated TUI (a spinner that never stops) may never settle —
raise `--idle-ms`, or it will hit `--timeout-ms`.

## Driving an LLM CLI (claude, codex)

You can wrap an LLM coding CLI, send it a prompt, and capture just its answer.

`--profile <name>` is a convenience bundle for a known LLM CLI. Today it sets
the `--response-marker` to the glyph that tool prefixes its reply lines with:

| `--profile` | reply glyph |
|------------|-------------|
| `claude`   | `●` |
| `codex`    | `•` |

Example — ask Claude Code one thing and print only its reply:

```sh
cd ~/your/project        # a directory the CLI already trusts (see note)
flat-cyborg --tui --profile claude --idle-ms 4000 --timeout-ms 120000 \
  --cmd 'Reply with exactly one word: pineapple' \
  -- claude
# -> pineapple
```

What happens: flat-cyborg starts `claude` in a PTY, so it detects a terminal
and launches its **interactive** UI (not headless). `--tui` waits for the UI to
finish rendering, types the prompt, waits for the answer, and `--profile claude`
filters the captured screen down to claude's reply lines.

If a tool's reply glyph changes, or for any CLI without a built-in profile, set
the marker yourself — `--response-marker` always overrides a profile:

```sh
flat-cyborg --tui --response-marker '●' --idle-ms 4000 --cmd '...' -- some-llm-cli
```

Without any marker, `--tui` prints the **entire** final screen (banner, input
box, status bar, and the reply); the marker is what narrows it to just the
answer.

> **Note on onboarding.** Run the LLM CLI in a directory it already trusts.
> On first use in a new directory, these tools show an arrow-key "trust this
> folder" menu (not a `[y/n]` prompt), which the auto-confirm cannot answer and
> `--tui` will wait on until it times out.

This is a best-effort, generic capability — flat-cyborg has no app-specific
code, only the small `--profile` table plus the generic `--response-marker`. A
full-screen TUI is not an API; a CLI's UI can change between versions. For
robust automation prefer a tool's own non-interactive/headless mode or API when
one exists; use flat-cyborg when it does not, or when you specifically need the
interactive path.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Success — target exited 0, or returned to an idle prompt. |
| target's code | In capture/orchestrator mode, the target's own exit status is propagated. |
| `2` | Usage error (bad arguments). |
| `124` | The watchdog timed out and aborted the operation. |

## Self-update

```sh
flat-cyborg update          # update to the latest release
flat-cyborg update --check  # only report whether an update is available
flat-cyborg version         # print the installed version
```

`update` downloads the latest release for your platform, verifies its SHA256
checksum (it refuses to install on a checksum failure unless
`FLAT_CYBORG_INSECURE=1`), and replaces the running binary in place (falling
back to `sudo` if the install directory is not writable).

## How it works

1. **PTY** — the target is spawned in a pseudo-terminal sized 120×40 with
   `TERM=xterm-256color`, inheriting your working directory and environment, so
   it behaves as if launched in a real interactive terminal.
2. **Input jitter** — typed commands are sent one character at a time with
   small randomized delays (alphanumerics 40–120 ms, punctuation 150–300 ms),
   terminated by a carriage return.
3. **Output sanitize** — ANSI escape sequences are parsed out; in line mode a
   single-line emulator collapses progress spinners to their final frame, in
   `--tui` mode a 2D grid renders the visible screen.
4. **State + watchdog** — the wrapper detects RUNNING / CONFIRMATION_PROMPT /
   IDLE and, if an operation does not reach idle within `--timeout-ms`, sends
   `Ctrl+C` and then `SIGKILL` to the target's process group.

## Limitations

- The `--tui` screen emulator is partial: it does not implement scroll regions
  (DECSTBM), insert/delete line/character (IL/DL/ICH/DCH/ECH), repeat (REP), or
  autowrap mode, and counts wide/CJK characters as one cell. Programs that fully
  repaint each frame render faithfully; incrementally-edited screens may show
  minor artifacts.
- Confirmation auto-answer recognizes line-oriented `[y/n]`-style prompts, not
  arrow-key menus.
- Self-update and SIGKILL-of-process-group are Unix (Linux/macOS) features.

## Troubleshooting

| Symptom | Likely cause / fix |
|---------|--------------------|
| Exit `124`, no output | The target never reached idle. Raise `--idle-ms` and/or `--timeout-ms`; for a full-screen TUI add `--tui`. |
| `--tui` capture is full of UI chrome | Add `--profile <name>` or `--response-marker <glyph>` to keep only reply lines. |
| LLM CLI stuck on a "trust this folder" screen | Run it in a directory it already trusts (the menu is arrow-key driven and cannot be auto-answered). |
| `--tui` "has no effect" warning | `--tui` applies to `--cmd` orchestration and piped capture, not interactive passthrough. |
| Typed command seems to race the UI | The target needs longer to render before input; raise `--idle-ms`. |
