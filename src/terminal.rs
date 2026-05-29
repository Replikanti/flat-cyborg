//! Host terminal raw-mode management with guaranteed restoration.
//!
//! When the wrapper is driven interactively it puts the host terminal into raw
//! mode so keystrokes reach the Target CLI verbatim. [`RawModeGuard`] performs
//! that switch and, crucially, restores the terminal to its original (canonical)
//! settings when it is dropped — including during stack unwinding from a panic.
//!
//! The guard owns a duplicate of the terminal file descriptor, so restoration
//! does not depend on the original handle still being around, and dropping the
//! guard never closes the caller's `stdin`.

use std::io;
use std::os::fd::{AsFd, OwnedFd};

use rustix::termios::{isatty, tcgetattr, tcsetattr, OptionalActions, Termios};

/// Restores a terminal's mode on drop.
///
/// Created via [`RawModeGuard::stdin`] (or [`RawModeGuard::new`] for an
/// arbitrary terminal fd). If the target fd is not a TTY, construction returns
/// `Ok(None)` and nothing is changed — so the same code path works when output
/// is piped (e.g. under tests or in a non-interactive pipeline).
#[derive(Debug)]
pub struct RawModeGuard {
    fd: OwnedFd,
    original: Termios,
}

impl RawModeGuard {
    /// Puts `fd`'s terminal into raw mode, returning a guard that restores the
    /// previous settings on drop.
    ///
    /// Returns `Ok(None)` if `fd` is not a terminal.
    ///
    /// # Errors
    /// Returns an error if the terminal attributes cannot be read or set, or if
    /// the descriptor cannot be duplicated.
    pub fn new<Fd: AsFd>(fd: Fd) -> io::Result<Option<Self>> {
        let fd = fd.as_fd();
        if !isatty(fd) {
            return Ok(None);
        }
        let original = tcgetattr(fd)?;
        let mut raw = original.clone();
        raw.make_raw();
        tcsetattr(fd, OptionalActions::Now, &raw)?;
        // Own a duplicate so restoration does not depend on the borrowed fd's
        // lifetime, and dropping the guard does not close the caller's fd.
        let owned = fd.try_clone_to_owned()?;
        Ok(Some(Self {
            fd: owned,
            original,
        }))
    }

    /// Puts the process's standard input terminal into raw mode.
    ///
    /// Returns `Ok(None)` if stdin is not a terminal.
    ///
    /// # Errors
    /// See [`RawModeGuard::new`].
    pub fn stdin() -> io::Result<Option<Self>> {
        Self::new(rustix::stdio::stdin())
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // Best-effort restore; there is nothing useful to do on failure during
        // teardown. This runs on normal scope exit and during panic unwinding.
        let _ = tcsetattr(&self.fd, OptionalActions::Now, &self.original);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::termios::LocalModes;

    #[test]
    fn non_tty_yields_no_guard() {
        // A regular file is not a terminal: construction is a no-op.
        let file = std::fs::File::open("/dev/null").expect("open /dev/null");
        let guard = RawModeGuard::new(&file).expect("new");
        assert!(guard.is_none());
    }

    #[test]
    fn raw_mode_is_entered_and_restored() {
        // A PTY slave is a real terminal we can toggle without touching the
        // host's stdin.
        let (_pty, pts) = pty_process::blocking::open().expect("open pty");

        let before = tcgetattr(&pts).expect("tcgetattr before");
        assert!(
            before.local_modes.contains(LocalModes::ICANON),
            "fresh pts should start in canonical mode"
        );

        {
            let guard = RawModeGuard::new(&pts).expect("new").expect("is a tty");
            let during = tcgetattr(&pts).expect("tcgetattr during");
            assert!(
                !during.local_modes.contains(LocalModes::ICANON),
                "raw mode should clear ICANON"
            );
            drop(guard);
        }

        let after = tcgetattr(&pts).expect("tcgetattr after");
        assert!(
            after.local_modes.contains(LocalModes::ICANON),
            "dropping the guard should restore canonical mode"
        );
    }
}
