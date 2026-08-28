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

/// Runs one `flat-cyborg --extract-structural` session against the fixture with
/// a slow reply. `env` are extra child env knobs (fixture behaviour + diag).
fn run_session(reply_delay_ms: u32, env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(bin());
    cmd.args([
        "--extract-structural",
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

/// A target that dies mid-reply (before the closing sentinel) is CLASSIFIED,
/// not mistaken for a flat-cyborg fault: the diagnostics show it completed via
/// EOF while un-interrupted, and flat-cyborg faithfully propagates the target's
/// non-zero exit (it did not self-abort). This is the prime #71 suspect arm,
/// reproduced deterministically without needing high concurrency.
#[test]
fn target_death_midway_is_classified_via_eof() {
    let out = run_session(200, &[("DIE_MIDWAY", "1"), ("FLAT_CYBORG_DIAG", "1")]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // flat-cyborg propagated the target's exit(1); it did not crash/abort.
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected propagated target exit 1: status={:?} stderr={stderr:?}",
        out.status
    );
    assert!(
        stderr.contains("wrapper.eof") && stderr.contains("interrupted=false"),
        "expected the un-interrupted EOF classification: {stderr:?}"
    );
    assert!(
        stderr.contains("outcome=Completed"),
        "expected the target-vanished completion classification: {stderr:?}"
    );
    // No fenced reply arrived, so nothing chrome-like is printed to stdout.
    assert!(
        !stdout.contains('█') && !stdout.contains("FCB_"),
        "capture leaked chrome/sentinel on target death: {stdout:?}"
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
