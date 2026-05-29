//! flat-cyborg — an asynchronous pseudo-terminal (PTY) wrapper for
//! bidirectional I/O interception of interactive CLI applications.
//!
//! The crate encapsulates an interactive "Target CLI" inside a virtual PTY,
//! fully emulating TTY behavior so the target launches in interactive mode.
//! It performs bidirectional stream interception, simulates human-like input
//! timing, and deterministically detects the target's lifecycle state by
//! parsing the output ANSI stream.
//!
//! The functional components are introduced incrementally:
//!
//! - PTY & process lifecycle management
//! - input jittering (human input emulation)
//! - output ANSI state machine & stream stripping
//! - wrapper orchestration & safety watchdogs
//!
//! This is the initial project skeleton; the components above land in
//! subsequent changes.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod ansi;
pub mod error;
pub mod jitter;
pub mod pty;
pub mod screen;
pub mod terminal;
pub mod wrapper;

pub use error::{Error, Result};
pub use jitter::Jitter;
pub use pty::PtySession;
pub use screen::Screen;
pub use terminal::RawModeGuard;
pub use wrapper::{Outcome, State, Wrapper, WrapperConfig};

/// Crate version string, sourced from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_populated() {
        assert!(!VERSION.is_empty());
    }
}
