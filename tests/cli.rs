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
    // A target that never finishes; the graceful watchdog interrupts it and the
    // CLI reports the conventional timeout code. The hard cap is raised above
    // `--timeout-ms` so the graceful watchdog (not the hard cap, whose default
    // equals `--timeout-ms`) is the arm exercised here.
    let out = Command::new(bin())
        .args([
            "--timeout-ms",
            "400",
            "--hard-timeout-ms",
            "30000",
            "--",
            "sh",
            "-c",
            "sleep 30",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(124), "expected timeout exit 124");
}

#[test]
fn hard_timeout_default_exits_69() {
    // The hard cap defaults to `--timeout-ms`, so a target that never finishes is
    // bounded by the cap (immediate SIGKILL) at `--timeout-ms` and exits the
    // reserved 69 — pre-empting the graceful watchdog's 124. This is the tight
    // guaranteed bound that also fires under continuous data (issue #81).
    let out = Command::new(bin())
        .args(["--timeout-ms", "400", "--", "sh", "-c", "sleep 30"])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert_eq!(
        out.status.code(),
        Some(69),
        "expected the default hard cap to exit 69"
    );
}

#[test]
fn hard_timeout_ms_rejects_a_non_numeric_value() {
    let out = Command::new(bin())
        .args(["--hard-timeout-ms", "soon", "--", "true"])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert_eq!(
        out.status.code(),
        Some(2),
        "bad flag value must be a usage error"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invalid --hard-timeout-ms"),
        "stderr: {stderr:?}"
    );
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
    // End-to-end multi-`--cmd` run, with the two completion paths in one run:
    // the "model" prints a banner, answers the FIRST prompt without any markers
    // (so that command completes on the marker-less grace), and answers the
    // SECOND by fencing ANSWER between that prompt's own markers (picked out of
    // the wrap instruction) with NOTHING after the closing one — so it completes
    // on its sentinel and the target then goes completely silent.
    //
    // Two regressions meet here. That silence used to strand the next command's
    // pre-typing readiness wait: the run ended `124` with an empty capture
    // because the second prompt was never typed. And the marker-less report is
    // per-wait state, so a stale one from command 1 must not be attributed to
    // command 2 — a fenced reply reported as marker-less is a false entry in the
    // rate operators measure from that line.
    let out = Command::new(bin())
        .args([
            "--extract-structural",
            "--no-jitter",
            "--idle-ms",
            "300",
            "--extract-grace-ms",
            "700",
            "--timeout-ms",
            "10000",
            "--cmd",
            "alpha",
            "--cmd",
            "bravo",
            "--",
            "sh",
            "-c",
            "n=1; printf 'banner\\n'; while read l; do \
             if [ \"$n\" = 1 ]; then printf 'UNFENCED\\n'; else \
             b=; e=; for w in $l; do \
             case $w in FCB_*_BEGIN) b=$w ;; FCB_*_END) e=$w ;; esac; done; \
             printf '%s\\nANSWER\\n%s\\n' \"$b\" \"$e\"; fi; n=$((n+1)); done",
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
    // The run ENDED on command 2's sentinel, so it must not be reported as
    // marker-less on account of command 1 having been.
    assert!(
        !stderr.contains("marker-less grace"),
        "a fenced final reply was reported as marker-less (stale state from the \
         earlier command): {stderr:?}"
    );
    // Only the LAST command's fenced reply is printed, and it is the reply
    // itself — not chrome, not the echoed instruction, not command 1's UNFENCED.
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

/// A unique temp path for a result-file test, cleaned up before use so a stale
/// file from an earlier run cannot be mistaken for this run's reply.
fn result_file_path(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("fcb-result-{}-{}.txt", std::process::id(), tag));
    std::fs::remove_file(&p).ok();
    p
}

#[test]
fn result_file_is_preferred_over_the_screen() {
    // The target writes one reply to the --result-file path and prints a
    // DIFFERENT sentinel-fenced reply on screen. flat-cyborg must print the FILE
    // contents (file > screen) and exit 0. The banner makes the target render
    // early so the pre-typing readiness wait passes and the command is typed.
    let path = result_file_path("preferred");
    let script = format!(
        "printf 'BANNER\\n'; read l; b=; e=; for w in $l; do \
         case $w in FCB_*_BEGIN) b=$w ;; FCB_*_END) e=$w ;; esac; done; \
         printf 'FILE_REPLY\\n' > '{}'; \
         printf '%s\\nSCREEN_REPLY\\n%s\\n' \"$b\" \"$e\"",
        path.display()
    );
    let out = Command::new(bin())
        .args([
            "--extract",
            "--no-jitter",
            "--idle-ms",
            "300",
            "--timeout-ms",
            "10000",
            "--result-file",
            &path.to_string_lossy(),
            "--cmd",
            "hi",
            "--",
            "sh",
            "-c",
            &script,
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run with --result-file");
    std::fs::remove_file(&path).ok();
    assert!(out.status.success(), "exit: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim(),
        "FILE_REPLY",
        "the file reply must win over the screen: {stdout:?}"
    );
}

#[test]
fn result_file_empty_falls_back_byte_identically_with_a_diagnostic() {
    // --result-file set but never written by the target: flat-cyborg must emit
    // ONE loud stderr diagnostic naming the no-op AND fall through to the screen
    // extraction, byte-identical to the same run without --result-file.
    let path = result_file_path("empty"); // deliberately never created
    let script = "printf 'BANNER\\n'; read l; b=; e=; for w in $l; do \
         case $w in FCB_*_BEGIN) b=$w ;; FCB_*_END) e=$w ;; esac; done; \
         printf '%s\\nSCREEN_REPLY\\n%s\\n' \"$b\" \"$e\"";
    let base_args = [
        "--extract",
        "--no-jitter",
        "--idle-ms",
        "300",
        "--timeout-ms",
        "10000",
        "--cmd",
        "hi",
        "--",
        "sh",
        "-c",
        script,
    ];
    // Without --result-file: the reference output.
    let plain = Command::new(bin())
        .args(base_args)
        .stdin(Stdio::null())
        .output()
        .expect("run without --result-file");
    // With --result-file pointing at an unwritten path.
    let armed = Command::new(bin())
        .args(["--result-file", &path.to_string_lossy()])
        .args(base_args)
        .stdin(Stdio::null())
        .output()
        .expect("run with an unwritten --result-file");
    std::fs::remove_file(&path).ok();
    assert!(plain.status.success() && armed.status.success());
    assert_eq!(
        armed.stdout, plain.stdout,
        "the fallback must be byte-identical to a run without --result-file"
    );
    let stderr = String::from_utf8_lossy(&armed.stderr);
    assert!(
        stderr.contains("empty or unwritten"),
        "expected the result-file no-op diagnostic, stderr: {stderr:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&armed.stdout).trim(),
        "SCREEN_REPLY",
        "the screen reply must still be recovered on fallback"
    );
}

#[test]
fn result_file_hit_exits_0_even_on_a_watchdog_timeout() {
    // The exit-0-on-file-hit override: the target writes the reply to the file
    // then sleeps without ever fencing it, so strict --extract never completes
    // and the graceful watchdog fires (a 124 without --result-file). Because the
    // answer already reached the caller via the file, the run must exit 0 and
    // print the file contents. The hard cap is raised above --timeout-ms so the
    // graceful watchdog (not the hard cap) is the outcome being overridden.
    let path = result_file_path("override");
    let script = format!(
        "printf 'BANNER\\n'; read l; printf 'FILE_ONLY_REPLY\\n' > '{}'; sleep 30",
        path.display()
    );
    let out = Command::new(bin())
        .args([
            "--extract",
            "--no-jitter",
            "--idle-ms",
            "300",
            "--timeout-ms",
            "800",
            "--hard-timeout-ms",
            "30000",
            "--result-file",
            &path.to_string_lossy(),
            "--cmd",
            "hi",
            "--",
            "sh",
            "-c",
            &script,
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run with a timing-out --result-file target");
    std::fs::remove_file(&path).ok();
    assert_eq!(
        out.status.code(),
        Some(0),
        "a non-empty result file must override the timeout exit code; stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "FILE_ONLY_REPLY",
        "the file reply must be printed even after the timeout"
    );
}

#[test]
fn result_file_without_extract_is_a_usage_error() {
    // The directive rides the --extract sentinel wrap, so --result-file without
    // --extract is a usage error (exit 2), naming the requirement.
    let out = Command::new(bin())
        .args([
            "--result-file",
            "/tmp/x",
            "--cmd",
            "hi",
            "--",
            "sh",
            "-c",
            "true",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run --result-file without --extract");
    assert_eq!(out.status.code(), Some(2), "expected usage exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--result-file requires --extract"),
        "stderr: {stderr:?}"
    );
}

#[test]
fn result_file_stale_content_on_a_reused_path_is_not_read_as_the_new_reply() {
    // Regression (PR #84 review): a driver reuses ONE fixed --result-file path
    // across turns (what $FLAT_CYBORG_RESULT_FILE_PATH invites). Turn 1 writes a
    // reply and fences it (completes, exit 0, file now holds ANSWER_ONE). Turn 2
    // asks a different question, writes NOTHING, and hangs → a real timeout. The
    // arm-time truncation must clear the stale ANSWER_ONE before turn 2, so
    // flat-cyborg must NOT print turn 1's content and must NOT exit 0: the read
    // falls through to the screen (no fence there) and the watchdog timeout (124)
    // surfaces. Without the guard this printed ANSWER_ONE and exited 0.
    let path = result_file_path("stale-reuse");
    let script = format!(
        "n=1; printf 'BANNER\\n'; while read l; do \
         b=; e=; for w in $l; do \
         case $w in FCB_*_BEGIN) b=$w ;; FCB_*_END) e=$w ;; esac; done; \
         if [ \"$n\" = 1 ]; then printf 'ANSWER_ONE\\n' > '{}'; \
         printf '%s\\nDONE_ONE\\n%s\\n' \"$b\" \"$e\"; \
         else sleep 30; fi; n=$((n+1)); done",
        path.display()
    );
    let out = Command::new(bin())
        .args([
            "--extract",
            "--no-jitter",
            "--idle-ms",
            "300",
            "--timeout-ms",
            "800",
            "--hard-timeout-ms",
            "30000",
            "--result-file",
            &path.to_string_lossy(),
            "--cmd",
            "q1",
            "--cmd",
            "q2",
            "--",
            "sh",
            "-c",
            &script,
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run two turns on one reused --result-file path");
    std::fs::remove_file(&path).ok();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("ANSWER_ONE"),
        "turn 1's stale reply must not be read as turn 2's answer: {stdout:?}"
    );
    assert_ne!(
        out.status.code(),
        Some(0),
        "a stale file must not arm the exit-0 override on a genuine timeout; \
         stdout: {stdout:?}, stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.status.code(),
        Some(124),
        "the real watchdog timeout must surface once the stale file is cleared"
    );
}
