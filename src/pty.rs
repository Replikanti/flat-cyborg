//! PTY & process lifecycle management.
//!
//! [`PtySession`] allocates a native pseudo-terminal (master/slave pair) with
//! a fixed geometry, spawns the Target CLI as a child process attached to the
//! slave end, and multiplexes the master end's I/O without an async runtime.
//! Two dedicated std threads do the blocking work so the caller's thread never
//! blocks on the PTY:
//!
//! - a **reader** thread drains the master into a channel;
//! - a **writer** thread writes queued input to the master.
//!
//! The caller enqueues input with [`PtySession::write_input`] (non-blocking)
//! and polls output with [`PtySession::read_output`] (bounded by a timeout).
//!
//! The child fully inherits the current working directory and environment of
//! the host process; only `TERM` is forced to `xterm-256color` so the target
//! detects a fully-featured terminal and launches in interactive mode.

use crate::error::{Error, Result};
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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

/// Upper bound on how long [`Drop`] waits for the reader thread to finish
/// before detaching it, so teardown can never hang the host thread.
const READER_JOIN_BUDGET: Duration = Duration::from_secs(2);
/// Upper bound on how long [`Drop`] waits for the writer thread.
const WRITER_JOIN_BUDGET: Duration = Duration::from_secs(1);
/// Upper bound on reaping the child after SIGKILL, so a wedged (D-state) child
/// cannot hang teardown indefinitely.
const TERMINATE_REAP_BUDGET: Duration = Duration::from_secs(2);

/// The result of polling the PTY for output.
#[derive(Debug)]
pub enum Output {
    /// A chunk of raw bytes read from the master.
    Data(Vec<u8>),
    /// No new output arrived within the poll timeout (the target may be
    /// running silently or waiting at a prompt).
    Idle,
    /// The master reached end-of-stream: the child closed the slave (it has
    /// exited, or is about to). Any buffered output is delivered as `Data`
    /// before this is reported.
    Eof,
}

/// An interactive Target CLI running inside a pseudo-terminal.
///
/// I/O is multiplexed with std threads rather than an async runtime, so neither
/// reading nor writing ever blocks the caller's thread.
pub struct PtySession {
    /// Sender into the writer thread. Wrapped in `Option` so [`Drop`] can drop
    /// it early, signalling the writer thread to exit.
    input: Option<Sender<Vec<u8>>>,
    output: Receiver<Vec<u8>>,
    child: Child,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    /// Set once the output channel disconnects, so repeated polling after EOF
    /// honors the timeout instead of busy-spinning.
    eof: AtomicBool,
    /// Guards against signalling the process group twice (the second time the
    /// pid may have been recycled).
    terminated: bool,
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
        Self::spawn_in(program, args, None, rows, cols)
    }

    /// Spawns `program` with `args` inside a PTY of the given geometry,
    /// optionally overriding the child's working directory.
    ///
    /// When `cwd` is `Some(dir)`, the child runs in `dir`; when `None`, it
    /// inherits the host's working directory (as [`spawn`](Self::spawn) and
    /// [`spawn_with_size`](Self::spawn_with_size) do). The child inherits the
    /// host's environment regardless; `TERM` is set to [`TERM`].
    ///
    /// # Errors
    /// Returns an error if the PTY cannot be allocated or resized, or if the
    /// child process fails to spawn.
    pub fn spawn_in<S, I, A>(
        program: S,
        args: I,
        cwd: Option<&Path>,
        rows: u16,
        cols: u16,
    ) -> Result<Self>
    where
        S: AsRef<OsStr>,
        I: IntoIterator<Item = A>,
        A: AsRef<OsStr>,
    {
        let (pty, pts) = pty_process::blocking::open()?;
        pty.resize(Size::new(rows, cols))?;

        // `pty_process::blocking::Command` wraps `std::process::Command`, which
        // inherits the parent CWD and environment by default. We only set TERM,
        // and override the working directory when `cwd` is given. Its builder
        // methods consume and return `self`, so they are chained. The child is
        // made a session leader (it gets the slave as controlling terminal), so
        // its process-group id equals its pid — see `Drop`.
        let mut command = pty_process::blocking::Command::new(program);
        command = command.args(args).env("TERM", TERM);
        if let Some(dir) = cwd {
            command = command.current_dir(dir);
        }
        let child = command.spawn(pts)?;

        // `Read`/`Write` are implemented for `&Pty`, so the two threads can
        // share the master via `Arc` without any interior mutability. The PTY
        // is full-duplex, so concurrent read and write on the fd are safe.
        let pty = Arc::new(pty);

        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
        let reader_pty = Arc::clone(&pty);
        let reader = thread::spawn(move || {
            let mut handle: &Pty = &reader_pty;
            let mut buf = [0u8; READ_CHUNK];
            loop {
                match handle.read(&mut buf) {
                    Ok(0) => break, // clean EOF
                    Ok(n) => {
                        if out_tx.send(buf[..n].to_vec()).is_err() {
                            break; // receiver dropped
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    // On Linux, reading the master after the slave closes
                    // surfaces as EIO, which is the expected end-of-stream
                    // signal here. Any other error is also terminal for the
                    // stream, so we stop reading in every error case.
                    Err(_) => break,
                }
            }
        });

        let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>();
        let writer_pty = Arc::clone(&pty);
        let writer = thread::spawn(move || {
            let mut handle: &Pty = &writer_pty;
            // Exits when the input channel is dropped (Disconnected) or a write
            // fails (the child closed the slave).
            while let Ok(bytes) = in_rx.recv() {
                if handle.write_all(&bytes).is_err() {
                    break;
                }
                let _ = handle.flush();
            }
        });

        Ok(Self {
            input: Some(in_tx),
            output: out_rx,
            child,
            reader: Some(reader),
            writer: Some(writer),
            eof: AtomicBool::new(false),
            terminated: false,
        })
    }

    /// Queues raw bytes to be written to the PTY master (the child's stdin).
    ///
    /// This only enqueues; the dedicated writer thread performs the blocking
    /// write, so the caller never blocks even if the child has stopped reading.
    ///
    /// # Errors
    /// Returns an error if the writer thread has terminated.
    pub fn write_input(&self, bytes: &[u8]) -> Result<()> {
        let tx = self.input.as_ref().ok_or_else(|| {
            Error::Io(io::Error::new(io::ErrorKind::BrokenPipe, "session closed"))
        })?;
        tx.send(bytes.to_vec()).map_err(|_| {
            Error::Io(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "pty writer thread terminated",
            ))
        })
    }

    /// Waits up to `timeout` for the next chunk of output from the master.
    ///
    /// Returns [`Output::Data`] if bytes arrived, [`Output::Idle`] if the
    /// timeout elapsed with no output, or [`Output::Eof`] once the child has
    /// closed the slave end. After the first `Eof`, subsequent calls still
    /// honor `timeout` (they sleep rather than returning instantly), so a poll
    /// loop that keeps calling after EOF will not busy-spin.
    pub fn read_output(&self, timeout: Duration) -> Output {
        if self.eof.load(Ordering::Acquire) {
            thread::sleep(timeout);
            return Output::Eof;
        }
        match self.output.recv_timeout(timeout) {
            Ok(chunk) => Output::Data(chunk),
            Err(RecvTimeoutError::Timeout) => Output::Idle,
            Err(RecvTimeoutError::Disconnected) => {
                self.eof.store(true, Ordering::Release);
                Output::Eof
            }
        }
    }

    /// Returns a cloneable handle for writing input from another thread (e.g.
    /// an interactive front-end forwarding the host's keystrokes), or `None`
    /// if the session is already closing.
    pub fn input_handle(&self) -> Option<InputHandle> {
        self.input.as_ref().map(|tx| InputHandle(tx.clone()))
    }

    /// The OS process id of the child.
    pub fn child_id(&self) -> u32 {
        self.child.id()
    }

    /// Waits up to `budget` for the child to exit and returns its status.
    ///
    /// On success the child has been reaped, so this marks the session
    /// terminated — [`Drop`] will then skip signalling (the pid could be
    /// recycled). Returns `None` if the child has not exited within `budget`.
    pub fn wait_with_timeout(&mut self, budget: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + budget;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.terminated = true;
                    return Some(status);
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return None,
            }
        }
    }

    /// Mutable access to the child process handle (for signalling / waiting).
    pub fn child(&mut self) -> &mut Child {
        &mut self.child
    }

    /// SIGKILLs the child's entire process group and reaps the child.
    ///
    /// Used by the watchdog as the last-resort step after a graceful interrupt,
    /// and by [`Drop`]. Idempotent: signalling happens at most once, since the
    /// pid could be recycled after the child is reaped.
    pub fn terminate(&mut self) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        // Kill the whole group *before* reaping, so any grandchild holding the
        // slave fd dies and the reader's blocking read unblocks with EOF/EIO.
        // (`child.id()` is only valid before `wait`.)
        kill_process_group(self.child.id());
        let _ = self.child.kill();
        // Reap, but bounded: a child wedged in uninterruptible (D) state will
        // not die even on SIGKILL until its I/O completes, and a blind
        // `wait()` would hang teardown. Poll `try_wait` and give up after a
        // short budget — there is nothing more we can do for a D-state child.
        let deadline = Instant::now() + TERMINATE_REAP_BUDGET;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
}

/// A cloneable, `Send` handle for writing input to a [`PtySession`]'s master
/// from another thread. Writes are enqueued (non-blocking); the session's
/// writer thread performs the actual write.
#[derive(Clone)]
pub struct InputHandle(Sender<Vec<u8>>);

impl InputHandle {
    /// Queues `bytes` to be written to the PTY master.
    ///
    /// # Errors
    /// Returns an error if the session (and its writer thread) has shut down.
    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        self.0.send(bytes.to_vec()).map_err(|_| {
            Error::Io(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "pty writer thread terminated",
            ))
        })
    }
}

/// SIGKILLs the entire process group led by `pid`, so sub-processes spawned by
/// the Target CLI (which share its process group via the controlling terminal)
/// are terminated too — not just the direct child.
fn kill_process_group(pid: u32) {
    if let Some(p) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(p, rustix::process::Signal::KILL);
    }
}

/// Joins `handle`, but gives up after `budget` and detaches the thread instead
/// of blocking forever. Teardown correctness never depends on a thread that
/// refuses to exit.
fn join_bounded(handle: JoinHandle<()>, budget: Duration) {
    let start = Instant::now();
    while !handle.is_finished() {
        if start.elapsed() >= budget {
            return; // detach rather than hang
        }
        thread::sleep(Duration::from_millis(5));
    }
    let _ = handle.join();
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // 1. Drop the input channel so the writer thread's `recv` returns and
        //    it exits.
        self.input.take();

        // 2. Kill the whole process group and reap (idempotent).
        self.terminate();

        // 3. Bounded joins: teardown can never hang the host thread.
        if let Some(reader) = self.reader.take() {
            join_bounded(reader, READER_JOIN_BUDGET);
        }
        if let Some(writer) = self.writer.take() {
            join_bounded(writer, WRITER_JOIN_BUDGET);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn spawn_in_runs_target_in_the_given_cwd() {
        // With an explicit cwd, the child sees that directory rather than the
        // host's. `/tmp` is dash-safe and present on every Unix test host.
        let session = PtySession::spawn_in(
            "sh",
            ["-c", "pwd"],
            Some(Path::new("/tmp")),
            DEFAULT_ROWS,
            DEFAULT_COLS,
        )
        .expect("spawn in /tmp");
        let out = read_until(&session, "/tmp", Duration::from_secs(5));
        assert!(out.contains("/tmp"), "target cwd not honored: {out:?}");
    }

    #[test]
    fn input_written_to_master_reaches_target() {
        // `cat` echoes its stdin back; prove the write path drives the child.
        let session = PtySession::spawn("cat", std::iter::empty::<&str>()).expect("spawn cat");
        session.write_input(b"ping\r").expect("queue input");
        let out = read_until(&session, "ping", Duration::from_secs(5));
        assert!(
            out.contains("ping"),
            "did not observe echoed input: {out:?}"
        );
        // `session` is dropped here, which kills `cat` and joins both threads.
    }

    #[test]
    fn reports_eof_after_child_exits() {
        let session = PtySession::spawn("sh", ["-c", "echo bye"]).expect("spawn");
        let start = Instant::now();
        let mut saw_eof = false;
        while start.elapsed() < Duration::from_secs(5) {
            if let Output::Eof = session.read_output(Duration::from_millis(200)) {
                saw_eof = true;
                break;
            }
        }
        assert!(saw_eof, "expected EOF after the child exited");
    }

    #[test]
    fn polling_after_eof_honors_timeout() {
        // Regression: after EOF the channel is disconnected; a naive
        // `recv_timeout` returns instantly and a poll loop busy-spins. The
        // sticky-EOF path must sleep for the requested timeout instead.
        let session = PtySession::spawn("sh", ["-c", "echo bye"]).expect("spawn");
        // Reach EOF first.
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if let Output::Eof = session.read_output(Duration::from_millis(100)) {
                break;
            }
        }
        // A subsequent poll must take roughly the timeout, not return instantly.
        let t = Instant::now();
        assert!(matches!(
            session.read_output(Duration::from_millis(300)),
            Output::Eof
        ));
        assert!(
            t.elapsed() >= Duration::from_millis(250),
            "post-EOF poll returned too fast ({:?}); it is busy-spinning",
            t.elapsed()
        );
    }

    #[test]
    fn drop_does_not_hang_with_surviving_grandchild() {
        // Regression for the QA blocker: the shell backgrounds a long sleep
        // (a grandchild sharing the process group) and waits on it. Dropping
        // the session must SIGKILL the whole group and return promptly, not
        // hang on the reader thread because the grandchild keeps the slave open.
        let session =
            PtySession::spawn("sh", ["-c", "sleep 300 & echo STARTED; wait"]).expect("spawn");
        let out = read_until(&session, "STARTED", Duration::from_secs(5));
        assert!(out.contains("STARTED"), "target did not start: {out:?}");

        let start = Instant::now();
        drop(session);
        assert!(
            start.elapsed() < READER_JOIN_BUDGET + Duration::from_secs(1),
            "Drop hung for {:?}",
            start.elapsed()
        );
    }
}
