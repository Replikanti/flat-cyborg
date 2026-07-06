# Security

## What flat-cyborg does, from a security standpoint

flat-cyborg executes an **arbitrary program** — everything after `--` — inside a
pseudo-terminal that convinces that program it is attached to a real interactive
TTY. It then types into the program and reads everything it prints. Treat a
flat-cyborg invocation exactly like running the target program yourself, with
the same user, working directory, and environment: flat-cyborg adds no
sandboxing, no privilege separation, and no filtering of what the target does.

Points worth understanding before automating with it:

- **`--auto-approve` bypasses the target's own safety gates.** Agentic CLIs
  pause on arrow-key approval/trust menus precisely so a human can veto
  destructive actions (pushing to a remote, trusting a directory). With
  `--auto-approve`, flat-cyborg presses Enter on the default option for the
  agent — including for destructive actions. It is opt-in and off by default;
  turn it on only for workflows where you have already decided every action the
  agent may take is acceptable.
- **The default `[y/n]` auto-confirm answers `y`.** Disable it with
  `--no-confirm` if the target may ask questions you would not answer yes to.
- **Prompts and replies transit the PTY in the clear.** Anything you send via
  `--cmd`/`--cmd-file` and everything the target prints is held in memory and
  written to stdout. Do not put secrets in prompts you would not pass on a
  command line; prefer `--cmd-file` with restrictive file permissions when the
  prompt is sensitive (argv is world-readable via `/proc` on most systems).
- **Watchdog kills are process-group `SIGKILL`.** On timeout, after a `Ctrl+C`
  and a grace period, the whole process group is killed. Anything the target
  spawned that escaped its process group survives; do not rely on the watchdog
  as a resource-containment boundary.

## Self-update integrity

`flat-cyborg update` (and `install.sh`) download release binaries over HTTPS
from this repository's GitHub Releases and **fail closed** on SHA256 checksum
mismatch. Setting `FLAT_CYBORG_INSECURE=1` skips checksum verification — never
do this in automation; it exists for air-gapped debugging only.

## Supply chain

The dependency tree is deliberately minimal (`pty-process` → `rustix`, no async
runtime) and license/advisory-gated in CI by `cargo deny` and `cargo audit`.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting ("Report a vulnerability"
under this repository's **Security** tab) rather than a public issue for
anything exploitable — e.g. a way to make `--extract` emit attacker-controlled
text as a trusted reply, an escape from the strict sentinel contract, or a
checksum-verification bypass in the updater.
