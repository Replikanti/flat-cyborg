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
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--"), "stderr: {stderr}");
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
