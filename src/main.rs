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
use flat_cyborg::{Outcome, PtySession, RawModeGuard, Wrapper, WrapperConfig};

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

struct Args {
    cmds: Vec<String>,
    config: WrapperConfig,
    extract: bool,
    /// `--extract-structural`: allow the chrome-filtered structural fallback
    /// when the sentinel markers are absent. Off by default (sentinel-strict).
    extract_structural: bool,
    cwd: Option<String>,
    program: String,
    program_args: Vec<String>,
}

/// What the parsed command line asks for.
enum Mode {
    Help,
    Version,
    Run(Box<Args>),
}

fn parse_args() -> Result<Mode, String> {
    parse_from(std::env::args().skip(1).collect())
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
    let mut config = WrapperConfig::default();
    let mut prompts: Vec<String> = Vec::new();
    let mut extract = false;
    let mut extract_structural = false;
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
            "--cmd" => cmds.push(take_value("--cmd")?),
            // Like --cmd, but the prompt text is read from a file instead of an
            // argv value. A multi-MB prompt as a command-line argument overflows
            // ARG_MAX (E2BIG / "Argument list too long"); a file does not.
            // Selects orchestrator mode exactly like --cmd. Repeatable.
            "--cmd-file" => {
                let path = take_value("--cmd-file")?;
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("--cmd-file {path}: {e}"))?;
                cmds.push(text);
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

    Ok(Mode::Run(Box::new(Args {
        cmds,
        config,
        extract,
        extract_structural,
        cwd,
        program: rest[0].clone(),
        program_args: rest[1..].to_vec(),
    })))
}

/// Builds a unique per-run ASCII sentinel pair. Plain `[A-Za-z0-9_]` so the
/// tokens survive shell quoting, typing into the target, and ANSI sanitization.
fn sentinels() -> (String, String) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tok = format!("{:x}{:x}", std::process::id(), nanos);
    (format!("FCB_{tok}_BEGIN"), format!("FCB_{tok}_END"))
}

/// The IDLE gate for the orchestrator, given the (optional) sentinel pair and whether
/// the structural fallback (`--extract-structural`) is enabled.
///
/// - Strict `--extract` (sentinels present, NOT structural) → `Some(end)`: gate IDLE on
///   the closing marker so a mid-think pause is not mistaken for a finished reply.
/// - `--extract-structural` → `None`: the model intermittently omits the sentinel (e.g.
///   claude refusing the wrap protocol). Marker-gating then burns the full `--timeout-ms`
///   and FAILS even though the structural fallback could recover the reply. With no gate a
///   SETTLED screen is treated as idle (like `--tui`) and the reply is scraped (marker
///   first, structural fallback) — fast AND marker-less-tolerant.
/// - No `--extract` → `None` (unchanged).
///
/// Pure so it is unit-testable without a PTY. (#55)
fn idle_gate_for(sentinels: &Option<(String, String)>, extract_structural: bool) -> Option<String> {
    match sentinels {
        Some((_, end)) if !extract_structural => Some(end.clone()),
        _ => None,
    }
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
        flat_cyborg::pty::DEFAULT_COLS,
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
    // With --extract we ALWAYS wrap the prompt with a per-run sentinel pair (for
    // every target, including known CLIs): the markers are self-validating and
    // are tried first when extracting. The pair is generated once so the same
    // markers are used for both wrapping and extraction.
    let sentinels = args.extract.then(sentinels);
    let mut config = args.config;
    // IDLE gating depends on the extract mode (see `idle_gate_for`): strict --extract
    // marker-gates IDLE (a mid-think pause must not be mistaken for a finished reply);
    // --extract-structural does NOT, so a marker-less reply (the model intermittently
    // omits the sentinel) completes on a settled screen and is recovered structurally
    // instead of burning the full --timeout-ms and failing. (#55)
    config.idle_gate = idle_gate_for(&sentinels, args.extract_structural);
    let mut wrapper = Wrapper::with_config(session, config);
    let mut last = Outcome::Completed;
    for cmd in &args.cmds {
        // Wrapping (when used) is kept a CLI concern; the wrapper library stays
        // unaware of sentinels.
        let effective = match &sentinels {
            Some((begin, end)) => wrap_command(cmd, begin, end),
            None => cmd.clone(),
        };
        last = wrapper.run_command(&effective)?;
        if last == Outcome::TimedOut {
            break;
        }
    }
    print_capture(
        &wrapper,
        tui,
        sentinels.as_ref(),
        &program,
        args.extract_structural,
    );
    Ok(exit_code_for(&mut wrapper, last))
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
    print_capture(
        &wrapper,
        tui,
        args.extract.then(sentinels).as_ref(),
        &program,
        args.extract_structural,
    );
    Ok(exit_code_for(&mut wrapper, outcome))
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
fn print_capture(
    wrapper: &Wrapper,
    tui: bool,
    sentinels: Option<&(String, String)>,
    program: &str,
    allow_structural: bool,
) {
    if let Some((begin, end)) = sentinels {
        let text = if tui {
            wrapper.screen_full_text()
        } else {
            wrapper.clean_log()
        };
        match extract::choose_reply(program, &text, begin, end, allow_structural) {
            Some(s) => println!("{s}"),
            // The target did not emit the markers (and, under --extract-structural,
            // no chrome-free slice was recoverable). Print nothing (never chrome)
            // and warn. Suggest the opt-in only when it is not already on.
            None if allow_structural => eprintln!(
                "flat-cyborg: --extract found no fenced reply and no chrome-free \
                 structural fallback; printing nothing."
            ),
            None => eprintln!(
                "flat-cyborg: --extract found no fenced reply (the target did not \
                 emit the markers); printing nothing. Pass --extract-structural \
                 for a best-effort structural scrape of a known CLI."
            ),
        }
        io::stdout().flush().ok();
        return;
    }
    if tui {
        println!("{}", wrapper.screen_text());
    } else {
        print!("{}", wrapper.clean_log());
    }
    io::stdout().flush().ok();
}

/// Maps a terminal [`Outcome`] to a process exit code: the target's own exit
/// status when it completed, `124` on watchdog timeout, `0` when it merely
/// returned to an idle prompt (our commands ran; the target is still alive).
fn exit_code_for(wrapper: &mut Wrapper, outcome: Outcome) -> ExitCode {
    match outcome {
        Outcome::TimedOut => ExitCode::from(124),
        Outcome::Idle => ExitCode::SUCCESS,
        Outcome::Completed => {
            let code = wrapper
                .session()
                .wait_with_timeout(Duration::from_secs(2))
                .and_then(|status| status.code());
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
        let (b, e) = sentinels();
        assert_ne!(b, e);
        assert!(b.ends_with("_BEGIN"));
        assert!(e.ends_with("_END"));
        assert!(b.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        assert!(e.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    #[test]
    fn idle_gate_for_modes() {
        let s = Some(("FCB_x_BEGIN".to_string(), "FCB_x_END".to_string()));
        // strict --extract: gate IDLE on the closing marker.
        assert_eq!(idle_gate_for(&s, false), Some("FCB_x_END".to_string()));
        // --extract-structural: NO marker gate (settle-based, marker-less-tolerant).
        assert_eq!(idle_gate_for(&s, true), None);
        // no --extract: no gate, regardless of the structural flag.
        assert_eq!(idle_gate_for(&None, false), None);
        assert_eq!(idle_gate_for(&None, true), None);
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
        std::fs::write(&path, "hello from file\nsecond line").expect("write");
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
                    "--cmd-file should push the file's content as a cmd"
                );
            }
            _ => panic!("expected Mode::Run"),
        }
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
