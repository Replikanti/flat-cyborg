//! Demo front-end for the flat-cyborg PTY wrapper.
//!
//! Usage:
//!
//! ```text
//! flat-cyborg [OPTIONS] -- <program> [args...]
//! ```
//!
//! Modes (selected automatically):
//!
//! - **Orchestrator** — if one or more `--cmd <text>` are given, each is typed
//!   into the target (jittered), the wrapper waits for the target to return to
//!   IDLE / exit between commands, and the sanitized log is printed at the end.
//! - **Capture** — with no `--cmd` and a non-terminal stdin (e.g. a pipe), the
//!   target is run to completion and its sanitized output is printed.
//! - **Interactive** — with no `--cmd` and a terminal stdin, the host terminal
//!   is put in raw mode (restored on exit/panic) and keystrokes are forwarded
//!   to the target while its raw output is mirrored back: a transparent PTY
//!   wrapper around, say, `bash`.

use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use flat_cyborg::pty::Output;
use flat_cyborg::{IdleGate, Outcome, PtySession, RawModeGuard, Wrapper, WrapperConfig};

mod extract;
mod update;

const HELP: &str = "\
flat-cyborg — asynchronous PTY wrapper

USAGE:
    flat-cyborg [OPTIONS] -- <program> [args...]
    flat-cyborg update [--check]
    flat-cyborg version

OPTIONS:
    --cmd <TEXT>        Type TEXT into the target (repeatable). Selects
                        orchestrator mode.
    --cmd-file <PATH>   Like --cmd but read the prompt text from PATH. Use for
                        large prompts: a multi-MB prompt as an argv value
                        overflows ARG_MAX (the Argument-list-too-long limit);
                        a file does not. Repeatable; selects orchestrator mode.
    --timeout-ms <N>    Execution timeout per operation (default 60000).
    --idle-ms <N>       Silence after the prompt before declaring IDLE
                        (default 500).
    --prompt <TOKEN>    Trailing prompt token for IDLE detection (repeatable;
                        defaults to common shell prompts).
    --no-confirm        Do not auto-answer [y/n] confirmation prompts.
    --cwd <DIR>         Run the target with this working directory (default:
                        inherit flat-cyborg's).
    --cols <N>          PTY width for the target, in columns (default 120, or
                        $FLAT_CYBORG_COLS when set; 40-4000). A TUI soft-wraps
                        reply lines longer than the terminal, so a screen-read
                        --extract reply comes back re-wrapped; widen the PTY to
                        keep long single-line replies whole.
    --auto-approve      Auto-confirm agent approval menus (e.g. codex git-push,
                        claude trust). Bypasses the agent's safety gates —
                        opt-in. Off by default.
    --tui               Full-screen TUI mode: capture via a 2D screen grid and
                        treat a settled screen as idle (for apps using the
                        alternate screen / cursor addressing). Prints the final
                        rendered screen instead of the line log. A continuously
                        animated TUI may never settle — raise --idle-ms for it.
    --extract           Print only the model's reply. Wraps each --cmd prompt
                        with unique markers and prints the fenced reply between
                        them. Sentinel-STRICT by default: if the markers aren't
                        found it prints nothing and warns (a malformed/refusal
                        reply is empty downstream, never UI chrome). Needs --cmd.
                        Implies the 2D screen-grid capture (as --tui) since the
                        reply is read from the rendered screen — required for
                        alt-screen CLIs like claude.
    --extract-structural
                        Opt-in (implies --extract): if the markers are absent,
                        fall back to a best-effort, chrome-filtered structural
                        scrape of a known CLI's screen. Off by default because
                        the scrape can return echoed-prompt prose on a refusal.
                        Completion stays gated on the closing marker; a
                        marker-less reply completes after --extract-grace-ms.
    --extract-grace-ms <MS>
                        How long the output must be continuously quiet before a
                        marker-less reply is accepted as complete (the fallback
                        when the model omits the closing marker). Default with
                        --extract-structural: min(max(4 x --idle-ms, 30000),
                        --timeout-ms / 2). A settled screen is accepted on the
                        last of the --timeout-ms budget regardless, so the grace
                        never costs a timeout. 0 completes on the first settled
                        screen (the pre-0.13.0 --extract-structural behavior).
                        With strict --extract there is no grace unless this flag
                        sets one.
    --transcript-dir <DIR>
                        Directory of the target's own reply transcripts (default
                        $HOME/.claude/projects). Under --extract with a claude
                        target, the sentinel-fenced reply is recovered from the
                        transcript FIRST — it is authoritative and immune to how
                        the TUI renders long replies (newer claude collapses long
                        output off-screen, which the screen scrape then misses).
                        The rendered screen stays the fallback.
    --no-transcript-read
                        Disable transcript recovery; extract from the rendered
                        screen only (the pre-transcript behavior).
    --no-jitter         Write each --cmd as a single burst with no per-keystroke
                        human-cadence delay. The default jitter types one char
                        at a time (40-300 ms each), which is minutes for a
                        multi-thousand-char prompt; --no-jitter makes a large
                        prompt land in one write. Use for programmatic drivers
                        where the anti-anomaly cadence is not wanted.
    --wrap-input <COLS> Soft-fold each input line to <=COLS columns at word
                        boundaries before sending (default 0 = off). An
                        ultra-long single line overflows an Ink-style editor's
                        input field; folding makes a large prompt land reliably.
                        Pairs with --no-jitter (the burst path).
    --paste-input       Deliver each --cmd via bracketed paste (ESC[200~ .. the
                        body .. ESC[201~) then a settled Enter. An editor in
                        bracketed-paste mode (claude/codex) takes the whole block
                        atomically — no per-line submit, no overflow, no chunk
                        timing. Deterministic alternative to --no-jitter; takes
                        precedence over it. --wrap-input is unneeded under paste.
    -h, --help          Print this help.

COMMANDS:
    update [--check]    Self-update to the latest release (--check only reports).
    version             Print the version.

With no --cmd, a terminal stdin starts interactive passthrough; a piped stdin
runs the target to completion and prints its sanitized output.
";

#[derive(Debug)]
struct Args {
    cmds: Vec<String>,
    config: WrapperConfig,
    extract: bool,
    /// `--extract-structural`: allow the chrome-filtered structural fallback
    /// when the sentinel markers are absent. Off by default (sentinel-strict).
    extract_structural: bool,
    /// `--extract-grace-ms`: explicit marker-less grace for the IDLE gate.
    /// `None` = not given (the mode's default applies, see [`idle_gate_for`]).
    extract_grace: Option<Duration>,
    /// Directory of the target's own reply transcripts (claude:
    /// `~/.claude/projects`). Under `--extract` with a claude target, the
    /// sentinel-fenced reply is recovered from there FIRST (it is authoritative
    /// and immune to how the TUI renders long replies), the rendered screen only
    /// a fallback. `None` = disabled (`--no-transcript-read`, or a non-claude /
    /// no-`--extract` run). Defaults to `$HOME/.claude/projects`.
    transcript_dir: Option<String>,
    cwd: Option<String>,
    program: String,
    program_args: Vec<String>,
}

/// What the parsed command line asks for.
#[derive(Debug)]
enum Mode {
    Help,
    Version,
    Run(Box<Args>),
}

fn parse_args() -> Result<Mode, String> {
    parse_from(std::env::args().skip(1).collect())
}

/// Bounds for `--cols` / `$FLAT_CYBORG_COLS`: narrower than 40 breaks every
/// TUI's layout, wider than 4000 is a typo, not a terminal.
const MIN_COLS: u16 = 40;
const MAX_COLS: u16 = 4000;

/// Parses a PTY width, rejecting anything outside [`MIN_COLS`]..=[`MAX_COLS`].
fn parse_cols(v: &str) -> Result<u16, ()> {
    match v.trim().parse::<u16>() {
        Ok(c) if (MIN_COLS..=MAX_COLS).contains(&c) => Ok(c),
        _ => Err(()),
    }
}

/// Pure arg-parsing core, split out so it can be unit-tested without touching
/// the process-global `std::env::args`.
fn parse_from(raw: Vec<String>) -> Result<Mode, String> {
    // Split on the first `--` first, so flat-cyborg's own flags (`-h`/`--help`,
    // `--version`/`-V`) are only honored before it, never when they are
    // arguments to the target program after `--`.
    let split = raw.iter().position(|a| a == "--");
    let opts_slice = match split {
        Some(s) => &raw[..s],
        None => &raw[..],
    };
    if opts_slice.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(Mode::Help);
    }
    // `version` as a bare subcommand, or the `--version`/`-V` flags (scoped
    // before `--`, like `--help`).
    if (split.is_none() && raw.first().map(String::as_str) == Some("version"))
        || opts_slice.iter().any(|a| a == "--version" || a == "-V")
    {
        return Ok(Mode::Version);
    }

    let Some(split) = split else {
        return Err("missing `--` separator before the target program".into());
    };
    let (opts, rest) = raw.split_at(split);
    let rest = &rest[1..]; // drop the "--"
    if rest.is_empty() {
        return Err("no target program given after `--`".into());
    }

    let mut cmds = Vec::new();
    let mut has_cmd = false;
    let mut has_cmd_file = false;
    let mut config = WrapperConfig::default();
    // `$FLAT_CYBORG_COLS` is the width default for drivers that cannot pass
    // flags (a fixed argv builder); an explicit `--cols` still wins below.
    if let Ok(v) = std::env::var("FLAT_CYBORG_COLS") {
        config.cols = parse_cols(&v).map_err(|_| format!("invalid $FLAT_CYBORG_COLS: {v}"))?;
    }
    let mut prompts: Vec<String> = Vec::new();
    let mut extract = false;
    let mut extract_structural = false;
    let mut extract_grace: Option<Duration> = None;
    let mut transcript_dir: Option<String> = None;
    let mut transcript_off = false;
    let mut cwd: Option<String> = None;

    let mut i = 0;
    while i < opts.len() {
        let opt = &opts[i];
        let mut take_value = |name: &str| -> Result<String, String> {
            i += 1;
            opts.get(i)
                .cloned()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match opt.as_str() {
            "--cmd" => {
                if has_cmd_file {
                    return Err("--cmd and --cmd-file are mutually exclusive".into());
                }
                has_cmd = true;
                cmds.push(take_value("--cmd")?);
            }
            // Like --cmd, but the prompt text is read from a file instead of an
            // argv value. A multi-MB prompt as a command-line argument overflows
            // ARG_MAX (E2BIG / "Argument list too long"); a file does not.
            // Selects orchestrator mode exactly like --cmd. Repeatable.
            "--cmd-file" => {
                if has_cmd {
                    return Err("--cmd and --cmd-file are mutually exclusive".into());
                }
                has_cmd_file = true;
                let path = take_value("--cmd-file")?;
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("--cmd-file {path}: {e}"))?;
                cmds.push(text.trim_end().to_string());
            }
            "--prompt" => prompts.push(take_value("--prompt")?),
            "--timeout-ms" => {
                let v = take_value("--timeout-ms")?;
                let ms: u64 = v
                    .parse()
                    .map_err(|_| format!("invalid --timeout-ms: {v}"))?;
                config.exec_timeout = Duration::from_millis(ms);
            }
            "--idle-ms" => {
                let v = take_value("--idle-ms")?;
                let ms: u64 = v.parse().map_err(|_| format!("invalid --idle-ms: {v}"))?;
                config.idle_silence = Duration::from_millis(ms);
            }
            "--no-confirm" => config.auto_confirm = false,
            "--auto-approve" => config.auto_approve = true,
            "--cwd" => cwd = Some(take_value("--cwd")?),
            "--cols" => {
                let v = take_value("--cols")?;
                config.cols = parse_cols(&v).map_err(|_| format!("invalid --cols: {v}"))?;
            }
            "--tui" => config.tui = true,
            // --extract structurally needs the 2D screen grid: its transcript is
            // the screen's full_text (scrollback included), and a full-screen
            // alt-screen CLI (e.g. claude) is invisible to the line-log path. So
            // --extract implies the grid capture — otherwise it silently yields
            // no reply for exactly the alt-screen TUIs it is meant to read.
            "--extract" => {
                extract = true;
                config.tui = true;
            }
            // Opt-in best-effort structural fallback; implies --extract (and thus
            // the grid). Default --extract is sentinel-strict (see choose_reply).
            "--extract-structural" => {
                extract = true;
                extract_structural = true;
                config.tui = true;
            }
            // The marker-less grace: how long the output must be quiet before a
            // reply without the closing marker is accepted (see `idle_gate_for`).
            "--extract-grace-ms" => {
                let v = take_value("--extract-grace-ms")?;
                let ms: u64 = v
                    .parse()
                    .map_err(|_| format!("invalid --extract-grace-ms: {v}"))?;
                extract_grace = Some(Duration::from_millis(ms));
            }
            // Where the target persists its own reply transcript (default
            // `$HOME/.claude/projects`). Under --extract with claude, the fenced
            // reply is read from there first (see Args::transcript_dir).
            "--transcript-dir" => transcript_dir = Some(take_value("--transcript-dir")?),
            // Opt OUT of transcript recovery; fall back to screen scraping only.
            "--no-transcript-read" => transcript_off = true,
            "--no-jitter" => config.burst_input = true,
            "--paste-input" => config.paste_input = true,
            "--wrap-input" => {
                let v = take_value("--wrap-input")?;
                let cols: usize = v
                    .parse()
                    .map_err(|_| format!("invalid --wrap-input: {v}"))?;
                config.wrap_input = cols;
            }
            other => return Err(format!("unknown option: {other}")),
        }
        i += 1;
    }

    if !prompts.is_empty() {
        config.prompt_tokens = prompts;
    }

    // Validate `--cwd` here (a usage error → exit 2), before spawning.
    if let Some(dir) = &cwd {
        if !std::path::Path::new(dir).is_dir() {
            return Err(format!("cwd does not exist: {dir}"));
        }
    }

    // Resolve the transcript directory: an explicit --transcript-dir wins; else,
    // for an --extract run (transcript recovery is a reply-extraction aid),
    // default to `$HOME/.claude/projects`. --no-transcript-read forces it off.
    // A missing dir is not an error here — recovery just no-ops and the screen
    // path runs, so a non-claude target or absent transcript stays harmless.
    let transcript_dir = if transcript_off {
        None
    } else if transcript_dir.is_some() {
        transcript_dir
    } else if extract {
        std::env::var_os("HOME").map(|home| {
            std::path::Path::new(&home)
                .join(".claude")
                .join("projects")
                .to_string_lossy()
                .into_owned()
        })
    } else {
        None
    };

    Ok(Mode::Run(Box::new(Args {
        cmds,
        config,
        extract,
        extract_structural,
        extract_grace,
        transcript_dir,
        cwd,
        program: rest[0].clone(),
        program_args: rest[1..].to_vec(),
    })))
}

/// Builds a unique ASCII sentinel pair for command `seq`. Plain `[A-Za-z0-9_]` so
/// the tokens survive shell quoting, typing into the target, and ANSI sanitization.
///
/// The pair is per COMMAND, not per run: the closing marker is what completes the
/// reply wait, so a pair shared by every `--cmd` would leave the gate open on the
/// previous command's marker — already in the transcript — before the next reply
/// exists. `seq` makes the distinction deterministic rather than relying on the
/// clock advancing between two calls.
fn sentinels(seq: usize) -> (String, String) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tok = format!("{:x}{:x}{:x}", std::process::id(), nanos, seq);
    (format!("FCB_{tok}_BEGIN"), format!("FCB_{tok}_END"))
}

/// The IDLE gate for the orchestrator, given the (optional) sentinel pair, whether
/// the structural fallback (`--extract-structural`) is enabled, and an explicit
/// `--extract-grace-ms` if the caller passed one.
///
/// - Strict `--extract` (sentinels present, NOT structural) → gate on the closing
///   marker with NO marker-less grace unless `--extract-grace-ms` sets one: a
///   mid-think pause must not be mistaken for a finished reply, and with no
///   structural fallback a marker-less run has no reply to recover anyway.
/// - `--extract-structural` → the same marker gate, plus a marker-less grace
///   (explicit, else [`default_markerless_grace`]). The model intermittently omits
///   the sentinel (e.g. claude refusing the wrap protocol); pure marker-gating then
///   burns the full `--timeout-ms` and FAILS even though the structural fallback
///   could recover the reply (#55). But dropping the gate altogether made a SETTLED
///   screen the only completion signal, so any think-pause longer than `--idle-ms`
///   ended the reply wait early and the scrape returned chrome instead of the answer
///   (#68). The grace keeps both properties: marker-first completion, and a bounded
///   settle-based fallback for a genuinely marker-less reply.
/// - No `--extract` → `None` (unchanged).
///
/// Pure so it is unit-testable without a PTY. (#55, #68)
fn idle_gate_for(
    sentinels: &Option<(String, String)>,
    extract_structural: bool,
    explicit_grace: Option<Duration>,
    idle: Duration,
    exec_timeout: Duration,
) -> Option<IdleGate> {
    let (_, end) = sentinels.as_ref()?;
    let markerless_grace = if extract_structural {
        Some(explicit_grace.unwrap_or_else(|| default_markerless_grace(idle, exec_timeout)))
    } else {
        explicit_grace
    };
    Some(IdleGate {
        needle: end.clone(),
        markerless_grace,
    })
}

/// The default marker-less grace for `--extract-structural`:
/// `min(max(4 * idle, 30s), exec_timeout / 2)`.
///
/// Each term is load-bearing:
/// - the **30 s floor** is the quiet window a large model actually needs under
///   concurrent multi-session load — anything shorter is the same guess about
///   model latency that `--idle-ms` has been ratcheted through (4000 → 8000 →
///   12000 → 30000) without ever converging;
/// - the **`4 * idle`** term honours the caller's own latency signal: a driver
///   that already tells us it expects long silences (`--idle-ms 12000` for an
///   agentic run) gets a proportionally longer grace (48 s);
/// - the **`exec_timeout / 2` cap** keeps the grace well inside the watchdog
///   budget, so a marker-less reply normally completes on quiet rather than on
///   the deadline. It deliberately wins over the floor for a short
///   `--timeout-ms`.
///
/// The cap alone does NOT bound the wait: the grace is measured from the last
/// content change, so a reply whose last chunk lands late still needs more than
/// the remaining budget. What guarantees the fallback never costs a `124` is
/// [`Wrapper::wait_until_idle`] accepting a settled screen on the last of the
/// budget, for any grace value — including an explicit `--extract-grace-ms`
/// larger than the whole timeout.
///
/// Pure so it is unit-testable without a PTY.
fn default_markerless_grace(idle: Duration, exec_timeout: Duration) -> Duration {
    const FLOOR: Duration = Duration::from_secs(30);
    (4 * idle).max(FLOOR).min(exec_timeout / 2)
}

/// Appends the sentinel wrap instruction to a typed command, asking the target
/// to fence its reply between the per-run markers.
///
/// Kept to a SINGLE line (no embedded `\n`): a newline-submitting TUI (codex)
/// submits at the break, so a `{cmd}\n\n{instruction}` form would deliver the
/// command and the instruction as two separate prompts — the model answers the
/// command, never sees the wrap instruction, emits no fence, and only the
/// echoed-instruction markers remain. One line → both arrive as one submission.
/// (Claude treats an embedded newline as a soft break, so it was unaffected
/// either way; this makes codex work too.)
fn wrap_command(cmd: &str, begin: &str, end: &str) -> String {
    format!(
        "{cmd}    IMPORTANT: Output ONLY your answer, wrapped exactly between \
         the marker {begin} on its own line before it and the marker {end} on \
         its own line after it. Do not include the markers anywhere else."
    )
}

fn main() -> ExitCode {
    // `update` is dispatched first (it consumes its own arguments). It only
    // fires as the first token; to wrap a program literally named `update`, use
    // the `--` form (e.g. `flat-cyborg -- update`).
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("update") {
        return update::cmd_update(&argv[1..]);
    }

    let args = match parse_args() {
        Ok(Mode::Help) => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Ok(Mode::Version) => {
            println!("flat-cyborg {}", flat_cyborg::VERSION);
            return ExitCode::SUCCESS;
        }
        Ok(Mode::Run(args)) => args,
        Err(e) => {
            eprintln!("flat-cyborg: {e}\n");
            eprint!("{HELP}");
            return ExitCode::from(2);
        }
    };

    match run(*args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("flat-cyborg: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> flat_cyborg::Result<ExitCode> {
    let session = PtySession::spawn_in(
        &args.program,
        &args.program_args,
        args.cwd.as_deref().map(std::path::Path::new),
        flat_cyborg::pty::DEFAULT_ROWS,
        args.config.cols,
    )?;

    if !args.cmds.is_empty() {
        orchestrate(session, args)
    } else if rustix::termios::isatty(rustix::stdio::stdin()) {
        if args.config.tui {
            eprintln!(
                "flat-cyborg: --tui has no effect in interactive passthrough mode \
                 (it applies to --cmd orchestration and piped capture)"
            );
        }
        interactive(session)
    } else {
        capture(session, args)
    }
}

/// Orchestrator mode: type each command and wait for the target between them.
fn orchestrate(session: PtySession, args: Args) -> flat_cyborg::Result<ExitCode> {
    let tui = args.config.tui;
    let program = args.program.clone();
    let idle_silence = args.config.idle_silence;
    let exec_timeout = args.config.exec_timeout;
    let extract_structural = args.extract_structural;
    let transcript_dir = args.transcript_dir.clone();
    // Recovery only considers transcript files touched from here on, so a stale
    // session that happens to reuse a sentinel token can never be picked up.
    let since = std::time::SystemTime::now();
    let mut wrapper = Wrapper::with_config(session, args.config);
    let mut last = Outcome::Completed;
    // The last command's sentinel pair — what the final capture is extracted
    // between.
    let mut sentinels_used: Option<(String, String)> = None;
    for (seq, cmd) in args.cmds.iter().enumerate() {
        // With --extract we ALWAYS wrap the prompt with sentinel markers (for
        // every target, including known CLIs): they are self-validating and are
        // tried first when extracting. A FRESH pair per command — the closing
        // marker is the gate's completion signal, so reusing one pair would let
        // the previous command's marker, still in the transcript, complete this
        // command's reply wait before the answer exists.
        let pair = args.extract.then(|| sentinels(seq));
        // IDLE gating depends on the extract mode (see `idle_gate_for`): both
        // --extract modes marker-gate IDLE (a mid-think pause must not be mistaken
        // for a finished reply), and --extract-structural additionally accepts a
        // settled screen once the marker-less grace has elapsed, so a reply that
        // omits the sentinel is still recovered structurally instead of burning
        // the full --timeout-ms. (#55, #68)
        let gate = idle_gate_for(
            &pair,
            args.extract_structural,
            args.extract_grace,
            idle_silence,
            exec_timeout,
        );
        wrapper.set_idle_gate(gate);
        // Wrapping (when used) is kept a CLI concern; the wrapper library stays
        // unaware of sentinels.
        let effective = match &pair {
            Some((begin, end)) => wrap_command(cmd, begin, end),
            None => cmd.clone(),
        };
        sentinels_used = pair;
        last = wrapper.run_command(&effective)?;
        // A watchdog timeout or a mid-reply target death ends the session: the
        // target is gone (or unresponsive), so later `--cmd` commands cannot run.
        if matches!(last, Outcome::TimedOut | Outcome::TargetExitedEarly) {
            break;
        }
    }
    // Tell the operator WHICH completion signal ended the run. Without this, a
    // reply scraped from a screen that merely went quiet is indistinguishable
    // from one the model actually finished and fenced — the difference between
    // "no verdict" and "captured too early". The wrapper reports the marker-less
    // path only when it actually took it, so a target that exited on its own or
    // was killed by the watchdog cannot inflate the marker-less rate operators
    // measure from this line. The number is the OBSERVED quiet window: on the
    // watchdog-budget bound that is much shorter than the configured grace.
    if let Some(quiet) = wrapper.markerless_quiet() {
        eprintln!(
            "flat-cyborg: --extract: no closing sentinel; completed on the \
             marker-less grace ({} ms)",
            quiet.as_millis()
        );
    }
    // Print the reply FIRST: under --extract-structural a mid-reply death can
    // still leave a recoverable, chrome-free reply on the settled screen. Whether
    // one actually reached stdout decides if this was a lost reply (exit 75) or a
    // mere missing marker (exit 0) — so it must be known before both the
    // observability line and the exit-code mapping below.
    let reply_recovered = print_capture(
        &wrapper,
        tui,
        sentinels_used.as_ref(),
        &program,
        extract_structural,
        transcript_dir.as_deref(),
        since,
    );
    // Observability only (NOT the machine contract — that is exit 75): tell the
    // operator the target vanished mid-reply and its reply was LOST, so a bare
    // non-zero exit is not read as a real fault. Suppressed when the reply was
    // still recovered — that path exits 0, nothing transient happened. The
    // retry-able signal is the exit code, not this string.
    if last == Outcome::TargetExitedEarly && !reply_recovered {
        eprintln!(
            "flat-cyborg: the target exited before completing its reply \
             (mid-reply exit; treated as transient, exit {EX_TEMPFAIL})"
        );
    }
    Ok(exit_code_for(&mut wrapper, last, reply_recovered))
}

/// Capture mode: run the target to completion, print its sanitized output.
fn capture(session: PtySession, args: Args) -> flat_cyborg::Result<ExitCode> {
    let tui = args.config.tui;
    let program = args.program.clone();
    // --extract has nothing to wrap here (no --cmd selects orchestrator mode),
    // so there are no sentinel markers in the output; extraction therefore warns
    // and prints nothing (strict default), or — with --extract-structural — tries
    // a chrome-filtered structural scrape for a known CLI.
    let mut wrapper = Wrapper::with_config(session, args.config);
    let outcome = wrapper.wait_until_idle()?;
    let reply_recovered = print_capture(
        &wrapper,
        tui,
        args.extract.then(|| sentinels(0)).as_ref(),
        &program,
        args.extract_structural,
        // Capture mode never wraps/sends a sentinel command, so there is no
        // fenced reply to recover from a transcript — screen path only.
        None,
        std::time::SystemTime::now(),
    );
    Ok(exit_code_for(&mut wrapper, outcome, reply_recovered))
}

/// Prints the captured output: the rendered screen in TUI mode, otherwise the
/// line-oriented sanitized log.
///
/// With `--extract` (`sentinels` present) it uses the sentinel-first hybrid
/// ([`extract::choose_reply`]): the fenced reply between the last marker pair if
/// the model honored the wrap, otherwise a sanity-checked structural slice for a
/// known CLI, otherwise nothing (with a warning). It never prints UI chrome.
/// Without `--extract` it prints the plain captured output.
///
/// The full transcript (including lines scrolled off the top in TUI mode) is
/// used for extraction so long multi-line replies are captured whole.
/// Returns `true` when an actual reply reached stdout (a fenced or clean
/// structural reply under `--extract`, or the plain captured output without it),
/// `false` when `--extract` found nothing printable. The caller uses this to
/// decide whether a mid-reply target death actually lost the reply (exit 75) or
/// merely dropped the closing marker while the reply was still recovered (exit 0).
fn print_capture(
    wrapper: &Wrapper,
    tui: bool,
    sentinels: Option<&(String, String)>,
    program: &str,
    allow_structural: bool,
    transcript_dir: Option<&str>,
    since: std::time::SystemTime,
) -> bool {
    if let Some((begin, end)) = sentinels {
        // Authoritative source FIRST: the target's own reply transcript captures
        // the whole reply regardless of how its TUI renders long output — newer
        // claude collapses long replies off the rendered screen, which the screen
        // scrape below then cannot recover. Falls through to the screen path when
        // no transcript carries the fence (transcript saving off, target is not
        // claude, or the reply never completed).
        if let Some(dir) = transcript_dir {
            if let Some(reply) = reply_from_transcript(dir, program, begin, end, since) {
                println!("{reply}");
                io::stdout().flush().ok();
                return true;
            }
        }
        let text = if tui {
            wrapper.screen_full_text()
        } else {
            wrapper.clean_log()
        };
        let recovered = match extract::choose_reply(program, &text, begin, end, allow_structural) {
            Some(s) => {
                println!("{s}");
                true
            }
            // The target did not emit the markers (and, under --extract-structural,
            // no chrome-free slice was recoverable). Print nothing (never chrome)
            // and warn. Suggest the opt-in only when it is not already on.
            None if allow_structural => {
                eprintln!(
                    "flat-cyborg: --extract found no fenced reply and no chrome-free \
                     structural fallback; printing nothing."
                );
                false
            }
            None => {
                eprintln!(
                    "flat-cyborg: --extract found no fenced reply (the target did not \
                     emit the markers); printing nothing. Pass --extract-structural \
                     for a best-effort structural scrape of a known CLI."
                );
                false
            }
        };
        io::stdout().flush().ok();
        return recovered;
    }
    if tui {
        println!("{}", wrapper.screen_text());
    } else {
        print!("{}", wrapper.clean_log());
    }
    io::stdout().flush().ok();
    true
}

/// Recovers the fenced reply for `begin`/`end` from the target's own transcript
/// files under `dir` (claude persists each turn to
/// `~/.claude/projects/<proj>/<session>.jsonl`). Only attempted for a claude
/// target. Scans `*.jsonl` one level under `dir` whose mtime lands in a window
/// around/after `since` — the sentinel is unique per run, so the window is only
/// a scan bound, not a correctness filter — newest first, and returns the first
/// sentinel-fenced reply found. `None` when the target is not claude, `dir` is
/// unreadable, or no transcript carries the fence (e.g. transcript saving off).
fn reply_from_transcript(
    dir: &str,
    program: &str,
    begin: &str,
    end: &str,
    since: std::time::SystemTime,
) -> Option<String> {
    if std::path::Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        != Some("claude")
    {
        return None;
    }
    // Widen the mtime floor generously: correctness rides on the unique sentinel,
    // so this only avoids reading long-dead transcripts.
    let window = since
        .checked_sub(Duration::from_secs(300))
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    for proj in std::fs::read_dir(dir).ok()?.flatten() {
        let Ok(sessions) = std::fs::read_dir(proj.path()) else {
            continue;
        };
        for entry in sessions.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            if mtime >= window {
                files.push((mtime, path));
            }
        }
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.0)); // newest first
    for (_, path) in files {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Some(reply) = extract::reply_from_jsonl_text(&content, begin, end) {
                return Some(reply);
            }
        }
    }
    None
}

/// Reserved exit code for a mid-reply target death whose reply was actually
/// LOST: the target closed the PTY un-interrupted, under an `--extract` gate
/// that never opened, AND no reply could be recovered (not even by the
/// `--extract-structural` fallback). `75` is `EX_TEMPFAIL` from `sysexits.h` —
/// "temporary failure; retry" — chosen so a resilience layer can key on a
/// transient by contract instead of scraping stderr. It overrides the target's
/// own passthrough status on this one arm only; every other exit code
/// (0 / 1 / 2 / 124) keeps its existing meaning. Crucially, when the reply WAS
/// recovered (a marker-less structural extraction still printed it), this is a
/// success (exit 0), not a transient — a mere missing closing marker must never
/// be reported as a lost reply. See issue #71.
const EX_TEMPFAIL: u8 = 75;

/// Maps a terminal [`Outcome`] to a process exit code: the target's own exit
/// status when it completed, `124` on watchdog timeout, `75` when the target
/// vanished mid-reply AND the reply was lost (transient; see [`EX_TEMPFAIL`]),
/// `0` when it returned to an idle prompt OR the mid-reply-death reply was still
/// recovered (`reply_recovered`). `reply_recovered` reflects whether
/// [`print_capture`] actually put a reply on stdout.
fn exit_code_for(wrapper: &mut Wrapper, outcome: Outcome, reply_recovered: bool) -> ExitCode {
    match outcome {
        Outcome::TimedOut => ExitCode::from(124),
        // The target vanished mid-reply. Signal the retryable transient ONLY when
        // the reply was genuinely lost; if the structural fallback still recovered
        // and printed it, the answer reached the caller — that is a success, and
        // returning 75 would make a resilience layer discard a good reply (#71 review).
        Outcome::TargetExitedEarly if reply_recovered => ExitCode::SUCCESS,
        Outcome::TargetExitedEarly => ExitCode::from(EX_TEMPFAIL),
        Outcome::Idle => ExitCode::SUCCESS,
        Outcome::Completed => {
            let status = wrapper.session().wait_with_timeout(Duration::from_secs(2));
            let code = status.and_then(|s| s.code());
            // Diagnostic tap (off unless FLAT_CYBORG_DIAG is set): the final
            // per-run classification the #71 harness buckets on. A `Completed`
            // outcome means the target's PTY closed while un-interrupted; the
            // resolved code separates a clean target exit from one killed by a
            // signal (`code=none` -> exit 1 here, NOT flat-cyborg self-faulting).
            flat_cyborg::diag!(
                "exit.classify",
                "outcome=Completed reaped={} code={}",
                status.is_some(),
                code.map_or_else(|| "none".to_string(), |c| c.to_string())
            );
            match code {
                Some(c) => ExitCode::from(c.clamp(0, 255) as u8),
                None => ExitCode::FAILURE, // killed by signal / unknown
            }
        }
    }
}

/// Interactive mode: forward host keystrokes to the target and mirror its raw
/// output, with the host terminal in raw mode (restored on exit/panic).
fn interactive(session: PtySession) -> flat_cyborg::Result<ExitCode> {
    // Restoring the host terminal is guaranteed by the guard's Drop, which runs
    // on normal return and during panic unwinding.
    let _raw_guard = RawModeGuard::stdin()?;

    if let Some(input) = session.input_handle() {
        // Forward host stdin to the target on a dedicated thread so the main
        // thread is free to mirror output. The thread is detached; it ends with
        // the process once the target exits.
        thread::spawn(move || {
            let mut stdin = io::stdin();
            let mut buf = [0u8; 1024];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if input.write(&buf[..n]).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    let mut stdout = io::stdout();
    loop {
        match session.read_output(Duration::from_millis(100)) {
            Output::Data(chunk) => {
                // Mirror raw bytes so the user sees the target exactly (colors,
                // cursor moves, and all).
                stdout.write_all(&chunk).ok();
                stdout.flush().ok();
            }
            Output::Idle => {}
            Output::Eof => break,
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinels_are_distinct_ascii() {
        let (b, e) = sentinels(0);
        assert_ne!(b, e);
        assert!(b.ends_with("_BEGIN"));
        assert!(e.ends_with("_END"));
        assert!(b.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        assert!(e.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    #[test]
    fn each_command_gets_its_own_sentinel_pair() {
        // The closing marker completes the reply wait, so two commands sharing a
        // pair would let command A's marker — still in the transcript — end
        // command B's wait before B has answered. The sequence number makes the
        // pairs differ even if the clock does not advance between the calls.
        let (b0, e0) = sentinels(0);
        let (b1, e1) = sentinels(1);
        assert_ne!(b0, b1, "BEGIN markers must differ per command");
        assert_ne!(e0, e1, "END markers must differ per command");
    }

    /// `--idle-ms` / `--timeout-ms` defaults, so the gate tests read as the CLI
    /// behaves out of the box.
    const IDLE: Duration = Duration::from_millis(500);
    const TIMEOUT: Duration = Duration::from_secs(60);

    #[test]
    fn idle_gate_for_modes() {
        let s = Some(("FCB_x_BEGIN".to_string(), "FCB_x_END".to_string()));
        // strict --extract: gate IDLE on the closing marker, no marker-less grace
        // (the watchdog is the backstop).
        let strict = idle_gate_for(&s, false, None, IDLE, TIMEOUT).expect("gate");
        assert_eq!(strict.needle, "FCB_x_END");
        assert_eq!(strict.markerless_grace, None);
        // --extract-structural: same marker gate PLUS a bounded settle fallback.
        let structural = idle_gate_for(&s, true, None, IDLE, TIMEOUT).expect("gate");
        assert_eq!(structural.needle, "FCB_x_END");
        assert_eq!(
            structural.markerless_grace,
            Some(default_markerless_grace(IDLE, TIMEOUT)),
            "--extract-structural must default the grace, not disable the gate"
        );
        // no --extract: no gate, regardless of the structural flag.
        assert!(idle_gate_for(&None, false, None, IDLE, TIMEOUT).is_none());
        assert!(idle_gate_for(&None, true, None, IDLE, TIMEOUT).is_none());
    }

    #[test]
    fn idle_gate_for_honours_an_explicit_grace() {
        let s = Some(("FCB_x_BEGIN".to_string(), "FCB_x_END".to_string()));
        let explicit = Some(Duration::from_millis(7000));
        // --extract-grace-ms overrides the structural default ...
        assert_eq!(
            idle_gate_for(&s, true, explicit, IDLE, TIMEOUT)
                .expect("gate")
                .markerless_grace,
            explicit
        );
        // ... and opts strict --extract into a grace it does not have by default.
        assert_eq!(
            idle_gate_for(&s, false, explicit, IDLE, TIMEOUT)
                .expect("gate")
                .markerless_grace,
            explicit
        );
        // `0` is the escape hatch: a settled screen completes immediately, which
        // is the pre-0.13.0 --extract-structural behaviour (and the A/B control).
        assert_eq!(
            idle_gate_for(&s, true, Some(Duration::ZERO), IDLE, TIMEOUT)
                .expect("gate")
                .markerless_grace,
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn default_markerless_grace_floor() {
        // A small --idle-ms must not shrink the grace below the 30 s floor: the
        // floor, not the idle window, is the quiet period a large model needs.
        assert_eq!(
            default_markerless_grace(Duration::from_millis(500), Duration::from_secs(600)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn default_markerless_grace_scales_with_idle() {
        // A caller that already signals long silences (--idle-ms 12000 for an
        // agentic run) gets 4x that, above the floor.
        assert_eq!(
            default_markerless_grace(Duration::from_secs(12), Duration::from_secs(600)),
            Duration::from_secs(48)
        );
    }

    #[test]
    fn default_markerless_grace_is_capped_below_the_watchdog() {
        // The cap wins over both the 4x term and the floor, so the marker-less
        // fallback always fires before the watchdog — it can never turn a
        // completed run into a 124 timeout (#55 must not come back).
        assert_eq!(
            default_markerless_grace(Duration::from_secs(12), Duration::from_secs(60)),
            Duration::from_secs(30)
        );
        assert_eq!(
            default_markerless_grace(Duration::from_millis(500), Duration::from_secs(20)),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn extract_grace_ms_flag_parses() {
        let m = parse_from(vec![
            "--extract-structural".into(),
            "--extract-grace-ms".into(),
            "0".into(),
            "--cmd".into(),
            "hi".into(),
            "--".into(),
            "claude".into(),
        ])
        .expect("parse");
        match m {
            Mode::Run(a) => assert_eq!(a.extract_grace, Some(Duration::ZERO)),
            _ => panic!("expected Mode::Run"),
        }
    }

    #[test]
    fn extract_grace_ms_rejects_a_non_numeric_value() {
        let err = parse_from(vec![
            "--extract-grace-ms".into(),
            "later".into(),
            "--".into(),
            "claude".into(),
        ])
        .expect_err("expected a parse error");
        assert!(err.contains("invalid --extract-grace-ms"), "got: {err}");
    }

    #[test]
    fn wrap_command_appends_markers() {
        let w = wrap_command("hello", "B_BEGIN", "B_END");
        assert!(w.starts_with("hello"));
        assert!(w.contains("B_BEGIN"));
        assert!(w.contains("B_END"));
    }

    #[test]
    fn wrap_command_is_single_line() {
        // No embedded newline: a newline-submitting TUI (codex) must receive the
        // command and the wrap instruction as ONE submission, else it never sees
        // the instruction and emits no fence (#40).
        let w = wrap_command("do a thing", "B_BEGIN", "B_END");
        assert!(!w.contains('\n'), "wrap_command must be single-line: {w:?}");
        assert!(w.contains("IMPORTANT"));
    }

    #[test]
    fn cols_flag_sets_pty_width_and_defaults_to_120() {
        // Default stays the historical 120-column PTY; `--cols` overrides it
        // (this test does not set $FLAT_CYBORG_COLS, so the default applies).
        let m =
            parse_from(vec!["--cmd".into(), "hi".into(), "--".into(), "sh".into()]).expect("parse");
        match m {
            Mode::Run(a) => assert_eq!(a.config.cols, flat_cyborg::pty::DEFAULT_COLS),
            other => panic!("expected Run, got {other:?}"),
        }
        let m = parse_from(vec![
            "--cols".into(),
            "600".into(),
            "--cmd".into(),
            "hi".into(),
            "--".into(),
            "sh".into(),
        ])
        .expect("parse");
        match m {
            Mode::Run(a) => assert_eq!(a.config.cols, 600),
            other => panic!("expected Run, got {other:?}"),
        }
        for bad in ["0", "39", "4001", "wide", ""] {
            let r = parse_from(vec![
                "--cols".into(),
                bad.into(),
                "--cmd".into(),
                "hi".into(),
                "--".into(),
                "sh".into(),
            ]);
            assert!(r.is_err(), "--cols {bad:?} must be rejected");
        }
    }

    #[test]
    fn extract_implies_screen_grid() {
        // --extract reads the reply from the rendered screen, so it must turn on
        // the grid capture (config.tui) even when --tui is not passed — otherwise
        // it silently yields nothing for alt-screen CLIs like claude.
        let m = parse_from(vec![
            "--extract".into(),
            "--cmd".into(),
            "hi".into(),
            "--".into(),
            "claude".into(),
        ])
        .expect("parse");
        match m {
            Mode::Run(a) => {
                assert!(a.extract, "extract flag should be set");
                assert!(
                    a.config.tui,
                    "--extract must imply the screen grid (config.tui)"
                );
            }
            _ => panic!("expected Mode::Run"),
        }
    }

    #[test]
    fn cmd_file_reads_prompt_from_file() {
        // --cmd-file must read the prompt text from the file (so a multi-MB
        // prompt does not overflow ARG_MAX), and select orchestrator mode the
        // same way --cmd does.
        let path = std::env::temp_dir().join("flat-cyborg-cmdfile-test.txt");
        std::fs::write(&path, "hello from file\nsecond line\n").expect("write");
        let m = parse_from(vec![
            "--cmd-file".into(),
            path.to_string_lossy().into_owned(),
            "--".into(),
            "claude".into(),
        ])
        .expect("parse");
        std::fs::remove_file(&path).ok();
        match m {
            Mode::Run(a) => {
                assert_eq!(
                    a.cmds,
                    vec!["hello from file\nsecond line".to_string()],
                    "--cmd-file should push the file's content as a cmd, trailing whitespace trimmed"
                );
            }
            _ => panic!("expected Mode::Run"),
        }
    }

    #[test]
    fn cmd_and_cmd_file_are_mutually_exclusive() {
        // --cmd and --cmd-file both select orchestrator mode from different
        // sources; combining them is ambiguous and must be rejected.
        let path = std::env::temp_dir().join("flat-cyborg-cmdfile-mutex-test.txt");
        std::fs::write(&path, "from file").expect("write");
        let err = parse_from(vec![
            "--cmd".into(),
            "from argv".into(),
            "--cmd-file".into(),
            path.to_string_lossy().into_owned(),
            "--".into(),
            "claude".into(),
        ])
        .expect_err("expected an error");
        std::fs::remove_file(&path).ok();
        assert!(
            err.contains("mutually exclusive"),
            "error should mention mutual exclusivity: {err:?}"
        );
    }

    #[test]
    fn cmd_file_missing_path_reports_the_flag_name() {
        // A --cmd-file pointing at a non-existent path must fail with an error
        // that names the flag, so the caller knows which argument was bad.
        let path = std::env::temp_dir().join("flat-cyborg-cmdfile-does-not-exist.txt");
        std::fs::remove_file(&path).ok();
        let err = parse_from(vec![
            "--cmd-file".into(),
            path.to_string_lossy().into_owned(),
            "--".into(),
            "claude".into(),
        ])
        .expect_err("expected an error for a missing file");
        assert!(
            err.contains("--cmd-file"),
            "error should name the --cmd-file flag: {err:?}"
        );
    }

    #[test]
    fn plain_extract_is_sentinel_strict() {
        let m = parse_from(vec![
            "--extract".into(),
            "--cmd".into(),
            "hi".into(),
            "--".into(),
            "claude".into(),
        ])
        .expect("parse");
        match m {
            Mode::Run(a) => {
                assert!(a.extract);
                assert!(
                    !a.extract_structural,
                    "plain --extract must be sentinel-strict (no structural fallback)"
                );
            }
            _ => panic!("expected Mode::Run"),
        }
    }

    #[test]
    fn extract_structural_implies_extract_and_grid() {
        let m = parse_from(vec![
            "--extract-structural".into(),
            "--cmd".into(),
            "hi".into(),
            "--".into(),
            "claude".into(),
        ])
        .expect("parse");
        match m {
            Mode::Run(a) => {
                assert!(a.extract, "--extract-structural implies --extract");
                assert!(a.extract_structural, "structural fallback opted in");
                assert!(a.config.tui, "--extract-structural implies the screen grid");
            }
            _ => panic!("expected Mode::Run"),
        }
    }

    #[test]
    fn wrap_input_flag_sets_the_fold_width() {
        let m = parse_from(vec![
            "--wrap-input".into(),
            "72".into(),
            "--cmd".into(),
            "hi".into(),
            "--".into(),
            "claude".into(),
        ])
        .expect("parse");
        match m {
            Mode::Run(a) => assert_eq!(a.config.wrap_input, 72),
            _ => panic!("expected Mode::Run"),
        }
    }

    #[test]
    fn paste_input_flag_sets_paste_mode() {
        let m = parse_from(vec![
            "--paste-input".into(),
            "--cmd".into(),
            "hi".into(),
            "--".into(),
            "claude".into(),
        ])
        .expect("parse");
        match m {
            Mode::Run(a) => {
                assert!(a.config.paste_input, "--paste-input sets paste mode");
                assert!(!a.config.burst_input, "paste does not imply burst");
            }
            _ => panic!("expected Mode::Run"),
        }
    }

    #[test]
    fn wrap_input_rejects_a_non_numeric_value() {
        let err = match parse_from(vec![
            "--wrap-input".into(),
            "wide".into(),
            "--".into(),
            "claude".into(),
        ]) {
            Err(e) => e,
            Ok(_) => panic!("expected a parse error"),
        };
        assert!(err.contains("invalid --wrap-input"), "got: {err}");
    }
}
