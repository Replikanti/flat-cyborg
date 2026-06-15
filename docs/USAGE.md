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
- [Driving an LLM CLI (claude, codex, ...)](#driving-an-llm-cli-claude-codex-)
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
| `--cwd <DIR>` | Run the target with this working directory, instead of inheriting flat-cyborg's own. Useful when you launch flat-cyborg from a parent directory but want the target (e.g. an agent) to act on a specific repo. The directory must exist (a missing one is a usage error, exit `2`). |
| `--auto-approve` | Auto-confirm agent **approval / trust menus** — the arrow-key numbered menus that agentic CLIs show for actions the `[y/n]` auto-confirm cannot answer (e.g. codex's `git push` confirmation, claude's "trust this folder" prompt) — by pressing Enter on the default "yes/proceed/trust" option. **Bypasses the agent's own safety gates (including for destructive actions), so it is opt-in and off by default.** |
| `--extract` | The reply-extraction mechanism (see below): wraps each `--cmd` so the target fences its reply between unique per-run markers and prints only the fenced reply. **Sentinel-strict by default** — if the markers aren't found it prints nothing and warns, so a malformed/refusal reply is empty downstream, never UI chrome. Requires `--cmd`. |
| `--extract-structural` | Opt-in (implies `--extract`): when the markers are absent, fall back to a best-effort, chrome-filtered structural scrape of a known CLI's (claude/codex) screen. Off by default because on a refusal the scrape can return echoed-prompt prose that no chrome filter catches — prefer the strict default for programmatic capture. |
| `--tui` | Full-screen TUI mode (see below). |
| `--no-jitter` | Write each `--cmd` as a fast chunked burst instead of one human-cadenced keystroke at a time (40-300 ms each — minutes for a multi-thousand-char prompt). Use for programmatic LLM drivers where the anti-anomaly typing cadence is not wanted. |
| `--wrap-input <COLS>` | Soft-fold each input line to at most `COLS` columns at word boundaries before sending (default `0` = off). An ultra-long *single* line overflows an Ink-style editor's input field so the prompt is never delivered whole; folding it (the model reads the wrapped text identically) makes a large prompt land reliably. Pairs with `--no-jitter`. |
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

## Driving an LLM CLI (claude, codex, ...)

You can wrap an LLM coding CLI, send it a prompt, and capture just its answer
with `--extract`.

`--extract` works in two layers, sentinel-first:

1. **Sentinel (primary, LLM-agnostic).** It rewrites each `--cmd` so the model
   fences its reply between unique, per-run markers, then prints only the text
   between those markers. Because the markers are injected into the prompt
   rather than scraped from the CLI's visible UI, this works for **any** LLM
   CLI, captures **multi-line** answers in full, and (thanks to the virtual
   terminal's scrollback) handles **long** answers that scroll off screen.
   If the markers are absent (a refusal/clarification, or a CLI that drops them),
   the default is **strict**: flat-cyborg prints nothing and warns on stderr, so
   downstream "is stdout empty?" cleanly means "no reply". It never scrapes the
   screen by default.
2. **Structural fallback — opt-in via `--extract-structural`.** Some CLIs ignore
   the wrap instruction for longer answers (codex does). With
   `--extract-structural`, when the markers are absent and the target is a known
   CLI (`claude`, `codex`), flat-cyborg falls back to slicing the reply out of
   the rendered screen by recognizing that tool's chrome, accepted only if it
   passes a strict cleanliness check (no UI glyphs, separators, banners, or
   runaway lines). This is **off by default**: on a refusal the scrape can return
   echoed wrap-instruction prose — which carries no chrome glyph, so the
   cleanliness check passes it — and hand a programmatic consumer a fragment
   indistinguishable from a real reply. Prefer the strict default for capture;
   use the opt-in only for best-effort human convenience.

The structural fallback is per-CLI and recognizes the current claude/codex UI,
so it may need updating if those tools redesign their interface; the sentinel
layer (and, when opted in, the cleanliness check) bound the blast radius — the
default worst case is a loud warning + empty stdout, never garbage.

Example — ask Claude Code one thing and print only its reply:

```sh
cd ~/your/project        # a directory the CLI already trusts (see note)
flat-cyborg --tui --extract --idle-ms 4000 --timeout-ms 120000 \
  --cmd 'Reply with exactly one word: pineapple' \
  -- claude
# -> pineapple
```

What happens: flat-cyborg starts `claude` in a PTY, so it detects a terminal
and launches its **interactive** UI (not headless). `--tui` waits for the UI to
finish rendering, then flat-cyborg types the wrapped prompt, waits for the
answer, and prints only what the model fenced between the markers.

`--extract` needs at least one `--cmd` (it has to have a prompt to wrap). If the
markers are not found, flat-cyborg prints nothing to stdout and warns on stderr
(strict default) — it never dumps the raw screen. Pass `--extract-structural`
to opt into the best-effort structural scrape described above.

Without `--extract`, `--tui` prints the **entire** final screen (banner, input
box, status bar, and the reply); `--extract` is what narrows it to just the
answer.

> **Note on onboarding.** Run the LLM CLI in a directory it already trusts, or
> point it at one with `--cwd <repo>`. On first use in a new directory, these
> tools show an arrow-key "trust this folder" menu (not a `[y/n]` prompt), which
> the `[y/n]` auto-confirm cannot answer; `--tui` would otherwise wait on it
> until it times out. Pass `--auto-approve` to have flat-cyborg confirm such
> menus for you (see the safety note below).

> **Note on multi-step agent actions.** When you drive an agent to write files,
> run `git`, or open a PR, it may pause on an arrow-key **approval menu** (e.g.
> codex confirming a `git push`) that the `[y/n]` auto-confirm cannot answer.
> Pass `--auto-approve` to confirm these automatically. This **bypasses the
> agent's own safety gates** (including for destructive actions), so use it
> deliberately — prefer running the agent in a mode/dir that does not prompt
> when you do not need it.
>
> Multi-step agent runs also need a **larger `--idle-ms`**: the agent goes quiet
> for several seconds between steps (thinking, running a tool), and a small idle
> window makes flat-cyborg declare IDLE mid-run and cut the capture short. For
> agentic git/PR workflows use `--idle-ms 12000` or more.

This is a best-effort, generic capability — flat-cyborg has no app-specific
code; `--extract` is fully LLM-agnostic. A full-screen TUI is not an API; a
CLI's UI can change between versions. For robust automation prefer a tool's own
non-interactive/headless mode or API when one exists; use flat-cyborg when it
does not, or when you specifically need the interactive path.

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
| `--tui` capture is full of UI chrome | Add `--extract` (with `--cmd`) to print only the model's fenced reply. |
| LLM CLI stuck on a "trust this folder" screen | The menu is arrow-key driven, so the `[y/n]` auto-confirm cannot answer it. Run the CLI in a directory it already trusts, or pass `--auto-approve` to confirm the trust menu (it bypasses the agent's safety gate, so use deliberately). |
| Agent stalls on an approval menu (codex `git push`, etc.) | Pass `--auto-approve` to auto-confirm agent approval/trust menus. **It bypasses the agent's safety gates** (including destructive actions); alternatively run the agent in a pre-trusted dir or a mode that does not prompt. |
| Target says "not a git repository" / acts on the wrong directory | flat-cyborg inherits its own working directory by default, so an agent launched from a parent dir sees the wrong CWD. Point the target explicitly with `--cwd <repo>`. |
| `--tui` "has no effect" warning | `--tui` applies to `--cmd` orchestration and piped capture, not interactive passthrough. |
| Typed command seems to race the UI | The target needs longer to render before input; raise `--idle-ms`. |
