//! Acceptance tests mapping to the spec's implementation criteria (§5),
//! exercising the public library API as an external consumer would.

use std::time::{Duration, Instant};

use flat_cyborg::pty::Output;
use flat_cyborg::{Outcome, PtySession, Wrapper, WrapperConfig};

/// Runs a one-shot target to completion and returns its sanitized log.
fn run_to_completion(program: &str, args: &[&str]) -> String {
    let session = PtySession::spawn(program, args).expect("spawn target");
    let config = WrapperConfig {
        exec_timeout: Duration::from_secs(10),
        idle_silence: Duration::from_millis(200),
        poll_interval: Duration::from_millis(50),
        ..WrapperConfig::default()
    };
    let mut wrapper = Wrapper::with_config(session, config);
    let outcome = wrapper.wait_until_idle().expect("wait");
    assert_ne!(outcome, Outcome::TimedOut, "target unexpectedly timed out");
    wrapper.clean_log()
}

/// Criterion 1: the target detects a fully-featured TTY and launches in
/// interactive mode rather than headless mode.
#[test]
fn target_detects_a_full_tty() {
    let log = run_to_completion(
        "sh",
        &[
            "-c",
            "if [ -t 0 ] && [ -t 1 ] && [ -t 2 ]; then echo INTERACTIVE_MODE; else echo HEADLESS; fi",
        ],
    );
    assert!(log.contains("INTERACTIVE_MODE"), "log: {log:?}");
    assert!(!log.contains("HEADLESS"), "log: {log:?}");
}

/// Criterion 2: sub-processes spawned by the target also run with a working
/// TTY context (the wrapper does not break the TTY for children).
#[test]
fn subprocesses_inherit_the_tty_context() {
    let log = run_to_completion("sh", &["-c", "sh -c '[ -t 1 ] && echo SUBPROCESS_HAS_TTY'"]);
    assert!(log.contains("SUBPROCESS_HAS_TTY"), "log: {log:?}");
}

/// Criterion 4: the sanitized output contains no unparsed ANSI control
/// characters and no loading-spinner artifacts.
#[test]
fn sanitized_output_has_no_ansi_or_spinner_artifacts() {
    // Emit a colored line, then a carriage-return spinner that erases to end of
    // line each frame, then a final line.
    let script = concat!(
        "printf '\\033[31mERROR\\033[0m text\\n'; ",
        "printf '\\rworking|\\033[K'; ",
        "printf '\\rworking/\\033[K'; ",
        "printf '\\rworking-\\033[K'; ",
        "printf '\\rFINISHED\\033[K\\n'"
    );
    let log = run_to_completion("sh", &["-c", script]);

    assert!(
        !log.contains('\u{1b}'),
        "ANSI escape leaked into log: {log:?}"
    );
    assert!(log.contains("ERROR text"), "log: {log:?}");
    assert!(log.contains("FINISHED"), "log: {log:?}");
    // The intermediate spinner frames collapsed away.
    assert!(
        !log.contains("working|"),
        "spinner artifact survived: {log:?}"
    );
    assert!(
        !log.contains("working/"),
        "spinner artifact survived: {log:?}"
    );
}

/// Criterion 3: I/O is asynchronous and non-blocking — early output is readable
/// well before a slow target finishes.
#[test]
fn output_streams_without_blocking() {
    let session =
        PtySession::spawn("sh", ["-c", "echo FIRST; sleep 2; echo SECOND"]).expect("spawn");

    let start = Instant::now();
    let mut seen = String::new();
    while start.elapsed() < Duration::from_secs(2) {
        match session.read_output(Duration::from_millis(100)) {
            Output::Data(chunk) => {
                seen.push_str(&String::from_utf8_lossy(&chunk));
                if seen.contains("FIRST") {
                    break;
                }
            }
            Output::Idle => continue,
            Output::Eof => break,
        }
    }

    assert!(
        seen.contains("FIRST"),
        "did not stream early output: {seen:?}"
    );
    assert!(
        start.elapsed() < Duration::from_millis(1500),
        "early output arrived too late ({:?}); reads appear to block",
        start.elapsed()
    );
    // `session` drops here, SIGKILLing the lingering `sleep 2`.
}
