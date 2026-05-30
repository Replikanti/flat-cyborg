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

mod update;

/// Built-in profiles for known LLM CLIs. A `--profile <name>` is a named bundle
/// of settings for driving that tool; today it supplies the response marker its
/// assistant reply lines begin with (verified by observation), and more per-CLI
/// defaults can be added here over time. Override any of it with the explicit
/// flags (e.g. `--response-marker`). The tool stays LLM-agnostic — this is just
/// a name→settings map, one line per CLI.
struct Profile {
    name: &'static str,
    response_marker: &'static str,
}

const PROFILES: &[Profile] = &[
    Profile {
        name: "claude",
        response_marker: "●",
    },
    Profile {
        name: "codex",
        response_marker: "•",
    },
];

fn find_profile(name: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|p| p.name == name)
}

fn known_profiles() -> String {
    PROFILES
        .iter()
        .map(|p| p.name)
        .collect::<Vec<_>>()
        .join(", ")
}

const HELP: &str = "\
flat-cyborg — asynchronous PTY wrapper

USAGE:
    flat-cyborg [OPTIONS] -- <program> [args...]
    flat-cyborg update [--check]
    flat-cyborg version

OPTIONS:
    --cmd <TEXT>        Type TEXT into the target (repeatable). Selects
                        orchestrator mode.
    --timeout-ms <N>    Execution timeout per operation (default 60000).
    --idle-ms <N>       Silence after the prompt before declaring IDLE
                        (default 500).
    --prompt <TOKEN>    Trailing prompt token for IDLE detection (repeatable;
                        defaults to common shell prompts).
    --no-confirm        Do not auto-answer [y/n] confirmation prompts.
    --profile <NAME>    Settings bundle for a known LLM CLI; currently sets
                        --response-marker to that tool's reply glyph. Known:
                        claude (●), codex (•). Explicit flags override it.
    --tui               Full-screen TUI mode: capture via a 2D screen grid and
                        treat a settled screen as idle (for apps using the
                        alternate screen / cursor addressing). Prints the final
                        rendered screen instead of the line log. A continuously
                        animated TUI may never settle — raise --idle-ms for it.
    --response-marker <S> Print only captured lines whose first non-blank
                        content starts with <S>, with <S> stripped (e.g.
                        '●' to extract an assistant's reply lines).
    --extract           Wrap each --cmd so the target fences its reply between
                        unique markers, and print only the fenced reply
                        (robust for multi-line LLM answers; needs --cmd).
                        Wins over --profile/--response-marker.
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
    response_marker: Option<String>,
    extract: bool,
    program: String,
    program_args: Vec<String>,
}

/// What the parsed command line asks for.
enum Mode {
    Help,
    Version,
    Run(Args),
}

fn parse_args() -> Result<Mode, String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();

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
    let mut response_marker: Option<String> = None;
    let mut profile: Option<String> = None;
    let mut extract = false;

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
            "--tui" => config.tui = true,
            "--profile" => profile = Some(take_value("--profile")?),
            "--response-marker" => response_marker = Some(take_value("--response-marker")?),
            "--extract" => extract = true,
            other => return Err(format!("unknown option: {other}")),
        }
        i += 1;
    }

    if !prompts.is_empty() {
        config.prompt_tokens = prompts;
    }

    // Apply a --profile's defaults, but never override a setting the user gave
    // explicitly. An unknown profile name is an error (listing the known ones)
    // so a typo fails loudly rather than silently doing nothing. The name is
    // validated even when --response-marker already set it.
    if let Some(name) = &profile {
        let p = find_profile(name)
            .ok_or_else(|| format!("unknown --profile: {name} (known: {})", known_profiles()))?;
        if response_marker.is_none() {
            response_marker = Some(p.response_marker.to_string());
        }
    }

    Ok(Mode::Run(Args {
        cmds,
        config,
        response_marker,
        extract,
        program: rest[0].clone(),
        program_args: rest[1..].to_vec(),
    }))
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

/// Appends the sentinel wrap instruction to a typed command, asking the target
/// to fence its reply between the per-run markers.
fn wrap_command(cmd: &str, begin: &str, end: &str) -> String {
    format!(
        "{cmd}\n\nIMPORTANT: Output ONLY your answer, wrapped exactly between \
         the marker {begin} on its own line before it and the marker {end} on \
         its own line after it. Do not include the markers anywhere else."
    )
}

/// Extracts the text between the LAST begin/end marker pair in `text`. Using the
/// last pair skips the echoed instruction (which appears earlier in the
/// transcript) and grabs the model's actual fenced reply. Returns `None` if
/// either marker is missing.
fn extract_between(text: &str, begin: &str, end: &str) -> Option<String> {
    let e = text.rfind(end)?; // last END
    let b = text[..e].rfind(begin)?; // last BEGIN before it
    let inner = &text[b + begin.len()..e];
    Some(inner.trim().to_string())
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

    match run(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("flat-cyborg: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> flat_cyborg::Result<ExitCode> {
    let session = PtySession::spawn(&args.program, &args.program_args)?;

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
    let marker = args.response_marker.clone();
    // Generate the per-run sentinel pair once so the same markers are used both
    // for wrapping the typed --cmd and for extracting the reply afterwards.
    let sentinels = args.extract.then(sentinels);
    let mut wrapper = Wrapper::with_config(session, args.config);
    let mut last = Outcome::Completed;
    for cmd in &args.cmds {
        // With --extract, append a wrap instruction so the target fences its
        // reply between the per-run markers. Kept a CLI concern; the wrapper
        // library stays unaware of sentinels.
        let effective = match &sentinels {
            Some((begin, end)) => wrap_command(cmd, begin, end),
            None => cmd.clone(),
        };
        last = wrapper.run_command(&effective)?;
        if last == Outcome::TimedOut {
            break;
        }
    }
    print_capture(&wrapper, tui, marker.as_deref(), sentinels.as_ref());
    Ok(exit_code_for(&mut wrapper, last))
}

/// Capture mode: run the target to completion, print its sanitized output.
fn capture(session: PtySession, args: Args) -> flat_cyborg::Result<ExitCode> {
    let tui = args.config.tui;
    let marker = args.response_marker.clone();
    // --extract has nothing to wrap here (no --cmd selects orchestrator mode),
    // but extraction from the captured output is still honored.
    let sentinels = args.extract.then(sentinels);
    let mut wrapper = Wrapper::with_config(session, args.config);
    let outcome = wrapper.wait_until_idle()?;
    print_capture(&wrapper, tui, marker.as_deref(), sentinels.as_ref());
    Ok(exit_code_for(&mut wrapper, outcome))
}

/// Prints the captured output: the rendered screen in TUI mode, otherwise the
/// line-oriented sanitized log.
///
/// `--extract` (via `sentinels`) takes precedence over `--profile`/
/// `--response-marker`: it reads the full transcript (including scrolled-off
/// lines in TUI mode) and prints only the text between the last marker pair.
fn print_capture(
    wrapper: &Wrapper,
    tui: bool,
    marker: Option<&str>,
    sentinels: Option<&(String, String)>,
) {
    if let Some((begin, end)) = sentinels {
        let text = if tui {
            wrapper.screen_full_text()
        } else {
            wrapper.clean_log()
        };
        match extract_between(&text, begin, end) {
            Some(s) => println!("{s}"),
            None => eprintln!(
                "flat-cyborg: --extract markers not found in output \
                 (the target may not have followed the wrap instruction)"
            ),
        }
        io::stdout().flush().ok();
        return;
    }
    if let Some(m) = marker {
        let text = if tui {
            wrapper.screen_text()
        } else {
            wrapper.clean_log()
        };
        let out: String = text
            .lines()
            .filter_map(|line| {
                let t = line.trim_start();
                t.strip_prefix(m).map(|rest| rest.trim_start().to_string())
            })
            .map(|s| s + "\n")
            .collect();
        print!("{out}");
    } else if tui {
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
    fn extract_between_picks_last_pair() {
        // The transcript contains the echoed instruction (an earlier mention of
        // the markers in prose) followed by the model's real fenced reply.
        let begin = "FCB_abc123_BEGIN";
        let end = "FCB_abc123_END";
        let transcript = format!(
            "> summarize\n\nIMPORTANT: wrap between {begin} and {end}.\n\
             {begin}\nLine one of the answer.\nLine two of the answer.\n{end}\n"
        );
        let got = extract_between(&transcript, begin, end).unwrap();
        assert_eq!(got, "Line one of the answer.\nLine two of the answer.");
    }

    #[test]
    fn extract_between_multiline_reply_retained() {
        let begin = "FCB_x_BEGIN";
        let end = "FCB_x_END";
        let body: Vec<String> = (0..50).map(|i| format!("answer line {i}")).collect();
        let joined = body.join("\n");
        let transcript = format!("noise before\n{begin}\n{joined}\n{end}\ntrailing noise");
        let got = extract_between(&transcript, begin, end).unwrap();
        assert_eq!(got, joined);
    }

    #[test]
    fn extract_between_missing_markers_is_none() {
        assert_eq!(extract_between("no markers here", "B", "E"), None);
    }

    #[test]
    fn extract_between_only_one_marker_is_none() {
        // BEGIN present but no END.
        assert_eq!(extract_between("FCB_B reply text", "FCB_B", "FCB_E"), None);
        // END present but no BEGIN.
        assert_eq!(extract_between("reply text FCB_E", "FCB_B", "FCB_E"), None);
    }

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
    fn wrap_command_appends_markers() {
        let w = wrap_command("hello", "B_BEGIN", "B_END");
        assert!(w.starts_with("hello\n\n"));
        assert!(w.contains("B_BEGIN"));
        assert!(w.contains("B_END"));
    }
}
