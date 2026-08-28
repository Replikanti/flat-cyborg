//! Env-gated diagnostic instrumentation for the #71 concurrency triage (M1).
//!
//! This module OBSERVES; it never changes control flow. It exists so the M1
//! reproduction harness can classify *which* failure arm fires when several
//! sessions run concurrently under a slow reply — session spawn/reap, fd
//! pressure, the child-exit (`Output::Eof`) branch, and the sentinel / idle-gate
//! state transitions — without guessing.
//!
//! # Off by default
//!
//! When `FLAT_CYBORG_DIAG` is unset (or empty, or `0`) every [`emit`] call and
//! every [`diag!`](crate::diag) invocation is a no-op: the flag is read exactly
//! once through a [`OnceLock`], no record is formatted, and behaviour is
//! byte-for-byte identical to a build without the taps. When it is set, records
//! are written to **stderr only** (never stdout), so `--extract` capture on
//! stdout stays uncontaminated.
//!
//! Each record is a single line:
//!
//! ```text
//! FCB_DIAG pid=<pid> t=<unix_ms> <category> <fields...>
//! ```
//!
//! so a harness can grep per-arm counts across concurrent processes.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Environment variable that turns the diagnostics on. Enabled when present and
/// not empty / not `0`.
pub const ENV_FLAG: &str = "FLAT_CYBORG_DIAG";

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Whether diagnostic tracing is enabled, read once from [`ENV_FLAG`].
///
/// The value is cached in a [`OnceLock`], so repeated calls on the hot path
/// cost only an atomic load.
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| match std::env::var(ENV_FLAG) {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => false,
    })
}

/// Emits one structured diagnostic record to stderr. No-op when [`enabled`] is
/// false; prefer the [`diag!`](crate::diag) macro, which defers formatting.
pub fn emit(category: &str, fields: std::fmt::Arguments<'_>) {
    if !enabled() {
        return;
    }
    let t_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    eprintln!(
        "FCB_DIAG pid={} t={} {} {}",
        std::process::id(),
        t_ms,
        category,
        fields
    );
}

/// Best-effort count of this process's open file descriptors, or `-1` when it
/// cannot be determined (non-Linux, or `/proc` unavailable). Surfaces fd
/// pressure across concurrent PTY sessions without adding a dependency.
pub fn open_fd_count() -> isize {
    match std::fs::read_dir("/proc/self/fd") {
        Ok(entries) => entries.count() as isize,
        Err(_) => -1,
    }
}

/// Emits a diagnostic record through [`emit`]. The [`enabled`] check is done
/// FIRST, so when diagnostics are off the argument expressions are never
/// evaluated — a disabled build pays nothing beyond the [`enabled`] check.
///
/// This ordering is load-bearing: some taps pass a value-producing call as an
/// argument (e.g. [`open_fd_count`], which scans `/proc/self/fd`, or a screen
/// read). Because `format_args!` evaluates its argument expressions eagerly, a
/// bare `emit($cat, format_args!(...))` would run those scans on every tapped
/// call even with diagnostics off — perturbing the very hot paths (#71) the
/// spike observes. Gating the whole expansion on `enabled()` keeps it inert.
///
/// ```ignore
/// diag!("pty.spawn", "child_pid={pid} fds={}", crate::diag::open_fd_count());
/// ```
#[macro_export]
macro_rules! diag {
    ($category:expr, $($arg:tt)*) => {
        if $crate::diag::enabled() {
            $crate::diag::emit($category, ::std::format_args!($($arg)*));
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_fd_count_is_positive_or_unavailable() {
        // Either a real Linux count (>0) or the -1 sentinel; never 0.
        let n = open_fd_count();
        assert!(n > 0 || n == -1, "unexpected fd count: {n}");
    }

    #[test]
    fn diag_does_not_evaluate_args_when_disabled() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // The lib test binary runs with FLAT_CYBORG_DIAG unset, so diagnostics
        // are off. This pins the load-bearing invariant the #71 spike relies on:
        // a value-producing argument (here a side-effecting counter, standing in
        // for open_fd_count()/idle_gate_open()) is NOT evaluated when off — so
        // the taps add nothing to the hot paths they observe. A bare
        // `emit(_, format_args!(...))` (args eager) would fail this.
        assert!(
            !enabled(),
            "test requires FLAT_CYBORG_DIAG unset (diagnostics disabled)"
        );
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        fn side_effect() -> usize {
            CALLS.fetch_add(1, Ordering::SeqCst)
        }
        crate::diag!("test.gate", "n={}", side_effect());
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            0,
            "diag! evaluated its argument while diagnostics were disabled"
        );
    }
}
