//! Wrapper orchestration and safety watchdogs.
//!
//! [`Wrapper`] ties the building blocks together: it types a command into the
//! [`PtySession`](crate::pty::PtySession) through the input
//! [`Jitter`](crate::jitter::Jitter), drains the master through the
//! [`Sanitizer`](crate::ansi::Sanitizer), and runs a lifecycle state machine
//! over the sanitized stream to decide when the Target CLI has finished:
//!
//! - **RUNNING** — output is actively appending.
//! - **CONFIRMATION_PROMPT** — a `[y/n]`-style prompt; the wrapper auto-answers
//!   `y\r` through the jitter (when `auto_confirm` is set).
//! - **IDLE (completed)** — the trailing prompt is present and no new output
//!   has arrived for at least `idle_silence`.
//!
//! A watchdog bounds every operation: if IDLE is not reached within
//! `exec_timeout`, the wrapper writes `Ctrl+C` (`\x03`) and, if the target does
//! not exit within `interrupt_grace`, SIGKILLs its process group.
//!
//! Note on raw-mode cleanup: this wrapper never alters the *host* terminal's
//! mode (commands arrive as strings from the orchestrator), so there is no host
//! raw mode to restore. The interactive demo front-end, which does put the host
//! TTY in raw mode, owns that cleanup via an RAII guard.

use std::thread;
use std::time::{Duration, Instant};

use crate::ansi::{is_approval_menu, is_confirmation_prompt, line_ends_with_any, Sanitizer};
use crate::error::Result;
use crate::jitter::Jitter;
use crate::pty::{Output, PtySession, DEFAULT_COLS, DEFAULT_ROWS};
use crate::screen::Screen;

/// Tunables for the wrapper's state machine and watchdog.
#[derive(Debug, Clone)]
pub struct WrapperConfig {
    /// Minimum silence after the trailing prompt appears before declaring IDLE.
    pub idle_silence: Duration,
    /// Maximum time to reach IDLE before the watchdog intervenes (`T_max`).
    pub exec_timeout: Duration,
    /// Grace period after `Ctrl+C` before escalating to SIGKILL.
    pub interrupt_grace: Duration,
    /// Granularity of the output poll loop.
    pub poll_interval: Duration,
    /// Trailing prompt tokens that, combined with silence, signal IDLE.
    pub prompt_tokens: Vec<String>,
    /// Whether to auto-answer confirmation prompts with `y\r`.
    pub auto_confirm: bool,
    /// Whether to auto-confirm agentic-CLI **approval / trust menus** (the
    /// arrow-key numbered menus a `[y/n]` reply cannot answer, e.g. codex's
    /// `git push` confirmation or claude's "trust this folder" prompt) by
    /// pressing Enter on the default "yes/proceed/trust" option.
    ///
    /// Off by default: confirming such a menu bypasses the agent's own safety
    /// gate (including for destructive actions), so it is strictly opt-in.
    pub auto_approve: bool,
    /// Full-screen TUI mode: capture output through a 2D screen grid and treat
    /// a settled screen (quiet for `idle_silence`) as IDLE, rather than looking
    /// for a line-oriented trailing prompt.
    pub tui: bool,
    /// Single-burst input (the `--no-jitter` flag): instead of one jittered
    /// keystroke per character (40-300 ms each — minutes for a
    /// multi-thousand-char prompt), write the command body in fast fixed-size
    /// chunks, let the screen settle, then send the `\r` submit as a *separate*
    /// write. Two things make this work against an Ink-style TUI (e.g. claude):
    /// chunking the body defeats the editor's "many chars at once = collapse to
    /// a [Pasted text] placeholder" heuristic, and the settled, separate Enter
    /// is registered as a real submit rather than being swallowed as part of a
    /// paste. Off by default (human-cadence jitter is the default).
    pub burst_input: bool,
}

impl Default for WrapperConfig {
    fn default() -> Self {
        Self {
            idle_silence: Duration::from_millis(500),
            exec_timeout: Duration::from_secs(60),
            interrupt_grace: Duration::from_secs(5),
            poll_interval: Duration::from_millis(100),
            prompt_tokens: ["$ ", "# ", "> ", "% "]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            auto_confirm: true,
            auto_approve: false,
            tui: false,
            burst_input: false,
        }
    }
}

/// The Target CLI's lifecycle state, as classified from the sanitized stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Output is actively appending.
    Running,
    /// A `[y/n]`-style prompt awaiting user interaction.
    ConfirmationPrompt,
    /// The trailing prompt is present and output has gone silent.
    Idle,
}

/// How an operation finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The trailing prompt appeared and output went silent: the target is ready
    /// for the next command.
    Idle,
    /// The child closed the PTY (it exited).
    Completed,
    /// The watchdog aborted the operation after `exec_timeout`.
    TimedOut,
}

/// Orchestrates an interactive Target CLI inside a PTY.
pub struct Wrapper {
    session: PtySession,
    sanitizer: Sanitizer,
    /// 2D screen grid, allocated only in TUI mode.
    screen: Option<Screen>,
    jitter: Jitter,
    config: WrapperConfig,
    state: State,
}

impl Wrapper {
    /// Wraps `session` with the default configuration and a clock-seeded jitter.
    pub fn new(session: PtySession) -> Self {
        Self::with_config(session, WrapperConfig::default())
    }

    /// Wraps `session` with an explicit configuration.
    pub fn with_config(session: PtySession, config: WrapperConfig) -> Self {
        // The grid matches the session's default PTY geometry; only needed in
        // TUI mode.
        let screen = config.tui.then(|| Screen::new(DEFAULT_ROWS, DEFAULT_COLS));
        Self {
            session,
            sanitizer: Sanitizer::new(),
            screen,
            jitter: Jitter::new(),
            config,
            state: State::Running,
        }
    }

    /// Replaces the input jitter (e.g. with a zero-delay one in tests).
    pub fn set_jitter(&mut self, jitter: Jitter) {
        self.jitter = jitter;
    }

    /// The most recently classified lifecycle [`State`] of the target.
    pub fn state(&self) -> State {
        self.state
    }

    /// The sanitized output log accumulated so far (ANSI-stripped, spinner-free).
    pub fn clean_log(&self) -> String {
        self.sanitizer.clean_log()
    }

    /// The current visible screen rendered as text. Meaningful in `--tui` mode,
    /// where output is captured through the 2D screen grid; empty otherwise.
    pub fn screen_text(&self) -> String {
        self.screen.as_ref().map(Screen::text).unwrap_or_default()
    }

    /// The full transcript including lines that scrolled off the top of the
    /// viewport. Meaningful in `--tui` mode; empty otherwise. Used by
    /// `--extract` to capture long multi-line replies.
    pub fn screen_full_text(&self) -> String {
        self.screen
            .as_ref()
            .map(Screen::full_text)
            .unwrap_or_default()
    }

    /// Mutable access to the underlying session.
    pub fn session(&mut self) -> &mut PtySession {
        &mut self.session
    }

    /// Types `command` (jittered) and then waits for the target to return to
    /// IDLE, completing, or the watchdog to fire.
    ///
    /// # Errors
    /// Returns an error if writing the command to the master fails.
    pub fn run_command(&mut self, command: &str) -> Result<Outcome> {
        if self.config.tui {
            // Let the TUI finish its current render and become ready for input
            // before typing, so keystrokes are not dropped during a redraw.
            match self.wait_until_idle()? {
                Outcome::Idle => {}
                other => return Ok(other),
            }
        }
        self.send(command)?;
        self.wait_until_idle()
    }

    /// Types `command` into the target, terminated by a carriage return.
    ///
    /// Uses human-like per-keystroke jitter by default, or a fast single-burst
    /// path when [`WrapperConfig::burst_input`] is set (see [`Self::send_burst`]).
    ///
    /// # Errors
    /// Returns an error if writing to the master fails.
    pub fn send(&mut self, command: &str) -> Result<()> {
        if self.config.burst_input {
            return self.send_burst(command);
        }
        let session = &self.session;
        self.jitter
            .type_command(command, |bytes| session.write_input(bytes))
    }

    /// Fast input path for [`WrapperConfig::burst_input`]: write the command
    /// body in fixed-size chunks (no per-char jitter), let the screen settle,
    /// then send the `\r` submit as a *separate* write.
    ///
    /// Rationale (vs. flooding the whole command + `\r` in one write):
    /// - **Chunking the body** keeps each write small enough that an
    ///   Ink-style editor (claude) accumulates it as typed text instead of
    ///   collapsing a large single write to a `[Pasted text]` placeholder.
    /// - **A settled, separate Enter** is registered as a deliberate submit; a
    ///   `\r` glued to the tail of a big burst is otherwise swallowed by the
    ///   editor's paste handling and the prompt is never sent.
    ///
    /// Newlines inside `command` are written as `\r` (matching the jitter
    /// terminator convention) so an editor that submits on Enter does not fire
    /// early on an embedded `\n`; the final submit is a lone `\r` after the
    /// body has rendered.
    ///
    /// # Errors
    /// Returns an error if writing to the master fails.
    pub fn send_burst(&mut self, command: &str) -> Result<()> {
        /// Bytes per body chunk. Small enough to stay under an editor's
        /// fast-input "paste" heuristic, large enough that even a multi-KB
        /// prompt is a handful of writes (delivered in well under a second).
        const CHUNK: usize = 64;
        /// Pause between body chunks: lets the editor's render keep up so the
        /// stream reads as fast typing, not a single pasted block.
        const CHUNK_GAP: Duration = Duration::from_millis(8);
        /// Settle before the submit Enter so the fully-rendered input field
        /// has left any paste-buffering state and accepts the `\r` as submit.
        const SUBMIT_SETTLE: Duration = Duration::from_millis(250);

        // Translate embedded newlines to `\r` so the body matches what the
        // jitter path emits and an Enter-submits editor does not fire early.
        let body: Vec<u8> = command
            .bytes()
            .map(|b| if b == b'\n' { b'\r' } else { b })
            .collect();
        let mut offset = 0;
        while offset < body.len() {
            let end = (offset + CHUNK).min(body.len());
            self.session.write_input(&body[offset..end])?;
            offset = end;
            if offset < body.len() {
                thread::sleep(CHUNK_GAP);
            }
        }
        // Let the input field render and settle, then submit with a lone `\r`.
        thread::sleep(SUBMIT_SETTLE);
        self.session.write_input(b"\r")
    }

    /// Drives the lifecycle state machine until the target is IDLE, has exited,
    /// or the watchdog aborts it.
    ///
    /// Confirmation prompts encountered along the way are auto-answered (when
    /// `auto_confirm` is set).
    ///
    /// # Errors
    /// Returns an error if writing a confirmation reply to the master fails.
    pub fn wait_until_idle(&mut self) -> Result<Outcome> {
        let start = Instant::now();
        let mut last_activity = Instant::now();
        let mut interrupted_at: Option<Instant> = None;
        // The commit index of the prompt line we last answered. A prompt's
        // identity is the number of committed lines beneath it, which advances
        // with every newline regardless of how output is chunked. This answers
        // each distinct confirmation exactly once — even two byte-identical
        // prompts, and even when the intervening output coalesces into a single
        // read so the non-prompt state is never observed on its own.
        let mut answered_at: Option<usize> = None;
        // Approval-menu de-dup (opt-in `auto_approve`). The menus are
        // full-screen and have no stable commit index, so the guard is a simple
        // edge latch: confirm once while the menu is on screen, then re-arm only
        // after it has gone (the menu's text is no longer detected).
        let mut approval_answered = false;
        // TUI settle detection must not fire before the screen has rendered at
        // least once.
        let mut saw_output = false;
        self.state = State::Running;

        loop {
            // Watchdog escalation.
            match interrupted_at {
                Some(t) if t.elapsed() >= self.config.interrupt_grace => {
                    // Graceful Ctrl+C did not work in time: SIGKILL the group.
                    self.session.terminate();
                    return Ok(Outcome::TimedOut);
                }
                None if start.elapsed() >= self.config.exec_timeout => {
                    // First escalation: send Ctrl+C and start the grace timer.
                    let _ = self.session.write_input(&[0x03]);
                    interrupted_at = Some(Instant::now());
                }
                _ => {}
            }

            match self.session.read_output(self.config.poll_interval) {
                Output::Data(chunk) => {
                    saw_output = true;
                    // The line sanitizer is always maintained (so `clean_log`
                    // works); the screen grid only in TUI mode.
                    let sani_changed = self.sanitizer.feed(&chunk);
                    let changed = if self.config.tui {
                        self.screen.as_mut().is_some_and(|s| s.feed(&chunk))
                    } else {
                        sani_changed
                    };
                    if changed {
                        last_activity = Instant::now();
                        self.state = State::Running;
                    }
                    // Confirmation auto-reply is line-oriented; in TUI mode the
                    // prompts are usually full-screen menus, so it is skipped.
                    if !self.config.tui {
                        let line = self.sanitizer.current_line();
                        if is_confirmation_prompt(&line) {
                            self.state = State::ConfirmationPrompt;
                            let prompt_id = self.sanitizer.committed_lines();
                            if self.config.auto_confirm
                                && interrupted_at.is_none()
                                && answered_at != Some(prompt_id)
                            {
                                // Reply `y\r` through the jitter layer (per spec).
                                let session = &self.session;
                                self.jitter
                                    .type_command("y", |bytes| session.write_input(bytes))?;
                                answered_at = Some(prompt_id);
                                last_activity = Instant::now();
                                // The jittered reply may have slept; re-check the
                                // deadline so it cannot push the first escalation
                                // past `exec_timeout`.
                                if start.elapsed() >= self.config.exec_timeout {
                                    let _ = self.session.write_input(&[0x03]);
                                    interrupted_at = Some(Instant::now());
                                }
                            }
                        }
                    }

                    // Opt-in: auto-confirm agentic-CLI approval / trust menus
                    // (arrow-key numbered menus the `[y/n]` path above cannot
                    // answer). These are full-screen, so detection reads the
                    // grid in TUI mode and the sanitized log otherwise. We press
                    // Enter (`\r`) to confirm the default "yes/proceed/trust"
                    // option. An edge latch confirms each menu exactly once and
                    // re-arms only once the menu has left the screen.
                    if self.config.auto_approve && interrupted_at.is_none() {
                        let screen = if self.config.tui {
                            self.screen_text()
                        } else {
                            self.sanitizer.clean_log()
                        };
                        if is_approval_menu(&screen) {
                            self.state = State::ConfirmationPrompt;
                            if !approval_answered {
                                let _ = self.session.write_input(b"\r");
                                approval_answered = true;
                                last_activity = Instant::now();
                            }
                        } else {
                            approval_answered = false;
                        }
                    }
                }
                Output::Idle => {
                    // Silence long enough, and not mid-abort.
                    if interrupted_at.is_none()
                        && last_activity.elapsed() >= self.config.idle_silence
                    {
                        let idle = if self.config.tui {
                            // A settled screen is IDLE for a full-screen TUI;
                            // there is no line prompt to match.
                            saw_output
                        } else {
                            let tokens: Vec<&str> = self
                                .config
                                .prompt_tokens
                                .iter()
                                .map(String::as_str)
                                .collect();
                            line_ends_with_any(&self.sanitizer.current_line(), &tokens)
                        };
                        if idle {
                            self.state = State::Idle;
                            return Ok(Outcome::Idle);
                        }
                    }
                }
                Output::Eof => {
                    return Ok(if interrupted_at.is_some() {
                        Outcome::TimedOut
                    } else {
                        Outcome::Completed
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zero-delay jitter so tests do not sleep through real typing cadence.
    fn instant_jitter() -> Jitter {
        Jitter::with_delays(1, (0, 0), (0, 0))
    }

    fn wrapper(program: &str, args: &[&str], config: WrapperConfig) -> Wrapper {
        let session = PtySession::spawn(program, args).expect("spawn");
        let mut w = Wrapper::with_config(session, config);
        w.set_jitter(instant_jitter());
        w
    }

    #[test]
    fn tui_mode_settles_drives_and_captures_the_screen() {
        // A minimal full-screen TUI: enter the alternate screen, paint a prompt
        // with absolute cursor addressing, read a line, then paint the answer.
        let config = WrapperConfig {
            tui: true,
            idle_silence: Duration::from_millis(300),
            exec_timeout: Duration::from_secs(20),
            poll_interval: Duration::from_millis(50),
            ..WrapperConfig::default()
        };
        let script = "printf '\\033[?1049h\\033[2J\\033[1;1HREADY'; \
                      read x; \
                      printf '\\033[3;1HGOT=%s' \"$x\"; \
                      sleep 0.4";
        let mut w = wrapper("sh", &["-c", script], config);

        let outcome = w.run_command("ping").expect("run");
        assert_eq!(outcome, Outcome::Idle);
        let screen = w.screen_text();
        assert!(screen.contains("READY"), "screen: {screen:?}");
        assert!(
            screen.contains("GOT=ping"),
            "TUI did not receive the typed input; screen: {screen:?}"
        );
        // Dropping `w` terminates the lingering `sleep`.
    }

    #[test]
    fn completes_when_the_child_exits() {
        let mut w = wrapper("sh", &["-c", "echo hello world"], WrapperConfig::default());
        let outcome = w.wait_until_idle().expect("wait");
        assert_eq!(outcome, Outcome::Completed);
        assert!(
            w.clean_log().contains("hello world"),
            "log: {:?}",
            w.clean_log()
        );
    }

    #[test]
    fn auto_answers_a_confirmation_prompt() {
        // The target asks to confirm, reads the answer, and echoes it back.
        let mut w = wrapper(
            "sh",
            &[
                "-c",
                "printf 'Continue? [y/n] '; read ans; printf 'ANSWER=%s\\n' \"$ans\"",
            ],
            WrapperConfig::default(),
        );
        let outcome = w.wait_until_idle().expect("wait");
        assert_eq!(outcome, Outcome::Completed);
        assert!(
            w.clean_log().contains("ANSWER=y"),
            "confirmation not auto-answered; log: {:?}",
            w.clean_log()
        );
    }

    #[test]
    fn answers_two_identical_confirmation_prompts() {
        // Regression: the dedup must re-arm so a second, byte-identical prompt
        // is also answered (it previously hung until the watchdog).
        let mut w = wrapper(
            "sh",
            &[
                "-c",
                "for i in 1 2; do printf 'Continue? [y/n] '; read a; printf 'A%s=%s\\n' \"$i\" \"$a\"; done",
            ],
            WrapperConfig::default(),
        );
        let outcome = w.wait_until_idle().expect("wait");
        assert_eq!(outcome, Outcome::Completed);
        let log = w.clean_log();
        assert!(
            log.contains("A1=y"),
            "first prompt not answered; log: {log:?}"
        );
        assert!(
            log.contains("A2=y"),
            "second prompt not answered; log: {log:?}"
        );
    }

    #[test]
    fn detects_idle_via_trailing_prompt_and_silence() {
        // Print a prompt then idle (without exiting). The wrapper should reach
        // IDLE on the prompt + silence, not wait for the long sleep to finish.
        let config = WrapperConfig {
            idle_silence: Duration::from_millis(250),
            exec_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_millis(50),
            prompt_tokens: vec!["> ".to_string()],
            ..WrapperConfig::default()
        };
        let mut w = wrapper("sh", &["-c", "printf 'ready> '; sleep 30"], config);

        let start = Instant::now();
        let outcome = w.wait_until_idle().expect("wait");
        assert_eq!(outcome, Outcome::Idle);
        assert_eq!(w.state(), State::Idle);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "took too long to detect idle: {:?}",
            start.elapsed()
        );
        assert!(
            w.clean_log().ends_with("ready> "),
            "log: {:?}",
            w.clean_log()
        );
        // Dropping `w` SIGKILLs the lingering `sleep 30`.
    }

    #[test]
    fn watchdog_times_out_and_interrupts_a_hung_target() {
        let config = WrapperConfig {
            exec_timeout: Duration::from_millis(300),
            interrupt_grace: Duration::from_secs(2),
            poll_interval: Duration::from_millis(50),
            idle_silence: Duration::from_millis(200),
            ..WrapperConfig::default()
        };
        // `sleep` has no prompt and never idles; SIGINT (from Ctrl+C) ends it.
        let mut w = wrapper("sh", &["-c", "sleep 30"], config);

        let start = Instant::now();
        let outcome = w.wait_until_idle().expect("wait");
        assert_eq!(outcome, Outcome::TimedOut);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "watchdog took too long: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn run_command_types_into_an_interactive_shell() {
        // Drive an interactive shell: send a command, observe its output, then
        // the shell idles at its prompt. Use a deterministic prompt via PS1.
        let config = WrapperConfig {
            idle_silence: Duration::from_millis(250),
            exec_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_millis(50),
            prompt_tokens: vec!["READY> ".to_string()],
            ..WrapperConfig::default()
        };
        // `sh -i` with a fixed prompt; disable the rcfile noise.
        let mut w = wrapper(
            "sh",
            &["-c", "PS1='READY> '; export PS1; exec sh -i"],
            config,
        );

        // Wait for the first prompt.
        let first = w.wait_until_idle().expect("first idle");
        assert_eq!(first, Outcome::Idle);

        // Send a command and wait for the next prompt.
        let outcome = w.run_command("echo abc123").expect("run");
        assert_eq!(outcome, Outcome::Idle);
        assert!(
            w.clean_log().contains("abc123"),
            "command output missing; log: {:?}",
            w.clean_log()
        );
    }

    #[test]
    fn burst_input_sends_a_large_command_in_one_burst() {
        // `--no-jitter` path: a multi-thousand-char command must be delivered
        // as a fast chunked burst (not minutes of per-char jitter) and still
        // execute. Drive an interactive shell and echo a long string back.
        let config = WrapperConfig {
            idle_silence: Duration::from_millis(250),
            exec_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_millis(50),
            prompt_tokens: vec!["READY> ".to_string()],
            burst_input: true,
            ..WrapperConfig::default()
        };
        let mut w = wrapper(
            "sh",
            &["-c", "PS1='READY> '; export PS1; exec sh -i"],
            config,
        );

        let first = w.wait_until_idle().expect("first idle");
        assert_eq!(first, Outcome::Idle);

        let payload = "q".repeat(3000);
        let cmd = format!("printf 'LEN=%s\\n' \"$(printf %s '{payload}' | wc -c)\"");
        let start = Instant::now();
        let outcome = w.run_command(&cmd).expect("run");
        assert_eq!(outcome, Outcome::Idle);
        assert!(
            w.clean_log().contains("LEN=3000"),
            "burst command did not execute; log: {:?}",
            w.clean_log()
        );
        // The burst is a handful of 64-byte writes + one settle; nowhere near
        // the minutes per-char jitter would take on 3000 chars.
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "burst input was not fast, took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn burst_input_translates_embedded_newlines_to_carriage_returns() {
        // The body's `\n` are rewritten to `\r` so a shell runs each line as a
        // separate statement (and an Enter-submits editor does not fire early).
        // Two newline-separated statements must both execute.
        let config = WrapperConfig {
            idle_silence: Duration::from_millis(250),
            exec_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_millis(50),
            prompt_tokens: vec!["READY> ".to_string()],
            burst_input: true,
            ..WrapperConfig::default()
        };
        let mut w = wrapper(
            "sh",
            &["-c", "PS1='READY> '; export PS1; exec sh -i"],
            config,
        );

        let first = w.wait_until_idle().expect("first idle");
        assert_eq!(first, Outcome::Idle);

        // `send` (burst) rewrites the embedded newline; the trailing submit is a
        // lone `\r`, so the second statement runs too.
        w.send("echo first\necho second").expect("send");
        let outcome = w.wait_until_idle().expect("wait");
        assert_eq!(outcome, Outcome::Idle);
        let log = w.clean_log();
        assert!(
            log.contains("first"),
            "first statement missing; log: {log:?}"
        );
        assert!(
            log.contains("second"),
            "second statement missing; log: {log:?}"
        );
    }
}
