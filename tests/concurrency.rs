//! Deterministic ≥2-concurrent-session reproduction harness for issue #71
//! (M1 spike: reproduce + classify, no fix here).
//!
//! Several `flat-cyborg` processes are driven in parallel against the local
//! fake-LLM fixture `tests/fixtures/slow_chrome_target.sh`, which emits a
//! slow, TUI-chrome-like stream and the `FCB_*_BEGIN/END` sentinel shape — no
//! live model, no network. The bounded (N = 2..4, single iteration) subset runs
//! in normal `cargo test` and asserts the concurrency invariants; the heavy
//! stress loop is `#[ignore]`d behind `FCB_STRESS=1` so required CI stays stable.
//!
//! Run the heavy loop on demand:
//!   FCB_STRESS=1 cargo test --test concurrency -- --ignored
//! and with the diagnostics on to see which arm fires:
//!   FLAT_CYBORG_DIAG=1 FCB_STRESS=1 cargo test --test concurrency -- --ignored --nocapture

use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// The wrapper binary under test.
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_flat-cyborg")
}

/// Absolute path to the fake-LLM fixture.
fn fixture() -> String {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/slow_chrome_target.sh"
    )
    .to_string()
}

/// Runs one gated `flat-cyborg` session against the fixture with a slow reply.
/// `extract_flag` selects the completion-gate flavour (`--extract` strict or
/// `--extract-structural`); `env` are extra child env knobs (fixture behaviour
/// + diag).
fn run_session_extract(reply_delay_ms: u32, extract_flag: &str, env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(bin());
    cmd.args([
        extract_flag,
        "--no-jitter",
        "--idle-ms",
        "300",
        "--timeout-ms",
        "15000",
        "--cmd",
        "ping",
        "--",
        "sh",
        &fixture(),
    ])
    .env("REPLY_DELAY_MS", reply_delay_ms.to_string())
    .stdin(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn flat-cyborg session")
}

/// Runs one `flat-cyborg --extract-structural` session (the default gate shape).
fn run_session(reply_delay_ms: u32, env: &[(&str, &str)]) -> Output {
    run_session_extract(reply_delay_ms, "--extract-structural", env)
}

/// Runs one plain-capture `flat-cyborg` session — NO `--extract`, so no
/// completion gate: an exit is the completion signal and the outcome must stay
/// `Completed` (the target's own exit code), never reclassified to 75.
fn run_session_no_gate(reply_delay_ms: u32, env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(bin());
    cmd.args([
        "--no-jitter",
        "--idle-ms",
        "300",
        "--timeout-ms",
        "15000",
        "--cmd",
        "ping",
        "--",
        "sh",
        &fixture(),
    ])
    .env("REPLY_DELAY_MS", reply_delay_ms.to_string())
    .stdin(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn flat-cyborg session")
}

/// Absolute path to the `claude`-BASENAME fixture. Its filename drives the
/// structural-extraction dispatch (`extract_for_target` keys on the basename),
/// so it is invoked DIRECTLY (not via `sh`) to exercise the marker-less
/// structural fallback — see the file header.
fn claude_fixture() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude").to_string()
}

/// Runs one gated `flat-cyborg` session against the `claude`-basename fixture,
/// which renders a complete but MARKER-LESS reply and then dies. `extract_flag`
/// selects strict `--extract` (no structural fallback) vs `--extract-structural`.
/// Diagnostics on so the test can assert the classification.
fn run_session_claude(extract_flag: &str) -> Output {
    let mut cmd = Command::new(bin());
    cmd.args([
        extract_flag,
        "--no-jitter",
        "--idle-ms",
        "300",
        "--timeout-ms",
        "15000",
        "--cmd",
        "ping",
        "--",
        &claude_fixture(),
    ])
    .env("FLAT_CYBORG_DIAG", "1")
    .stdin(Stdio::null());
    cmd.output().expect("spawn flat-cyborg claude session")
}

/// Asserts a session captured the fenced reply cleanly and did not self-fault.
fn assert_clean_capture(out: &Output) {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "session did not succeed: status={:?} stderr={stderr:?}",
        out.status
    );
    assert_eq!(
        stdout.trim(),
        "PONG",
        "captured reply was not the fenced answer: stdout={stdout:?} stderr={stderr:?}"
    );
    // Never leak chrome or sentinel fragments into the captured reply.
    assert!(
        !stdout.contains("Thinking") && !stdout.contains("FCB_") && !stdout.contains('█'),
        "capture leaked chrome/sentinel fragments: {stdout:?}"
    );
    assert!(
        !stderr.contains("panic"),
        "flat-cyborg panicked: {stderr:?}"
    );
}

/// Bounded, deterministic concurrency assertion for required CI: N sessions in
/// parallel against a slow reply each complete on their own sentinel and
/// capture the fenced answer — none self-faults, none captures another's reply
/// or chrome.
#[test]
fn concurrent_sessions_each_capture_their_reply() {
    const N: usize = 4;
    let outs: Vec<Output> = thread::scope(|s| {
        let handles: Vec<_> = (0..N).map(|_| s.spawn(|| run_session(300, &[]))).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for out in &outs {
        assert_clean_capture(out);
    }
}

/// The diagnostics are OFF by default: an ordinary run emits no `FCB_DIAG`
/// record, so instrumentation is zero-behaviour-change when the flag is unset,
/// and turning it on does emit records (to stderr only, never stdout).
#[test]
fn diagnostics_are_off_by_default() {
    let off = run_session(200, &[]);
    let off_err = String::from_utf8_lossy(&off.stderr);
    assert!(
        !off_err.contains("FCB_DIAG"),
        "diagnostics leaked while disabled: {off_err:?}"
    );

    let on = run_session(200, &[("FLAT_CYBORG_DIAG", "1")]);
    let on_err = String::from_utf8_lossy(&on.stderr);
    let on_out = String::from_utf8_lossy(&on.stdout);
    assert!(
        on_err.contains("FCB_DIAG"),
        "diagnostics did not emit when enabled: {on_err:?}"
    );
    assert!(
        !on_out.contains("FCB_DIAG"),
        "diagnostics contaminated stdout capture: {on_out:?}"
    );
    // The clean completion path must be visible in the classification.
    assert!(
        on_err.contains("wrapper.gate-open"),
        "expected the sentinel-gate completion record: {on_err:?}"
    );
}

/// A target that dies mid-reply (before the closing sentinel) under a configured
/// `--extract` gate is CLASSIFIED as a distinct, retry-able transient: the M2
/// fix maps it to the reserved exit code 75 (`EX_TEMPFAIL`) instead of the
/// target's ambiguous passthrough status. The diagnostics show it completed via
/// EOF while un-interrupted, with the gate never opened — this is the prime #71
/// suspect arm, reproduced deterministically without needing high concurrency.
#[test]
fn target_death_midway_is_classified_via_eof() {
    let out = run_session(200, &[("DIE_MIDWAY", "1"), ("FLAT_CYBORG_DIAG", "1")]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The contract: exit 75 = target vanished mid-reply (retry-able transient),
    // NOT the target's own exit(1) passthrough.
    assert_eq!(
        out.status.code(),
        Some(75),
        "expected reserved exit 75 (target exited mid-reply): status={:?} stderr={stderr:?}",
        out.status
    );
    assert!(
        stderr.contains("wrapper.eof") && stderr.contains("interrupted=false"),
        "expected the un-interrupted EOF classification: {stderr:?}"
    );
    assert!(
        stderr.contains("outcome=TargetExitedEarly"),
        "expected the target-vanished classification: {stderr:?}"
    );
    // The human-readable (non-contract) observability line is emitted too.
    assert!(
        stderr.contains("the target exited before completing its reply"),
        "expected the observability stderr line: {stderr:?}"
    );
    // No fenced reply arrived, so nothing chrome-like is printed to stdout.
    assert!(
        !stdout.contains('█') && !stdout.contains("FCB_"),
        "capture leaked chrome/sentinel on target death: {stdout:?}"
    );
}

/// The crux guard: a target that emits a CLEAN fenced reply and THEN exits is
/// NOT reclassified as a mid-reply death. The reply completed on the marker
/// (Idle, exit 0) before EOF was ever read, so exit 75 must not fire.
#[test]
fn clean_reply_then_exit_is_not_flagged_as_target_death() {
    let out = run_session(200, &[("EXIT_AFTER_REPLY", "1")]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a clean reply-then-exit must succeed, not report the transient: \
         status={:?} stderr={stderr:?}",
        out.status
    );
    assert_eq!(
        stdout.trim(),
        "PONG",
        "the fenced reply must still be captured: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        !stderr.contains("exited before completing its reply"),
        "a clean reply must not emit the mid-reply-death line: {stderr:?}"
    );
}

/// #71 review regression: a target (a known CLI) that renders a COMPLETE,
/// chrome-free reply but drops the closing marker and THEN dies is NOT a lost
/// reply under `--extract-structural` — the structural fallback still recovers it
/// from the settled screen. The gate never opened (no marker) so the wrapper
/// classifies `TargetExitedEarly`, but because the reply reached stdout the
/// process exits 0, NOT 75. A missing closing marker must never be reported as a
/// lost reply, or a resilience layer keyed on exit 75 would retry a call that
/// already succeeded and discard the answer.
#[test]
fn target_death_after_recoverable_body_under_structural_is_success() {
    let out = run_session_claude("--extract-structural");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a structurally-recovered marker-less reply after a mid-reply death must \
         succeed, not report the transient (exit 75): status={:?} stderr={stderr:?}",
        out.status
    );
    assert!(
        stdout.contains("PONG_STRUCTURAL_REPLY"),
        "the structurally-recovered reply must reach stdout: stdout={stdout:?} stderr={stderr:?}"
    );
    // The wrapper still classifies the EOF arm as TargetExitedEarly; only the
    // exit-code mapping downgrades to success because the reply was recovered.
    assert!(
        stderr.contains("outcome=TargetExitedEarly"),
        "expected the un-interrupted mid-reply-death classification: {stderr:?}"
    );
    // ...and the "reply lost" observability line is suppressed on recovery.
    assert!(
        !stderr.contains("exited before completing its reply"),
        "the mid-reply-death line must be suppressed when the reply was recovered: {stderr:?}"
    );
}

/// The strict counterpart: the SAME marker-less reply under strict `--extract`
/// (no structural fallback) is genuinely unrecoverable, so the reply IS lost and
/// exit 75 is the correct signal — proving the exit-0 downgrade above is driven
/// by an actually-recovered reply, not merely by the outcome.
#[test]
fn target_death_marker_less_reply_under_strict_extract_is_exit_75() {
    let out = run_session_claude("--extract");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(75),
        "a marker-less reply strict --extract cannot recover is a lost reply \
         (exit 75): status={:?} stderr={stderr:?}",
        out.status
    );
}

/// Strict `--extract` (no structural fallback) still configures the completion
/// gate, so a mid-reply death is classified as the transient there too: the
/// exit-75 contract does not depend on `--extract-structural`.
#[test]
fn target_death_midway_under_strict_extract_is_also_exit_75() {
    let out = run_session_extract(200, "--extract", &[("DIE_MIDWAY", "1")]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(75),
        "strict --extract must also report the mid-reply transient: \
         status={:?} stderr={stderr:?}",
        out.status
    );
}

/// The no-gate path must NOT regress: plain capture (no `--extract`) has no
/// completion gate, so a target that exits — even the DIE_MIDWAY shape — stays
/// `Completed` and propagates its own exit code (1 here), never 75.
#[test]
fn plain_capture_no_gate_keeps_target_exit_code() {
    let out = run_session_no_gate(200, &[("DIE_MIDWAY", "1")]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(1),
        "no-gate capture must propagate the target's own exit, not 75: \
         status={:?} stderr={stderr:?}",
        out.status
    );
    assert!(
        !stderr.contains("exited before completing its reply"),
        "the no-gate path must not emit the mid-reply-death line: {stderr:?}"
    );
}

/// Heavy stress reproduction (issue's acceptance shape). `#[ignore]`d and gated
/// on `FCB_STRESS=1` so it never destabilises required CI; run on demand with
/// the diagnostics on to see which arm, if any, fires under real load.
#[test]
#[ignore = "heavy concurrency stress loop; run with FCB_STRESS=1 -- --ignored"]
fn stress_many_concurrent_sessions() {
    if std::env::var("FCB_STRESS").ok().as_deref() != Some("1") {
        eprintln!("skipping: set FCB_STRESS=1 to run the heavy stress loop");
        return;
    }
    const N: usize = 8;
    const ITERS: usize = 12;
    let diag = if std::env::var("FLAT_CYBORG_DIAG").is_ok() {
        vec![("FLAT_CYBORG_DIAG", "1")]
    } else {
        vec![]
    };
    let mut failures = 0usize;
    for iter in 0..ITERS {
        let outs: Vec<Output> = thread::scope(|s| {
            let handles: Vec<_> = (0..N)
                .map(|_| {
                    let diag = diag.clone();
                    s.spawn(move || run_session(300, &diag))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (j, out) in outs.iter().enumerate() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let ok = out.status.success() && stdout.trim() == "PONG";
            if !ok {
                failures += 1;
                eprintln!(
                    "STRESS FAIL iter={iter} session={j} status={:?} stdout={stdout:?} \
                     diag_tail={:?}",
                    out.status,
                    stderr.lines().rev().take(4).collect::<Vec<_>>()
                );
            }
        }
    }
    assert_eq!(
        failures, 0,
        "{failures} concurrent sessions failed across {ITERS} iterations of N={N}"
    );
}

/// Drives one `flat-cyborg` session against the never-settling `ANIMATE_FOREVER`
/// fixture with an OWN in-test kill deadline, returning the child's `Output` plus
/// the observed wall-clock. The deadline is a safety net, not the assertion: a
/// correctly-bounded wait exits FAR sooner. If the deadline is hit the wrapper is
/// killed and the test panics — a regression to an unbounded wait can never hang
/// CI. `extra_args` are inserted before `--cmd` (e.g. the `--hard-timeout-ms`
/// knob); `env` carries the fixture + diagnostics knobs.
fn run_animating_bounded(
    extra_args: &[&str],
    timeout_ms: &str,
    env: &[(&str, &str)],
    deadline: Duration,
) -> (Output, Duration) {
    let mut cmd = Command::new(bin());
    cmd.arg("--extract")
        .arg("--no-jitter")
        .arg("--idle-ms")
        .arg("300")
        .arg("--timeout-ms")
        .arg(timeout_ms)
        .args(extra_args)
        .args(["--cmd", "ping", "--", "sh", &fixture()])
        .env("ANIMATE_FOREVER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let start = Instant::now();
    let mut child = cmd.spawn().expect("spawn flat-cyborg session");
    loop {
        if child.try_wait().expect("try_wait").is_some() {
            break;
        }
        if start.elapsed() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "flat-cyborg did not self-bound within {deadline:?} against a \
                 never-settling animating target — the wait is unbounded (regression)"
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
    let elapsed = start.elapsed();
    let out = child
        .wait_with_output()
        .expect("collect flat-cyborg output");
    (out, elapsed)
}

/// M1 (spike) — classify the hang arm. Against a target that repaints forever and
/// never fences its reply, the `Output::Idle` completion arm can never fire, so
/// the ONLY thing that ends the wait is the graceful watchdog at `--timeout-ms`:
/// the run rides to it and exits `124`. This pins the confirmed arm — completion
/// never fires (`idle=0`), the watchdog is NOT starved (`watchdog_fired=true`) —
/// the evidence the #81 hard-cap fix is built on.
///
/// The hard cap is raised WELL ABOVE `--timeout-ms` here so the graceful watchdog
/// (not the cap, whose CLI default equals `--timeout-ms`) is the arm exercised —
/// this test is about the classification of the pre-cap arm, `animating_target_hits_wall_clock_cap`
/// covers the cap firing.
#[test]
fn animating_target_never_completes_via_grace() {
    let (out, elapsed) = run_animating_bounded(
        &["--hard-timeout-ms", "60000"],
        "1500",
        &[("FLAT_CYBORG_DIAG", "1")],
        Duration::from_secs(20),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(124),
        "a never-settling target must ride to the watchdog (124): status={:?} \
         stderr_tail={:?}",
        out.status,
        stderr.lines().rev().take(6).collect::<Vec<_>>()
    );
    assert!(
        stderr.contains("wrapper.watchdog-interrupt"),
        "the graceful watchdog must fire (arm b is NOT starved): {stderr:?}"
    );
    // The LAST loop-exit is the reply wait (the first is the pre-typing readiness
    // wait, which settles on the banner BEFORE the animation loop even starts).
    let loop_exit = stderr
        .lines()
        .rev()
        .find(|l| l.contains("wrapper.loop-exit"))
        .unwrap_or_else(|| panic!("no wrapper.loop-exit record in: {stderr:?}"));
    assert!(
        loop_exit.contains("idle=0"),
        "completion arm must never fire (arm a confirmed): {loop_exit:?}"
    );
    assert!(
        loop_exit.contains("watchdog_fired=true"),
        "the watchdog must be the arm that bounds the wait: {loop_exit:?}"
    );
    // The watchdog DID bound it, but only after burning the whole --timeout-ms.
    assert!(
        elapsed >= Duration::from_millis(1500),
        "the wait rode to the full --timeout-ms before the watchdog: {elapsed:?}"
    );
}

/// M2 (fix) — the absolute wall-clock cap. With `--hard-timeout-ms` set well
/// BELOW `--timeout-ms`, the same never-settling target is hard-capped at the
/// ceiling and exits with the reserved `69` (EX_UNAVAILABLE), long before the
/// watchdog's `--timeout-ms` would fire. The cap is checked regardless of the
/// `Output` variant, so a target that never yields `Idle`/`Eof` is still bounded.
#[test]
fn animating_target_hits_wall_clock_cap() {
    let (out, elapsed) = run_animating_bounded(
        &["--hard-timeout-ms", "1500"],
        "60000",
        &[("FLAT_CYBORG_DIAG", "1")],
        Duration::from_secs(20),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(69),
        "the hard cap must return the reserved exit code 69: status={:?} \
         stderr_tail={:?}",
        out.status,
        stderr.lines().rev().take(6).collect::<Vec<_>>()
    );
    assert!(
        stderr.contains("wrapper.hardcap"),
        "the hard-cap diagnostic must be present: {stderr:?}"
    );
    // Fired at the cap (~1.5 s), NOT at the 60 s --timeout-ms watchdog.
    assert!(
        elapsed < Duration::from_secs(10),
        "the cap must fire at ~--hard-timeout-ms, well before --timeout-ms: {elapsed:?}"
    );
    assert!(
        !stderr.contains("wrapper.watchdog-interrupt"),
        "the hard cap must pre-empt the graceful watchdog: {stderr:?}"
    );
}
