//! Smoke tests for the `flat-cyborg` demo binary.

use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_flat-cyborg")
}

#[test]
fn help_is_printed() {
    let out = Command::new(bin())
        .arg("--help")
        .output()
        .expect("run --help");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("USAGE"), "help missing usage: {stdout}");
    assert!(stdout.contains("flat-cyborg"));
}

#[test]
fn missing_separator_is_an_error() {
    let out = Command::new(bin())
        .args(["sh"])
        .output()
        .expect("run without --");
    // Usage errors exit with code 2.
    assert_eq!(out.status.code(), Some(2), "expected exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--"), "stderr: {stderr}");
}

#[test]
fn help_after_separator_is_not_hijacked() {
    // `--help` *after* `--` belongs to the target, not flat-cyborg: capture
    // mode should run `echo --help` and print "--help", not the wrapper usage.
    let out = Command::new(bin())
        .args(["--", "echo", "--help"])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert!(out.status.success(), "exit: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--help"), "stdout: {stdout:?}");
    assert!(
        !stdout.contains("USAGE"),
        "wrapper help was hijacked: {stdout:?}"
    );
}

#[test]
fn version_prints_and_is_not_hijacked_after_separator() {
    // `version` subcommand prints the crate version.
    let out = Command::new(bin())
        .arg("version")
        .stdin(Stdio::null())
        .output()
        .expect("run version");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("flat-cyborg "),
        "version output: {stdout:?}"
    );

    // `--version` *after* `--` belongs to the target, not flat-cyborg.
    // `printf '%s\n' --version` echoes the literal operand (unlike `echo`,
    // whose GNU build would interpret `--version`).
    let out = Command::new(bin())
        .args(["--", "printf", "%s\\n", "--version"])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--version"), "stdout: {stdout:?}");
    assert!(
        !stdout.contains("flat-cyborg 0"),
        "flat-cyborg version was hijacked: {stdout:?}"
    );
}

#[test]
fn capture_mode_propagates_target_exit_code() {
    let out = Command::new(bin())
        .args(["--", "sh", "-c", "exit 7"])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert_eq!(
        out.status.code(),
        Some(7),
        "target exit code not propagated"
    );
}

#[test]
fn watchdog_timeout_exits_124() {
    // A target that never finishes; the watchdog interrupts it and the CLI
    // reports the conventional timeout code.
    let out = Command::new(bin())
        .args(["--timeout-ms", "400", "--", "sh", "-c", "sleep 30"])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(124), "expected timeout exit 124");
}

#[test]
fn capture_mode_prints_sanitized_output() {
    // Piped stdin (not a TTY) selects capture mode: run the target to
    // completion and print its ANSI-stripped output.
    let out = Command::new(bin())
        .args([
            "--",
            "sh",
            "-c",
            "printf '\\033[32mGREEN\\033[0m and \\033[1mBOLD\\033[0m\\n'",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run capture");

    assert!(out.status.success(), "exit: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("GREEN and BOLD"), "stdout: {stdout:?}");
    assert!(
        !stdout.contains('\u{1b}'),
        "ANSI escape leaked into output: {stdout:?}"
    );
}

#[test]
fn response_marker_extracts_only_marked_lines() {
    let out = Command::new(bin())
        .args([
            "--response-marker",
            "@@",
            "--",
            "sh",
            "-c",
            "printf 'noise\\n@@ first\\nmore noise\\n@@ second\\n'",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert!(out.status.success(), "exit: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout, "first\nsecond\n", "got: {stdout:?}");
}

#[test]
fn profile_claude_sets_the_reply_marker() {
    let out = Command::new(bin())
        .args([
            "--profile",
            "claude",
            "--",
            "sh",
            "-c",
            "printf 'banner\\n● hello\\nnoise\\n● world\\n'",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert!(out.status.success(), "exit: {:?}", out.status);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello\nworld\n");
}

#[test]
fn explicit_response_marker_overrides_profile() {
    // --response-marker wins over the profile's marker, regardless of order.
    let out = Command::new(bin())
        .args([
            "--profile",
            "claude",
            "--response-marker",
            "@@",
            "--",
            "sh",
            "-c",
            "printf '● circle\\n@@ at\\n'",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "at\n");
}

#[test]
fn unknown_profile_is_an_error() {
    let out = Command::new(bin())
        .args(["--profile", "bogus", "--", "sh", "-c", "echo hi"])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown --profile"));
}

#[test]
fn no_extract_output_unchanged() {
    // Regression: without --extract, capture-mode output is the cleaned log
    // verbatim — the new flag must not alter the default path.
    let out = Command::new(bin())
        .args(["--", "sh", "-c", "printf 'plain output\\nmore\\n'"])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert!(out.status.success(), "exit: {:?}", out.status);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "plain output\nmore\n");
}

#[test]
fn extract_without_markers_warns_and_prints_nothing() {
    // The per-run markers are random, so a static target cannot reproduce them.
    // When the markers are absent from the output, --extract prints nothing to
    // stdout and emits a clear warning on stderr.
    let out = Command::new(bin())
        .args(["--extract", "--", "sh", "-c", "printf 'no markers here\\n'"])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert!(out.status.success(), "exit: {:?}", out.status);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("markers not found"),
        "expected a not-found warning, got stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}
