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
fn cwd_runs_target_in_the_given_directory() {
    // Capture mode (piped stdin): `--cwd /tmp` makes the target's `pwd` print
    // the override, not flat-cyborg's own working directory. `/tmp` is
    // dash-safe and present on every Unix test host.
    let out = Command::new(bin())
        .args(["--cwd", "/tmp", "--", "sh", "-c", "pwd"])
        .stdin(Stdio::null())
        .output()
        .expect("run with --cwd");
    assert!(out.status.success(), "exit: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("/tmp"),
        "target did not run in --cwd: {stdout:?}"
    );
}

#[test]
fn cwd_nonexistent_is_a_usage_error() {
    // A missing --cwd directory is a usage error: exit 2 with a clear message.
    let out = Command::new(bin())
        .args(["--cwd", "/nonexistent-XYZ", "--", "sh", "-c", "true"])
        .stdin(Stdio::null())
        .output()
        .expect("run with bad --cwd");
    assert_eq!(out.status.code(), Some(2), "expected usage exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cwd does not exist"), "stderr: {stderr:?}");
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
fn no_jitter_types_a_large_command_in_one_burst() {
    // --no-jitter is the make-or-break path for programmatic drivers: a large
    // --cmd must be typed at once, not char-by-char over minutes. Drive an
    // interactive shell, echo a long string, and assert the round-trip both
    // produces the output and finishes well under the per-char-jitter time
    // (3000 chars at ~40-300 ms each would be minutes; the watchdog ceiling
    // here is generous but the burst must beat it comfortably).
    use std::time::Instant;
    let payload = "z".repeat(3000);
    let cmd = format!("printf 'LEN=%s\\n' \"$(printf %s '{payload}' | wc -c)\"");
    let start = Instant::now();
    let out = Command::new(bin())
        .args([
            "--no-jitter",
            "--idle-ms",
            "400",
            "--timeout-ms",
            "20000",
            "--prompt",
            "READY> ",
            "--cmd",
            &cmd,
            "--",
            "sh",
            "-c",
            "PS1='READY> '; export PS1; exec sh -i",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run with --no-jitter");
    let elapsed = start.elapsed();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("LEN=3000"),
        "the 3000-char command was not typed/executed; stdout: {stdout:?}"
    );
    // Per-char jitter on 3000 chars would be on the order of minutes; the burst
    // path must be far faster. 15s is a wide CI-safe margin that the jittered
    // path could never meet.
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "--no-jitter typing was not a single burst, took {elapsed:?}"
    );
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
        String::from_utf8_lossy(&out.stderr).contains("no fenced reply"),
        "expected a no-fenced-reply warning, got stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn extract_grace_ms_rejects_a_non_numeric_value() {
    // A bad --extract-grace-ms is a usage error: exit 2, naming the flag.
    let out = Command::new(bin())
        .args(["--extract-grace-ms", "soon", "--", "sh", "-c", "true"])
        .stdin(Stdio::null())
        .output()
        .expect("run with a bad --extract-grace-ms");
    assert_eq!(out.status.code(), Some(2), "expected usage exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invalid --extract-grace-ms"),
        "stderr: {stderr:?}"
    );
}

#[test]
fn extract_grace_ms_zero_completes_on_a_settled_screen() {
    // `--extract-grace-ms 0` is the escape hatch: it restores the pre-0.13.0
    // `--extract-structural` behaviour where a settled screen completes the run
    // at once. The target prints one line and then sleeps forever without ever
    // emitting the closing marker, so:
    //   * the run must finish on the settled screen (not on the watchdog: no 124),
    //   * far faster than the default grace would allow — with --idle-ms 300 and
    //     --timeout-ms 60000 the default is min(max(4x300ms, 30s), 30s) = 30s, so
    //     finishing well under that can only be the zero grace (the bound is
    //     deliberately loose for CI jitter),
    //   * and the marker-less completion must be reported on stderr.
    // --no-jitter keeps the typing itself out of the measured time.
    use std::time::Instant;
    let start = Instant::now();
    let out = Command::new(bin())
        .args([
            "--extract-structural",
            "--extract-grace-ms",
            "0",
            "--no-jitter",
            "--idle-ms",
            "300",
            "--timeout-ms",
            "60000",
            "--cmd",
            "ping",
            "--",
            "sh",
            "-c",
            "printf 'hello\\n'; sleep 30",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run with --extract-grace-ms 0");
    let elapsed = start.elapsed();
    assert!(out.status.success(), "exit: {:?}", out.status);
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "a zero grace must complete on the settled screen, took {elapsed:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The reported number is the OBSERVED quiet window (here roughly --idle-ms),
    // not the configured grace — a grace of 0 ms is never how long the screen
    // was actually quiet.
    assert!(
        stderr.contains("marker-less grace ("),
        "expected the marker-less completion diagnostic, stderr: {stderr:?}"
    );
    // `sh` is not a known CLI, so nothing is scraped: stdout stays empty.
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
}

#[test]
fn two_commands_are_both_delivered_and_the_last_reply_is_printed() {
    // End-to-end multi-`--cmd` run. The "model" prints a banner, then answers
    // each prompt by fencing ANSWER between that prompt's own markers (picked
    // out of the wrap instruction) and NOTHING after the closing one — so each
    // command completes on its own sentinel and the target is then completely
    // silent. That silence used to strand the next command's pre-typing
    // readiness wait: the run ended `124` with an empty capture because the
    // second prompt was never typed.
    let out = Command::new(bin())
        .args([
            "--extract-structural",
            "--no-jitter",
            "--idle-ms",
            "300",
            "--timeout-ms",
            "10000",
            "--cmd",
            "alpha",
            "--cmd",
            "bravo",
            "--",
            "sh",
            "-c",
            "printf 'banner\\n'; while read l; do b=; e=; for w in $l; do \
             case $w in FCB_*_BEGIN) b=$w ;; FCB_*_END) e=$w ;; esac; done; \
             printf '%s\\nANSWER\\n%s\\n' \"$b\" \"$e\"; done",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run two commands");
    assert!(
        out.status.success(),
        "multi-command run did not complete: {:?}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("no fenced reply"),
        "the last command's fence was not found, so it was never delivered: {stderr:?}"
    );
    // Only the LAST command's fenced reply is printed, and it is the reply
    // itself — not chrome, not the echoed instruction.
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ANSWER");
}

#[test]
fn no_grace_diagnostic_when_the_target_exits() {
    // The marker-less diagnostic names the completion path it belongs to. A
    // target that exits on its own never consumed the grace, so claiming it did
    // would inflate the marker-less rate operators measure from that line.
    let out = Command::new(bin())
        .args([
            "--extract-structural",
            "--no-jitter",
            "--idle-ms",
            "300",
            "--timeout-ms",
            "20000",
            "--cmd",
            "ping",
            "--",
            "sh",
            "-c",
            "printf 'hi\\n'; exit 3",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run against a target that exits");
    assert_eq!(
        out.status.code(),
        Some(3),
        "the target's own exit code must be propagated"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("marker-less grace"),
        "the grace diagnostic must not fire on a target that exited: {stderr:?}"
    );
}
