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

use std::time::{Duration, Instant};

use crate::ansi::{is_confirmation_prompt, line_ends_with_any, Sanitizer};
use crate::error::Result;
use crate::jitter::Jitter;
use crate::pty::{Output, PtySession};

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
        Self {
            session,
            sanitizer: Sanitizer::new(),
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
        self.send(command)?;
        self.wait_until_idle()
    }

    /// Types `command` into the target with human-like jitter, terminated by a
    /// carriage return.
    ///
    /// # Errors
    /// Returns an error if writing to the master fails.
    pub fn send(&mut self, command: &str) -> Result<()> {
        let session = &self.session;
        self.jitter
            .type_command(command, |bytes| session.write_input(bytes))
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
        // Whether the confirmation prompt currently on screen has already been
        // answered. Re-armed once the current line stops looking like a prompt,
        // so a *new* confirmation (even one with identical text) is answered
        // again, while the echoed reply does not trigger a second answer.
        let mut answered = false;
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
                    if self.sanitizer.feed(&chunk) {
                        last_activity = Instant::now();
                        self.state = State::Running;
                    }
                    let line = self.sanitizer.current_line();
                    if is_confirmation_prompt(&line) {
                        self.state = State::ConfirmationPrompt;
                        if self.config.auto_confirm && interrupted_at.is_none() && !answered {
                            // Reply `y\r` through the jitter layer (per spec).
                            let session = &self.session;
                            self.jitter
                                .type_command("y", |bytes| session.write_input(bytes))?;
                            answered = true;
                            last_activity = Instant::now();
                            // The jittered reply may have slept; re-check the
                            // deadline so it cannot push the first escalation
                            // past `exec_timeout`.
                            if start.elapsed() >= self.config.exec_timeout {
                                let _ = self.session.write_input(&[0x03]);
                                interrupted_at = Some(Instant::now());
                            }
                        }
                    } else {
                        // No longer at a confirmation prompt: re-arm.
                        answered = false;
                    }
                }
                Output::Idle => {
                    // Silence: IDLE iff the trailing prompt is on the current
                    // (uncommitted) line and we have been quiet long enough and
                    // we are not mid-abort.
                    if interrupted_at.is_none()
                        && last_activity.elapsed() >= self.config.idle_silence
                    {
                        let tokens: Vec<&str> = self
                            .config
                            .prompt_tokens
                            .iter()
                            .map(String::as_str)
                            .collect();
                        if line_ends_with_any(&self.sanitizer.current_line(), &tokens) {
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
}
