//! PTY & process lifecycle management.
//!
//! [`PtySession`] allocates a native pseudo-terminal (master/slave pair) with
//! a fixed geometry, spawns the Target CLI as a child process attached to the
//! slave end, and exposes the master end as asynchronous read/write halves so
//! the wrapper can multiplex I/O concurrently.
//!
//! The child fully inherits the current working directory and environment of
//! the host process; only `TERM` is forced to `xterm-256color` so the target
//! detects a fully-featured terminal and launches in interactive mode.

use crate::error::Result;
use std::ffi::OsStr;

use pty_process::{OwnedReadPty, OwnedWritePty, Size};
use tokio::process::Child;

/// Recommended terminal width, in character columns.
pub const DEFAULT_COLS: u16 = 120;
/// Recommended terminal height, in character rows.
pub const DEFAULT_ROWS: u16 = 40;
/// Terminal type advertised to the Target CLI.
pub const TERM: &str = "xterm-256color";

/// An interactive Target CLI running inside a pseudo-terminal.
///
/// The master end is split into independently-movable read and write halves
/// so a reader task and a writer task can operate on the PTY concurrently
/// without contending on a single handle.
pub struct PtySession {
    reader: OwnedReadPty,
    writer: OwnedWritePty,
    child: Child,
}

impl PtySession {
    /// Spawns `program` with `args` inside a PTY of the recommended geometry
    /// ([`DEFAULT_ROWS`] x [`DEFAULT_COLS`]).
    ///
    /// # Errors
    /// Returns an error if the PTY cannot be allocated or resized, or if the
    /// child process fails to spawn.
    pub fn spawn<S, I, A>(program: S, args: I) -> Result<Self>
    where
        S: AsRef<OsStr>,
        I: IntoIterator<Item = A>,
        A: AsRef<OsStr>,
    {
        Self::spawn_with_size(program, args, DEFAULT_ROWS, DEFAULT_COLS)
    }

    /// Spawns `program` with `args` inside a PTY of the given geometry.
    ///
    /// The child inherits the host's working directory and environment; `TERM`
    /// is set to [`TERM`] regardless of the inherited value.
    ///
    /// # Errors
    /// Returns an error if the PTY cannot be allocated or resized, or if the
    /// child process fails to spawn.
    pub fn spawn_with_size<S, I, A>(program: S, args: I, rows: u16, cols: u16) -> Result<Self>
    where
        S: AsRef<OsStr>,
        I: IntoIterator<Item = A>,
        A: AsRef<OsStr>,
    {
        let (pty, pts) = pty_process::open()?;
        pty.resize(Size::new(rows, cols))?;

        // `pty_process::Command` wraps `tokio::process::Command`, which inherits
        // the parent CWD and environment by default. We only override `TERM`.
        let child = pty_process::Command::new(program)
            .args(args)
            .env("TERM", TERM)
            .spawn(pts)?;

        let (reader, writer) = pty.into_split();
        Ok(Self {
            reader,
            writer,
            child,
        })
    }

    /// Mutable access to the read half of the PTY master.
    pub fn reader(&mut self) -> &mut OwnedReadPty {
        &mut self.reader
    }

    /// Mutable access to the write half of the PTY master.
    pub fn writer(&mut self) -> &mut OwnedWritePty {
        &mut self.writer
    }

    /// The child process handle.
    pub fn child(&mut self) -> &mut Child {
        &mut self.child
    }

    /// The OS process id of the child, or `None` if it has already exited and
    /// been reaped.
    pub fn child_id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Consumes the session, returning its component handles so a caller can
    /// move the read half, write half, and child into independent tasks.
    pub fn into_parts(self) -> (OwnedReadPty, OwnedWritePty, Child) {
        (self.reader, self.writer, self.child)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::{timeout, Duration};

    /// Reads from the PTY master until `needle` is seen or the stream ends.
    ///
    /// On Linux, reading the master after the slave closes surfaces as an
    /// `EIO` error rather than a clean EOF; both are treated as end-of-stream.
    async fn read_until(reader: &mut OwnedReadPty, needle: &str) -> String {
        let mut acc = String::new();
        let mut buf = [0u8; 1024];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    acc.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if acc.contains(needle) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        acc
    }

    #[tokio::test]
    async fn target_detects_a_tty() {
        // The Target CLI sees a real terminal on its stdout, so `[ -t 1 ]`
        // succeeds and it launches in "interactive" mode rather than headless.
        let mut session = PtySession::spawn(
            "sh",
            ["-c", "if [ -t 1 ]; then echo HAS_TTY; else echo NO_TTY; fi"],
        )
        .expect("spawn target in pty");

        let out = timeout(
            Duration::from_secs(5),
            read_until(session.reader(), "HAS_TTY"),
        )
        .await
        .expect("read did not time out");

        assert!(
            out.contains("HAS_TTY"),
            "target did not detect a tty: {out:?}"
        );
        assert!(!out.contains("NO_TTY"));
    }

    #[tokio::test]
    async fn spawn_reports_child_id() {
        let session = PtySession::spawn("sh", ["-c", "sleep 1"]).expect("spawn");
        assert!(session.child_id().is_some());
    }

    #[tokio::test]
    async fn input_written_to_master_reaches_target() {
        // `cat` echoes its stdin back; prove the write half drives the child.
        let mut session = PtySession::spawn("cat", std::iter::empty::<&str>()).expect("spawn cat");
        session
            .writer()
            .write_all(b"ping\r")
            .await
            .expect("write to master");
        let out = timeout(Duration::from_secs(5), read_until(session.reader(), "ping"))
            .await
            .expect("read did not time out");
        assert!(
            out.contains("ping"),
            "did not observe echoed input: {out:?}"
        );

        // Close the child so the test process does not linger.
        let _ = session.child().start_kill();
    }
}
