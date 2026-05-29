//! PTY & process lifecycle management.
//!
//! [`PtySession`] allocates a native pseudo-terminal (master/slave pair) with
//! a fixed geometry, spawns the Target CLI as a child process attached to the
//! slave end, and multiplexes the master end's I/O without an async runtime: a
//! dedicated reader thread drains the master into a channel, while the caller
//! writes to the master directly.
//!
//! The child fully inherits the current working directory and environment of
//! the host process; only `TERM` is forced to `xterm-256color` so the target
//! detects a fully-featured terminal and launches in interactive mode.

use crate::error::Result;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::process::Child;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use pty_process::blocking::Pty;
use pty_process::Size;

/// Recommended terminal width, in character columns.
pub const DEFAULT_COLS: u16 = 120;
/// Recommended terminal height, in character rows.
pub const DEFAULT_ROWS: u16 = 40;
/// Terminal type advertised to the Target CLI.
pub const TERM: &str = "xterm-256color";

/// Size of the reader thread's read buffer, in bytes.
const READ_CHUNK: usize = 8192;

/// The result of polling the PTY for output.
#[derive(Debug)]
pub enum Output {
    /// A chunk of raw bytes read from the master.
    Data(Vec<u8>),
    /// No new output arrived within the poll timeout (the target may be
    /// running silently or waiting at a prompt).
    Idle,
    /// The master reached end-of-stream: the child closed the slave (it has
    /// exited or is about to).
    Eof,
}

/// An interactive Target CLI running inside a pseudo-terminal.
///
/// I/O is multiplexed with std threads rather than an async runtime. A reader
/// thread continuously reads the PTY master and forwards chunks over a channel;
/// the caller writes input to the master directly via [`PtySession::write_input`]
/// and polls output via [`PtySession::read_output`].
pub struct PtySession {
    /// The master end, shared between the writer (this handle) and the reader
    /// thread. The PTY is full-duplex, so concurrent read/write on the shared
    /// file descriptor is safe.
    pty: Arc<Pty>,
    child: Child,
    output: Receiver<Vec<u8>>,
    reader: Option<JoinHandle<()>>,
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
        let (pty, pts) = pty_process::blocking::open()?;
        pty.resize(Size::new(rows, cols))?;

        // `pty_process::blocking::Command` wraps `std::process::Command`, which
        // inherits the parent CWD and environment by default. We only set TERM.
        let child = pty_process::blocking::Command::new(program)
            .args(args)
            .env("TERM", TERM)
            .spawn(pts)?;

        let pty = Arc::new(pty);
        let (tx, rx) = mpsc::channel::<Vec<u8>>();

        let reader_pty = Arc::clone(&pty);
        let reader = std::thread::spawn(move || {
            // `Read` is implemented for `&Pty`, so the shared handle is read
            // without any interior mutability.
            let mut handle: &Pty = &reader_pty;
            let mut buf = [0u8; READ_CHUNK];
            loop {
                match handle.read(&mut buf) {
                    Ok(0) => break, // clean EOF
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break; // receiver dropped
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    // On Linux, reading the master after the slave closes
                    // surfaces as EIO rather than a clean EOF. Treat any other
                    // error as end-of-stream.
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            pty,
            child,
            output: rx,
            reader: Some(reader),
        })
    }

    /// Writes raw bytes to the PTY master (the child's stdin) and flushes.
    ///
    /// # Errors
    /// Returns an error if the write or flush fails.
    pub fn write_input(&self, bytes: &[u8]) -> Result<()> {
        let mut handle: &Pty = &self.pty;
        handle.write_all(bytes)?;
        handle.flush()?;
        Ok(())
    }

    /// Waits up to `timeout` for the next chunk of output from the master.
    ///
    /// Returns [`Output::Data`] if bytes arrived, [`Output::Idle`] if the
    /// timeout elapsed with no output, or [`Output::Eof`] once the child has
    /// closed the slave end.
    pub fn read_output(&self, timeout: Duration) -> Output {
        match self.output.recv_timeout(timeout) {
            Ok(chunk) => Output::Data(chunk),
            Err(RecvTimeoutError::Timeout) => Output::Idle,
            Err(RecvTimeoutError::Disconnected) => Output::Eof,
        }
    }

    /// The OS process id of the child.
    pub fn child_id(&self) -> u32 {
        self.child.id()
    }

    /// Mutable access to the child process handle (for signalling / waiting).
    pub fn child(&mut self) -> &mut Child {
        &mut self.child
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Guarantee the child does not outlive the session. Killing it closes
        // the slave, which unblocks the reader thread (EIO/EOF) so it can be
        // joined without hanging.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Drains output until `needle` is seen or `deadline` passes.
    fn read_until(session: &PtySession, needle: &str, deadline: Duration) -> String {
        let start = Instant::now();
        let mut acc = String::new();
        while start.elapsed() < deadline {
            match session.read_output(Duration::from_millis(200)) {
                Output::Data(chunk) => {
                    acc.push_str(&String::from_utf8_lossy(&chunk));
                    if acc.contains(needle) {
                        break;
                    }
                }
                Output::Idle => continue,
                Output::Eof => break,
            }
        }
        acc
    }

    #[test]
    fn target_detects_a_tty() {
        // The Target CLI sees a real terminal on its stdout, so `[ -t 1 ]`
        // succeeds and it launches in "interactive" mode rather than headless.
        let session = PtySession::spawn(
            "sh",
            ["-c", "if [ -t 1 ]; then echo HAS_TTY; else echo NO_TTY; fi"],
        )
        .expect("spawn target in pty");

        let out = read_until(&session, "HAS_TTY", Duration::from_secs(5));
        assert!(
            out.contains("HAS_TTY"),
            "target did not detect a tty: {out:?}"
        );
        assert!(!out.contains("NO_TTY"));
    }

    #[test]
    fn spawn_reports_child_id() {
        let session = PtySession::spawn("sh", ["-c", "sleep 1"]).expect("spawn");
        assert!(session.child_id() > 0);
    }

    #[test]
    fn input_written_to_master_reaches_target() {
        // `cat` echoes its stdin back; prove the write path drives the child.
        let session = PtySession::spawn("cat", std::iter::empty::<&str>()).expect("spawn cat");
        session.write_input(b"ping\r").expect("write to master");
        let out = read_until(&session, "ping", Duration::from_secs(5));
        assert!(
            out.contains("ping"),
            "did not observe echoed input: {out:?}"
        );
        // `session` is dropped here, which kills `cat` and joins the reader.
    }

    #[test]
    fn reports_eof_after_child_exits() {
        let session = PtySession::spawn("sh", ["-c", "echo bye"]).expect("spawn");
        let mut saw_eof = false;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            match session.read_output(Duration::from_millis(200)) {
                Output::Eof => {
                    saw_eof = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(saw_eof, "expected EOF after the child exited");
    }
}
