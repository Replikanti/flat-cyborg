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
| **Orchestrator** | one or more `--cmd` / `--cmd-file` given | Types each command into the target (with human-like jitter), waits for the target to return to idle (or exit) between commands, then prints the captured output. |
| **Capture** | no `--cmd`, stdin is not a terminal (e.g. piped) | Runs the target to completion and prints its sanitized output. |
| **Interactive** | no `--cmd`, stdin is a terminal | Puts your terminal in raw mode and forwards your keystrokes to the target while mirroring its raw output — a transparent PTY passthrough. Restored to normal on exit. |

## Options

| Flag | Description |
|------|-------------|
| `--cmd <TEXT>` | Type `TEXT` into the target (repeatable). Selects orchestrator mode. |
| `--cmd-file <PATH>` | Like `--cmd` but read the prompt text from `PATH`. Use for large prompts: a multi-megabyte prompt passed as an argv value overflows `ARG_MAX` (the `Argument list too long` limit); a file does not. Repeatable, combines with `--cmd`; selects orchestrator mode. |
| `--timeout-ms <N>` | Per-operation execution timeout before the **graceful** watchdog intervenes (default 60000): on expiry it sends `Ctrl+C`, waits a grace, then SIGKILLs (exit `124`). |
| `--hard-timeout-ms <N>` | Absolute per-operation wall-clock ceiling (default: equal to `--timeout-ms`, or `$FLAT_CYBORG_HARD_TIMEOUT_MS` when set). Unlike `--timeout-ms` this is checked **regardless of output state**, so it bounds even a continuously-animating TUI that never settles — where the `--timeout-ms` completion path can never fire and the reply wait would otherwise ride the full timeout. On breach the target is SIGKILLed immediately (no graceful `Ctrl+C`) and the run exits `69` (`EX_UNAVAILABLE`), a **retryable transient** distinct from the watchdog's ambiguous `124`. Applies per operation, like `--timeout-ms`. The env form exists for drivers with a fixed argv builder; an explicit flag wins over it. |
| `--idle-ms <N>` | How long output must be silent before the target is considered idle (default 500). Raise it for slow or animated targets. Under `--extract` it is a **latency** knob, not a correctness one: completion is gated on the model's closing marker, so a think-pause longer than `--idle-ms` no longer cuts the reply short (see [Completion: what ends the reply wait](#completion-what-ends-the-reply-wait)). |
| `--prompt <TOKEN>` | Trailing prompt token that marks idle (repeatable; defaults to common shell prompts `$ `, `# `, `> `, `% `). |
| `--no-confirm` | Do not auto-answer `[y/n]` confirmation prompts (by default they are answered `y`). |
| `--cwd <DIR>` | Run the target with this working directory, instead of inheriting flat-cyborg's own. Useful when you launch flat-cyborg from a parent directory but want the target (e.g. an agent) to act on a specific repo. The directory must exist (a missing one is a usage error, exit `2`). |
| `--cols <N>` | PTY width for the target, in columns (default `120`, or `$FLAT_CYBORG_COLS` when set; `40`-`4000`). An Ink-style TUI soft-wraps any reply line longer than the terminal at word boundaries with a hanging indent, so a screen-read `--extract` reply comes back re-wrapped — a long `\|`-delimited protocol line loses its trailing fields to the continuation lines. Widen the PTY so long single-line replies stay whole. The env form exists for drivers with a fixed argv builder; an explicit flag wins over it. |
| `--auto-approve` | Auto-confirm agent **approval / trust menus** — the arrow-key numbered menus that agentic CLIs show for actions the `[y/n]` auto-confirm cannot answer (e.g. codex's `git push` confirmation, claude's "trust this folder" prompt) — by pressing Enter on the default "yes/proceed/trust" option. **Bypasses the agent's own safety gates (including for destructive actions), so it is opt-in and off by default.** |
| `--extract` | The reply-extraction mechanism (see below): wraps each `--cmd` so the target fences its reply between unique per-run markers and prints only the fenced reply. **Sentinel-strict by default** — if the markers aren't found it prints nothing and warns, so a malformed/refusal reply is empty downstream, never UI chrome. Requires `--cmd`. |
| `--extract-structural` | Opt-in (implies `--extract`): when the markers are absent, fall back to a best-effort, chrome-filtered structural scrape of a known CLI's (claude/codex) screen. Off by default because on a refusal the scrape can return echoed-prompt prose that no chrome filter catches — prefer the strict default for programmatic capture. Completion stays gated on the closing marker; a marker-less reply completes once the output has been quiet for `--extract-grace-ms`. |
| `--extract-grace-ms <MS>` | How long the output must be **continuously quiet** before a reply *without* the closing marker is accepted as finished. With `--extract-structural` the default is `min(max(4 × --idle-ms, 30000), --timeout-ms / 2)` — a 30 s floor, scaled up for a driver that already expects long silences, and kept well inside the watchdog budget. Whatever the value, a settled screen is accepted at the latest in the final `--idle-ms` before `--timeout-ms`, so waiting for the marker never costs a `124`. `0` completes on the first settled screen (the pre-0.13.0 `--extract-structural` behavior); strict `--extract` has no grace unless this flag sets one. |
| `--transcript-dir <DIR>` | Directory of the target's own reply transcripts (default `$HOME/.claude/projects`). Under `--extract` with a claude target, the sentinel-fenced reply is recovered from the transcript **first** — it is authoritative and independent of how the TUI renders the reply, so a reply too long to fit (or collapsed off) the rendered screen is still captured whole; the screen scrape stays the fallback. Recovery keys on the run's unique sentinel (immune to path sanitization and concurrent sessions) and no-ops when the target is not claude, transcript saving is off, or no fence is present. |
| `--no-transcript-read` | Disable transcript recovery; extract from the rendered screen only (the pre-transcript behavior). |
| `--result-file <PATH>` | Ask the target to **also** write its complete reply to `PATH` with its own file-writing tool, and read the reply from `PATH` **first** — preferred over the transcript and the screen (**file > transcript > screen**). A file the model writes is exact bytes, immune to reply size, TUI line-wrap, and a dark transcript (a driven *interactive* claude persists no `.jsonl`, so the transcript leg is empty on that path). Requires `--extract` (the directive rides its sentinel wrap). Defaults to `$FLAT_CYBORG_RESULT_FILE_PATH` when set (an explicit flag wins) — note this is the PATH env, distinct from a consuming repo's own boolean `FLAT_CYBORG_RESULT_FILE` gate. On a **non-empty** `PATH` the run exits `0` even on a timeout / mid-reply exit (the answer already reached the caller); an empty or unwritten `PATH` logs one stderr line and falls back to transcript/screen extraction, **byte-identical** to a run without the flag. The caller must pass a path the target can write (e.g. under a directory the sandbox binds read-write at the same host path); flat-cyborg only reads it. Pair it with `--extract-structural` so the run completes on a settled screen rather than riding `--timeout-ms` when the model omits the sentinel. |
| `--tui` | Full-screen TUI mode (see below). |
| `--no-jitter` | Write each `--cmd` as a fast chunked burst instead of one human-cadenced keystroke at a time (40-300 ms each — minutes for a multi-thousand-char prompt). Use for programmatic LLM drivers where the anti-anomaly typing cadence is not wanted. Best-effort for large prompts only: as a precaution it errors out above a conservative size guardrail (a policy line, not a proven boundary; measured *after* `--wrap-input` folding) and directs you to `--paste-input`, which delivers large prompts deterministically. |
| `--wrap-input <COLS>` | Soft-fold each input line to at most `COLS` columns at word boundaries before sending (default `0` = off). An ultra-long *single* line overflows an Ink-style editor's input field so the prompt is never delivered whole; folding it (the model reads the wrapped text identically) makes a large prompt land reliably. Pairs with `--no-jitter` — but for large prompts prefer `--paste-input`, which needs no folding and is not size-capped. (Folding inserts line breaks, so it *grows* the byte count the `--no-jitter` size guardrail measures.) |
| `--paste-input` | Deliver each `--cmd` via **bracketed paste** (`ESC[200~` + body + `ESC[201~`) then a settled Enter, instead of typing it. An editor in bracketed-paste mode (claude/codex) takes the whole block atomically — no per-line submit, no length overflow, no chunk-timing heuristic — a deterministic alternative to `--no-jitter` (and `--wrap-input` is unnecessary under it). Takes precedence over `--no-jitter`. |
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

### Completion: what ends the reply wait

With `--extract` (both modes), the **closing marker is the completion signal**.
The model emits it as the last thing in its reply, so the moment it appears
flat-cyborg stops waiting and captures — no guessing about model latency.

That matters because the obvious alternative — "the screen has been quiet for
`--idle-ms`" — is a guess: a large model pauses to think mid-reply, the screen
settles, and a settle-only completion captures a screen that does not contain
the answer yet (the scrape then returns UI chrome or echoed prompt prose). This
is why `--idle-ms` is a **latency** knob under `--extract`, not a correctness
one: raising it buys nothing but patience, and no value is ever provably enough.

A reply *without* the marker (a refusal, or a CLI that drops it) still
completes, but only after the output has been continuously quiet for
`--extract-grace-ms` — a bounded fallback, not the primary signal:

- `--extract-structural` defaults it to
  `min(max(4 × --idle-ms, 30000), --timeout-ms / 2)`. The 30 s floor is the
  quiet window a large model actually needs; the `4 ×` term honors a driver
  that already tells us to expect long silences (`--idle-ms 12000` → 48 s);
  the `--timeout-ms / 2` cap keeps the grace well inside the watchdog budget so
  a marker-less reply normally completes on quiet rather than on the deadline.
- **The grace never costs you a timeout.** It is measured from the last change
  on screen, so a reply whose final chunk lands late has less than a full grace
  of budget left; rather than sit on a good settled screen until the watchdog
  fires, flat-cyborg accepts it in the final `--idle-ms` before `--timeout-ms`.
  That holds for any value, including an explicit `--extract-grace-ms` longer
  than the whole timeout.
- Strict `--extract` has no grace unless you pass one: with no structural
  fallback there is nothing to recover from a marker-less run anyway, and the
  watchdog is the backstop.
- **A never-settling animating TUI is bounded by the hard cap, not the grace.**
  The grace (and the whole settle path) only fires once the output falls quiet;
  a target that repaints continuously — an animated spinner or "thinking" hint
  that never stops — never settles, so `Output::Idle` never fires and the reply
  wait rides the full `--timeout-ms`. `--hard-timeout-ms` (default: equal to
  `--timeout-ms`) is the absolute wall-clock bound checked regardless of output
  state, so this case ends promptly at the cap with a retryable exit `69`
  instead of the graceful watchdog's `124`.
- `--extract-grace-ms 0` restores the pre-0.13.0 `--extract-structural`
  behavior (the first settled screen completes the run) — an escape hatch and
  an A/B control arm.

When a run finishes on the grace instead of the marker, flat-cyborg says so on
stderr:

```text
flat-cyborg: --extract: no closing sentinel; completed on the marker-less grace (30000 ms)
```

The number is how long the output had *actually* been quiet when the reply was
accepted — normally the grace, but shorter when the run was accepted on the
watchdog-budget bound described above.

Treat that line as a signal: it separates "the model genuinely had no answer"
from "we captured the screen too early", and a high rate of it means the target
CLI is not rendering the marker as its own line. It is printed only when that
fallback is what completed the run, so a target that exits on its own (or is cut
short by the watchdog) never appears in the count.

#### When the target closes the PTY (EOF)

Under `--extract` there are three ways a run can end when the target's PTY
closes, and flat-cyborg distinguishes them by exit code:

- **Clean exit after a fenced reply → exit `0`.** The closing marker completes
  the wait *before* EOF is read (the reply is captured on the marker), so a
  target that then quits is a normal success.
- **Mid-reply death with the reply LOST → exit `75`.** The target closed the PTY
  un-interrupted with the completion gate never opened (e.g. it crashed, or was
  OOM-killed under load) **and** no reply could be recovered — so the answer the
  caller asked for never arrived. This is a transient the caller may retry
  (`EX_TEMPFAIL`); see the Exit codes table. **But a missing closing marker is not
  a lost reply:** under `--extract-structural`, if the chrome-free structural
  fallback still recovers the reply from the settled screen, the answer reached
  stdout and the run exits `0`, not `75`. Without `--extract` there is no gate, so
  a plain-capture exit stays a `Completed` passthrough of the target's own code.
- **Watchdog abort → exit `124`.** flat-cyborg itself interrupted the target
  after `--timeout-ms` (the marker never appeared and the screen never settled).

### Capturing the reply as a file (`--result-file`)

The screen scrape and the transcript both depend on the target's UI: a long
reply can be re-wrapped or collapsed off the rendered screen, and a driven
*interactive* claude (subscription, not `claude -p`) persists **no** `.jsonl`,
so the transcript leg is empty on that path. `--result-file <PATH>` sidesteps
both by asking the model to write its complete reply to a file with its own
file-writing tool — exact bytes, independent of how the UI renders them:

```console
flat-cyborg --extract --extract-structural --result-file "$RUN/reply.txt" \
    --idle-ms 12000 --timeout-ms 240000 \
    --cmd "$prompt" -- claude
```

- **Precedence is file > transcript > screen.** The file is read first; the
  transcript and the screen scrape stay as fallbacks, so the mode is strictly
  additive.
- **Fallback is byte-identical.** If the file is empty, missing, or unreadable
  (a weak model or a tool refusal), flat-cyborg logs one stderr line naming the
  no-op and falls through to today's transcript→screen extraction unchanged.
- **A non-empty file exits `0`** even if the run would otherwise time out or the
  target died mid-reply — the answer already reached the caller. This override is
  scoped strictly to the file-hit path; every non-file outcome keeps its code.
- **The caller owns the path.** flat-cyborg never creates it and only reads it,
  so it must be somewhere the target can write. A sandboxed target gets a fresh
  `/tmp`, so a host temp path is invisible inside it: pass a path under a
  directory the sandbox binds read-write at the identical host path (e.g. the run
  directory), and both the model (inside) and flat-cyborg (on the host) see the
  same file. `$FLAT_CYBORG_RESULT_FILE_PATH` supplies the default for drivers
  with a fixed argv builder; an explicit `--result-file` wins.
- **Pair it with `--extract-structural`** so a marker-less run still completes on
  a settled screen (bounded) instead of riding the full `--timeout-ms`.

### Large prompts

Three limits bite when the prompt gets big, each with its own flag:

1. **The OS argument limit.** A prompt passed via `--cmd` lives on the command
   line; past `ARG_MAX` (typically ~2 MB total argv) the kernel refuses to exec
   at all (`Argument list too long`). Put the prompt in a file and pass
   `--cmd-file prompt.txt` instead.
2. **Typing time.** The default human-cadence jitter types one character at a
   time — minutes for a multi-thousand-character prompt. Pass `--paste-input`
   (bracketed paste, atomic and deterministic — preferred) or `--no-jitter`
   (fast chunked burst). Burst delivery of large prompts is only best-effort —
   it can mis-deliver depending on the prompt's *shape*, not just its size — so
   as a precaution `--no-jitter` **errors out** above a conservative size
   guardrail and points you to `--paste-input`; use that for anything large.
   The guardrail is a policy line, not a proven safe boundary: it catches the
   clearly-oversized case, but it does not guarantee that every sub-guardrail
   burst delivers. The byte count in that error is measured *after*
   `--wrap-input` folding (which inserts line breaks), so it can exceed your
   raw input length.
3. **Editor line overflow** (burst path only). An ultra-long single line can
   overflow an Ink-style editor's input field; `--wrap-input 72` soft-folds it.
   Not needed under `--paste-input`.

Putting it together for a programmatic driver:

```sh
flat-cyborg --extract --paste-input --idle-ms 30000 --timeout-ms 240000 \
  --cmd-file prompt.txt -- claude
```

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
> Multi-step agent runs also want a **larger `--idle-ms`**: the agent goes quiet
> for several seconds between steps (thinking, running a tool). Without
> `--extract` a small idle window makes flat-cyborg declare IDLE mid-run and cut
> the capture short, so use `--idle-ms 12000` or more for agentic git/PR
> workflows. With `--extract` the closing marker ends the wait instead (see
> [Completion](#completion-what-ends-the-reply-wait)), so `--idle-ms` only tunes
> how quickly a *marker-less* reply is accepted — do not ratchet it up when a
> run comes back empty; that is a completion-path symptom, not an idle-window
> one.

This is a best-effort, generic capability — flat-cyborg has no app-specific
code; `--extract` is fully LLM-agnostic. A full-screen TUI is not an API; a
CLI's UI can change between versions. For robust automation prefer a tool's own
non-interactive/headless mode or API when one exists; use flat-cyborg when it
does not, or when you specifically need the interactive path.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Success — target exited 0, or returned to an idle prompt. Also returned when `--result-file` holds a non-empty reply, even if the outcome would otherwise be `69`/`75`/`124`: the answer reached the caller via the file (the exit-0-on-file-hit override, scoped strictly to the file-hit path). |
| target's code | In capture/orchestrator mode, the target's own exit status is propagated. |
| `1` | Generic failure — flat-cyborg itself failed (e.g. the target could not be spawned), or the target was killed by a signal. |
| `2` | Usage error (bad arguments). |
| `69` | The absolute hard cap (`--hard-timeout-ms`) fired — the target streamed continuously and never fenced (or settled) its reply within the ceiling, so it was SIGKILLed immediately (no graceful `Ctrl+C`). `69` is `EX_UNAVAILABLE`: a **retryable transient**, distinct from the watchdog's ambiguous `124`. This is the bound that fires under a never-settling animating TUI, where the `--timeout-ms` completion path cannot. |
| `75` | The target exited **mid-reply** and the reply was **lost** — it closed the PTY, un-interrupted, under `--extract` with the gate never opened, and no reply could be recovered (not even by the `--extract-structural` fallback). `75` is `EX_TEMPFAIL` ("temporary failure; retry"): a **transient** the caller may re-run. It overrides the target's own passthrough status on this arm only. A missing closing marker alone is **not** a lost reply: if `--extract-structural` still recovers the answer from the settled screen it returns `0`; a clean reply-then-exit returns `0`; and plain capture (no `--extract`) keeps propagating the target's own code. |
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
| Exit `75` under `--extract` | The target vanished mid-reply (it closed the PTY before fencing its answer — e.g. it crashed or was killed under load). This is a **transient**, not a flat-cyborg fault or a usage error: re-run the command. If it recurs, reduce concurrent sessions or check the target CLI's own logs. |
| `--tui` capture is full of UI chrome | Add `--extract` (with `--cmd`) to print only the model's fenced reply. |
| `--extract` output is empty or looks like chrome on a slow model | The reply was captured before the model finished. Do **not** raise `--idle-ms`: completion is gated on the closing marker, so check stderr for `completed on the marker-less grace` — if it is there, the target never rendered the marker on its own line. A `124` here means the screen never settled at all (a continuously animated UI), not that the grace was too long: raise `--timeout-ms`. |
| LLM CLI stuck on a "trust this folder" screen | The menu is arrow-key driven, so the `[y/n]` auto-confirm cannot answer it. Run the CLI in a directory it already trusts, or pass `--auto-approve` to confirm the trust menu (it bypasses the agent's safety gate, so use deliberately). |
| Agent stalls on an approval menu (codex `git push`, etc.) | Pass `--auto-approve` to auto-confirm agent approval/trust menus. **It bypasses the agent's safety gates** (including destructive actions); alternatively run the agent in a pre-trusted dir or a mode that does not prompt. |
| Target says "not a git repository" / acts on the wrong directory | flat-cyborg inherits its own working directory by default, so an agent launched from a parent dir sees the wrong CWD. Point the target explicitly with `--cwd <repo>`. |
| `--tui` "has no effect" warning | `--tui` applies to `--cmd` orchestration and piped capture, not interactive passthrough. |
| Typed command seems to race the UI | The target needs longer to render before input; raise `--idle-ms`. |
