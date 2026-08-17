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

use std::io;
use std::thread;
use std::time::{Duration, Instant};

use crate::ansi::{is_approval_menu, is_confirmation_prompt, line_ends_with_any, Sanitizer};
use crate::error::{Error, Result};
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
    /// Soft-fold each logical input line to at most this many columns at word
    /// boundaries before delivery (the `--wrap-input <COLS>` flag); `0` (the
    /// default) leaves the input untouched.
    ///
    /// An ultra-long *single* logical line overflows an Ink-style editor's
    /// input field so the prompt is never delivered whole; breaking it into
    /// shorter lines (which the model reads identically) makes delivery
    /// reliable. Only applied in the burst path ([`Self::burst_input`]): the
    /// fold introduces newlines, and only the burst path translates them to the
    /// `\r` an Enter-submits editor needs without firing early.
    pub wrap_input: usize,
    /// Optional sentinel gate on IDLE (see [`IdleGate`]): while set,
    /// [`Output::Idle`] silence is ignored until the gate needle has appeared as
    /// its own line in the transcript, or the gate's marker-less grace has
    /// elapsed. `None` (the default) disables the gate.
    pub idle_gate: Option<IdleGate>,
    /// Bracketed-paste input (the `--paste-input` flag): wrap the whole command
    /// body in `ESC[200~`/`ESC[201~` and write it in one shot, then submit with a
    /// settled, separate `\r`. An editor in bracketed-paste mode (claude/codex
    /// enable `ESC[?2004h`) accepts the block — newlines and all — as one atomic
    /// paste, so there is no per-line submit, no length overflow, and no
    /// chunk-timing heuristic. The deterministic alternative to
    /// [`Self::burst_input`]; off by default. Takes precedence over `burst_input`
    /// when both are set. (`wrap_input` folding is unnecessary under paste.)
    pub paste_input: bool,
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
            wrap_input: 0,
            idle_gate: None,
            paste_input: false,
        }
    }
}

/// The sentinel gate on IDLE ([`WrapperConfig::idle_gate`]).
///
/// Used by `--extract` to hold off completion until the model's closing
/// sentinel marker has been emitted — a model that emits startup chrome and
/// then pauses to think must not have that pause mistaken for a finished
/// (empty) reply. The needle is matched as a whole (trimmed) transcript line,
/// so the marker named mid-sentence inside the echoed wrap instruction does not
/// open the gate.
///
/// The needle and its grace live in ONE field on purpose: [`Wrapper::run_command`]
/// `take()`s the gate for the pre-typing readiness wait and restores it
/// afterwards (the closing marker cannot exist before the prompt is even typed),
/// and the grace must be suspended by that same `take()` — otherwise every
/// command would pay the grace before it is delivered.
#[derive(Debug, Clone)]
pub struct IdleGate {
    /// The transcript line that opens the gate (the model's closing marker).
    pub needle: String,
    /// How long the output must stay *continuously* quiet before a settled
    /// screen is accepted as IDLE even though the needle never appeared.
    ///
    /// `None` (sentinel-strict) means the needle is the only completion signal
    /// and the watchdog (`exec_timeout`) is the backstop. `Some(grace)` demotes
    /// the settled screen to a bounded fallback: marker-first (instant), and a
    /// marker-less reply still completes once the screen has been quiet for
    /// `grace`. `Some(Duration::ZERO)` restores pure settle-based completion.
    pub markerless_grace: Option<Duration>,
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

/// Conservative size guardrail (in delivered bytes) above which the
/// `--no-jitter` burst path refuses to deliver and requires `--paste-input`.
///
/// This is a POLICY line, not a measured reliability boundary. The burst
/// mis-delivery documented in issue #60 (empty / truncated-to-tail / garbled
/// replies) is intermittent and prompt-shape dependent, not purely size-bound,
/// so no scalar threshold can cleanly separate a prompt that delivers from one
/// that does not: a smaller shape-triggered failure can still slip under this
/// line, and burst below it is best-effort, not guaranteed. The guardrail only
/// catches the clearly-oversized case and steers it to the deterministic atomic
/// bracketed-paste path (`--paste-input`), which is the correct delivery
/// mechanism for any large prompt. 4096 is a deliberately round, conservative
/// value (~one tty buffer) sitting above the single-line instructions the burst
/// path was designed for; it was chosen as a precaution, not bisected from a
/// pass/fail sweep. See issue #60.
pub(crate) const BURST_MAX_BYTES: usize = 4096;

/// Orchestrates an interactive Target CLI inside a PTY.
pub struct Wrapper {
    session: PtySession,
    sanitizer: Sanitizer,
    /// 2D screen grid, allocated only in TUI mode.
    screen: Option<Screen>,
    jitter: Jitter,
    config: WrapperConfig,
    state: State,
    /// Needle occurrences already in the transcript when the IDLE gate was armed
    /// for the current command; the gate opens only above this count. See
    /// [`Wrapper::arm_idle_gate`].
    gate_baseline_hits: usize,
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
            gate_baseline_hits: 0,
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

    /// The transcript the IDLE gate reads: the screen's full text (scrollback
    /// included) in TUI mode, the sanitized line log otherwise.
    fn transcript(&self) -> String {
        if self.config.tui {
            self.screen_full_text()
        } else {
            self.sanitizer.clean_log()
        }
    }

    /// How many transcript lines currently equal the gate needle (0 when no gate
    /// is configured).
    fn gate_hits(&self) -> usize {
        match &self.config.idle_gate {
            None => 0,
            Some(gate) => transcript_line_hits(&self.transcript(), &gate.needle),
        }
    }

    /// Whether the IDLE gate ([`WrapperConfig::idle_gate`]) is satisfied: `true`
    /// when no gate is configured, or when the needle has appeared as its own
    /// (trimmed) transcript line *since the gate was armed* (see
    /// [`Self::arm_idle_gate`]).
    ///
    /// Public so a caller can tell a run that completed on the closing sentinel
    /// from one that fell back to the marker-less grace.
    pub fn idle_gate_open(&self) -> bool {
        match &self.config.idle_gate {
            None => true,
            Some(_) => self.gate_hits() > self.gate_baseline_hits,
        }
    }

    /// Arms the IDLE gate for a new command: the needle lines already in the
    /// transcript belong to EARLIER commands, so the gate must require a NEW one.
    ///
    /// Without this, a second [`Self::run_command`] with the same needle starts
    /// with the gate already open and its reply wait completes on the first
    /// settled screen — before the second answer exists. Called automatically by
    /// [`Self::run_command`]; call it directly when driving
    /// [`Self::wait_until_idle`] once per command.
    ///
    /// Best-effort, and deliberately so: the baseline is an occurrence COUNT (a
    /// line offset would be invalidated by every erase-display, alt-screen switch
    /// and scrollback eviction), and a repainting emulator can drop and re-render
    /// the same marker within a single read, which the count cannot see. The
    /// robust arrangement is a needle that is unique per command — which is what
    /// the `--extract` CLI does, generating a fresh sentinel pair for every
    /// `--cmd`. Where the needle repeats, a miss costs the marker-less grace, not
    /// a wrong answer.
    pub fn arm_idle_gate(&mut self) {
        self.gate_baseline_hits = self.gate_hits();
    }

    /// Replaces the IDLE gate ([`WrapperConfig::idle_gate`]) between commands —
    /// the `--extract` CLI uses it to install each command's own sentinel needle.
    pub fn set_idle_gate(&mut self, gate: Option<IdleGate>) {
        self.config.idle_gate = gate;
        self.gate_baseline_hits = 0;
    }

    /// Whether a settled screen may be accepted as IDLE even though the closing
    /// marker never appeared. Always `false` for a sentinel-strict gate
    /// (`markerless_grace: None`) — there the marker is the only signal.
    ///
    /// Two ways to qualify:
    /// - the output has been quiet for the whole grace (the normal path), or
    /// - the watchdog is about to fire and the screen is settled *now*. The grace
    ///   is measured from the last content change, not from the start of the
    ///   wait, so a reply whose last chunk lands late leaves less than a full
    ///   grace of budget; without this the wrapper would sit on a perfectly good
    ///   settled screen and exit `124` instead — exactly the failure #55 fixed.
    ///   This bound makes "a marker-less reply that settles is never converted
    ///   into a timeout" true for ANY grace value, including an explicit
    ///   `--extract-grace-ms` larger than the whole timeout.
    fn markerless_settle_allowed(&self, start: Instant, last_activity: Instant) -> bool {
        let Some(grace) = self
            .config
            .idle_gate
            .as_ref()
            .and_then(|gate| gate.markerless_grace)
        else {
            return false;
        };
        last_activity.elapsed() >= grace
            || start.elapsed() + self.config.idle_silence >= self.config.exec_timeout
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
            //
            // The sentinel idle-gate (set for `--extract`) must NOT apply to
            // this readiness wait: the model's closing marker cannot appear
            // before the prompt is even typed, so gating here would never
            // complete — the wait would burn the whole watchdog and `send()`
            // below would never run, so the prompt is never delivered and the
            // reply is empty. The gate belongs only to the post-typing reply
            // wait. Disable it for the readiness wait, then restore it. Taking
            // the whole gate also suspends its marker-less grace, so the
            // readiness wait still completes on the first settled screen instead
            // of paying the grace before the prompt is even delivered.
            let saved_gate = self.config.idle_gate.take();
            let ready = self.wait_until_idle();
            self.config.idle_gate = saved_gate;
            match ready? {
                Outcome::Idle => {}
                other => return Ok(other),
            }
        }
        // Require a NEW closing marker for THIS command: any needle already in
        // the transcript was emitted by an earlier one (the sentinel pair is
        // per-run, not per-command) and would otherwise leave the gate open
        // before the reply even starts. Armed after the readiness wait and
        // before typing — the echoed wrap instruction names the marker only
        // mid-sentence, and the gate matches whole lines.
        self.arm_idle_gate();
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
        if self.config.paste_input {
            return self.send_paste(command);
        }
        if self.config.burst_input {
            return self.send_burst(command);
        }
        let session = &self.session;
        self.jitter
            .type_command(command, |bytes| session.write_input(bytes))
    }

    /// Bracketed-paste input path ([`WrapperConfig::paste_input`]): wrap the whole
    /// command body in the bracketed-paste markers `ESC[200~`/`ESC[201~` and write
    /// it in one shot, then submit with a settled, separate `\r`.
    ///
    /// An Ink-style editor that has enabled bracketed-paste mode (`ESC[?2004h`,
    /// which claude/codex do) accepts the entire block — newlines and all — as
    /// literal pasted text in one atomic operation: no per-line submit, no
    /// length-based input overflow, no chunk-timing heuristic. The trailing `\r`
    /// (after the paste-end marker + a settle) is the deliberate submit. This is
    /// the deterministic alternative to the chunked [`Self::send_burst`].
    ///
    /// Newlines in the body are left as `\n`: paste content is literal, so the
    /// editor inserts them as line breaks rather than submitting on them.
    ///
    /// # Errors
    /// Returns an error if writing to the master fails.
    pub fn send_paste(&mut self, command: &str) -> Result<()> {
        /// Settle before the submit Enter so the editor has finished ingesting
        /// the paste and accepts the `\r` as a deliberate submit.
        const SUBMIT_SETTLE: Duration = Duration::from_millis(250);
        let seq = bracketed_paste(command);
        self.session.write_input(&seq)?;
        thread::sleep(SUBMIT_SETTLE);
        self.session.write_input(b"\r")
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

        // Soft-fold long single lines first: an ultra-long *single* logical line
        // overflows the editor's input field so the prompt is never delivered
        // whole. Breaking it at word boundaries (the model reads the wrapped text
        // identically) makes delivery reliable. The folded newlines become `\r`
        // below, which the burst path delivers as in-line newlines — only the
        // settled trailing `\r` submits.
        let folded;
        let command: &str = if self.config.wrap_input > 0 {
            folded = fold_text(command, self.config.wrap_input);
            &folded
        } else {
            command
        };

        // Translate embedded newlines to `\r` so the body matches what the
        // jitter path emits and an Enter-submits editor does not fire early.
        let body: Vec<u8> = command
            .bytes()
            .map(|b| if b == b'\n' { b'\r' } else { b })
            .collect();
        // Conservative guardrail: refuse the clearly-oversized case loudly rather
        // than risk silently mis-delivering it. Burst delivery of large prompts is
        // best-effort and its #60 failure mode is shape-dependent (not purely
        // size-bound), so this is a precautionary policy line, not a proven
        // boundary — it redirects big prompts to the deterministic `--paste-input`
        // path. `body.len()` is the post-fold, post-newline-translation byte count
        // actually headed for the PTY, so the reported figure can exceed the raw
        // input length after `--wrap-input` folding.
        if body.len() > BURST_MAX_BYTES {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "--no-jitter burst delivery is not reliable for large prompts; \
                     it is capped at {BURST_MAX_BYTES} bytes as a precaution \
                     (got {} bytes after any --wrap-input folding). \
                     Use --paste-input for large prompts.",
                    body.len()
                ),
            )));
        }
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

                    // Sentinel completion: when a gate is set (`--extract`), the
                    // closing marker on its own line is a definitive "reply
                    // complete" signal — it is the last thing the model emits. An
                    // animated TUI (e.g. claude rotates its idle "Try ..." hints)
                    // may never fall silent, so the silence-gated `Output::Idle`
                    // arm below can miss the reply before it scrolls out of the
                    // alt-screen. Completing the moment the gate opens — on the
                    // chunk that renders the marker, while the reply is still on
                    // the grid — fixes that. The watchdog remains the backstop if
                    // the marker never appears. (`idle_gate_open()` reads the
                    // just-fed screen grid in TUI mode.)
                    if self.config.idle_gate.is_some() && self.idle_gate_open() {
                        self.state = State::Idle;
                        return Ok(Outcome::Idle);
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
                        // The sentinel-aware gate (when set) holds off IDLE until
                        // the model has actually emitted its closing marker, so a
                        // mid-think pause is not mistaken for a finished reply.
                        // A gate carrying a marker-less grace demotes the settled
                        // screen to a BOUNDED fallback instead of disabling the
                        // gate outright: a model that omits the marker still
                        // completes, but only after the output has been quiet for
                        // the whole grace — far longer than `idle_silence`, so a
                        // think-pause no longer ends the reply wait. The fallback
                        // also fires on the last of the watchdog budget, so it
                        // never costs a settled screen a `124`.
                        if idle
                            && (self.idle_gate_open()
                                || self.markerless_settle_allowed(start, last_activity))
                        {
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

/// How many lines of `hay`, trimmed, equal `needle`.
///
/// The gate matches a standalone line (not a substring) so the marker named
/// *inside* the echoed wrap instruction — where it sits mid-sentence — does not
/// satisfy it; only the model emitting the marker on its own line does. It
/// counts rather than merely detects so the gate can require a marker emitted
/// for the CURRENT command: see [`Wrapper::arm_idle_gate`].
pub(crate) fn transcript_line_hits(hay: &str, needle: &str) -> usize {
    let needle = needle.trim();
    hay.lines().filter(|l| l.trim() == needle).count()
}

/// Soft-folds `text` so no line exceeds `width` columns, breaking at the last
/// blank within the width window where possible (like `fold -s`) and
/// hard-splitting a word longer than `width`. Existing newlines are preserved;
/// `width == 0` returns the text unchanged. Width is counted in `char`s.
pub(crate) fn fold_text(text: &str, width: usize) -> String {
    if width == 0 {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::new();
    for line in text.split('\n') {
        fold_line(line, width, &mut out);
    }
    out.join("\n")
}

/// Folds a single newline-free `line` into one or more segments of at most
/// `width` columns, appending each to `out`.
fn fold_line(line: &str, width: usize, out: &mut Vec<String>) {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= width {
        out.push(line.to_string());
        return;
    }
    let mut start = 0;
    while chars.len() - start > width {
        let window_end = start + width;
        // Prefer to break after the last blank inside the window; the blank
        // stays on the current segment, matching `fold -s`.
        let brk = (start + 1..window_end)
            .rev()
            .find(|&i| chars[i] == ' ' || chars[i] == '\t');
        let cut = match brk {
            Some(i) => i + 1,
            None => window_end, // no blank to break on: hard-split at the width
        };
        out.push(chars[start..cut].iter().collect());
        start = cut;
    }
    out.push(chars[start..].iter().collect());
}

/// Wraps `body` in the bracketed-paste markers (`ESC[200~` … `ESC[201~`) that an
/// editor in bracketed-paste mode reads as one atomic paste. Does NOT include the
/// submit `\r` (the caller sends that separately after a settle).
pub(crate) fn bracketed_paste(body: &str) -> Vec<u8> {
    const PASTE_BEGIN: &[u8] = b"\x1b[200~";
    const PASTE_END: &[u8] = b"\x1b[201~";
    let mut out = Vec::with_capacity(PASTE_BEGIN.len() + body.len() + PASTE_END.len());
    out.extend_from_slice(PASTE_BEGIN);
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(PASTE_END);
    out
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

    /// The `--extract` shape: grid capture, a short idle window, and a sentinel
    /// gate carrying `grace`. `sh` stands in for the model here; the closing
    /// marker is just a line the "model" does or does not print.
    fn gated_config(grace: Option<Duration>, exec_timeout: Duration) -> WrapperConfig {
        WrapperConfig {
            tui: true,
            idle_silence: Duration::from_millis(250),
            exec_timeout,
            poll_interval: Duration::from_millis(50),
            idle_gate: Some(IdleGate {
                needle: "FCB_T_END".to_string(),
                markerless_grace: grace,
            }),
            ..WrapperConfig::default()
        }
    }

    #[test]
    fn gate_holds_through_a_think_pause_and_completes_on_the_marker() {
        // THE regression this gate exists for: the model emits some chrome, goes
        // quiet for far longer than `idle_silence` while it thinks, and only then
        // writes its reply + closing marker. A settle-only completion returns
        // during that pause and captures a screen without the answer. With the
        // gate the wrapper must sit through the pause and complete the moment the
        // marker lands — promptly, not on the grace.
        let mut w = wrapper(
            "sh",
            &[
                "-c",
                "printf 'thinking\\n'; sleep 1.5; printf 'FCB_T_END\\n'; sleep 30",
            ],
            gated_config(Some(Duration::from_secs(8)), Duration::from_secs(30)),
        );

        let start = Instant::now();
        let outcome = w.wait_until_idle().expect("wait");
        let elapsed = start.elapsed();
        assert_eq!(outcome, Outcome::Idle);
        assert!(
            elapsed >= Duration::from_millis(1200),
            "returned during the think pause (settle-only completion): {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(6),
            "did not complete on the marker (waited for the grace?): {elapsed:?}"
        );
        assert!(
            w.screen_full_text().contains("FCB_T_END"),
            "the marker must be on the captured screen; screen: {:?}",
            w.screen_full_text()
        );
    }

    #[test]
    fn markerless_grace_completes_a_reply_without_the_marker() {
        // The model never emits the closing marker (claude intermittently refuses
        // the wrap protocol). The run must still complete — once the screen has
        // been quiet for the grace — instead of burning the whole watchdog (#55).
        let mut w = wrapper(
            "sh",
            &["-c", "printf 'unfenced answer\\n'; sleep 30"],
            gated_config(Some(Duration::from_millis(1500)), Duration::from_secs(20)),
        );

        let start = Instant::now();
        let outcome = w.wait_until_idle().expect("wait");
        let elapsed = start.elapsed();
        assert_eq!(outcome, Outcome::Idle);
        assert!(
            elapsed >= Duration::from_millis(1200),
            "completed on `idle_silence`, not on the grace: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "the grace did not fire well before the watchdog: {elapsed:?}"
        );
        assert!(!w.idle_gate_open(), "the gate must NOT report a marker");
    }

    #[test]
    fn markerless_settle_is_accepted_on_the_last_of_the_budget() {
        // Regression: the grace is measured from the LAST content change, so a
        // reply whose final chunk lands late has less than a full grace of
        // watchdog budget left. Capping the grace VALUE at `exec_timeout / 2`
        // does not bound that wait — here the last output lands at ~4 s with a
        // 5 s grace and a 6 s watchdog, so waiting the grace out would exit 124
        // on a perfectly good settled screen (the #55 failure, restored). The
        // settled screen must be accepted on the last of the budget instead.
        // (A 1 s idle window leaves a full second between the budget bound at
        // `exec_timeout - idle_silence` = 5 s and the watchdog at 6 s, so CI
        // jitter cannot flip the outcome.)
        let mut w = wrapper(
            "sh",
            &[
                "-c",
                "printf 'first chunk\\n'; sleep 3; printf 'late chunk\\n'; sleep 60",
            ],
            WrapperConfig {
                idle_silence: Duration::from_secs(1),
                ..gated_config(Some(Duration::from_secs(5)), Duration::from_secs(6))
            },
        );

        let start = Instant::now();
        let outcome = w.wait_until_idle().expect("wait");
        let elapsed = start.elapsed();
        assert_eq!(
            outcome,
            Outcome::Idle,
            "a settled screen must never be turned into a timeout by the grace"
        );
        assert!(
            elapsed >= Duration::from_secs(3),
            "the gate did not hold through the pause before the late chunk: {elapsed:?}"
        );
        assert!(
            w.screen_full_text().contains("late chunk"),
            "the late chunk must be in the capture; screen: {:?}",
            w.screen_full_text()
        );
    }

    #[test]
    fn an_oversized_explicit_grace_still_completes() {
        // `--extract-grace-ms` is not clamped at parse time: a caller can ask for
        // a grace longer than the whole timeout. The budget bound must still
        // complete the run on the settled screen rather than exit 124.
        let mut w = wrapper(
            "sh",
            &["-c", "printf 'unfenced answer\\n'; sleep 60"],
            WrapperConfig {
                idle_silence: Duration::from_secs(1),
                ..gated_config(Some(Duration::from_secs(600)), Duration::from_secs(4))
            },
        );

        let start = Instant::now();
        let outcome = w.wait_until_idle().expect("wait");
        let elapsed = start.elapsed();
        assert_eq!(outcome, Outcome::Idle);
        assert!(
            elapsed < Duration::from_secs(4),
            "must complete before the watchdog, took {elapsed:?}"
        );
    }

    #[test]
    fn a_stale_marker_does_not_open_the_gate_for_the_next_command() {
        // Multi-command regression: the sentinel pair is per-RUN, so command A's
        // closing marker is still in the transcript when command B's reply wait
        // starts. If it counted, B's gate would be open before B answered and the
        // wait would end on the first settled screen — worse than the settle-only
        // behaviour it replaced. Arming the gate must require a NEW marker.
        //
        // The "model" prints its marker straight away (command A), then pauses
        // and prints an unfenced second answer (command B).
        let mut w = wrapper(
            "sh",
            &[
                "-c",
                "printf 'FCB_T_END\\n'; sleep 2; printf 'second answer\\n'; sleep 60",
            ],
            gated_config(Some(Duration::from_millis(1500)), Duration::from_secs(20)),
        );

        // Command A completes on its marker.
        let first = w.wait_until_idle().expect("first wait");
        assert_eq!(first, Outcome::Idle);
        assert!(w.idle_gate_open(), "A must complete on its own marker");

        // Command B: re-arm (what `run_command` does per command) and wait.
        w.arm_idle_gate();
        assert!(
            !w.idle_gate_open(),
            "A's marker must not leave the gate open for B"
        );
        let start = Instant::now();
        let second = w.wait_until_idle().expect("second wait");
        let elapsed = start.elapsed();
        assert_eq!(second, Outcome::Idle);
        assert!(
            elapsed >= Duration::from_millis(1400),
            "B completed on A's stale marker (before its own answer existed): {elapsed:?}"
        );
        assert!(
            w.screen_full_text().contains("second answer"),
            "B's answer must be in the capture; screen: {:?}",
            w.screen_full_text()
        );
    }

    #[test]
    fn a_new_marker_reopens_the_gate_after_rearming() {
        // The armed baseline must not blind the gate: a marker emitted for the
        // CURRENT command opens it again, even with an identical needle already
        // in the transcript.
        let mut w = wrapper(
            "sh",
            &[
                "-c",
                "printf 'FCB_T_END\\n'; sleep 1; printf 'FCB_T_END\\n'; sleep 60",
            ],
            gated_config(Some(Duration::from_secs(30)), Duration::from_secs(20)),
        );
        assert_eq!(w.wait_until_idle().expect("first wait"), Outcome::Idle);

        w.arm_idle_gate();
        let start = Instant::now();
        let second = w.wait_until_idle().expect("second wait");
        let elapsed = start.elapsed();
        assert_eq!(second, Outcome::Idle);
        // Completed on the second marker (~1 s), nowhere near the 30 s grace.
        assert!(
            elapsed >= Duration::from_millis(700) && elapsed < Duration::from_secs(10),
            "must complete on the NEW marker, took {elapsed:?}"
        );
    }

    #[test]
    fn strict_gate_without_a_grace_rides_to_the_watchdog() {
        // Strict `--extract` is unchanged: with no grace the marker is the only
        // completion signal and a marker-less run ends on the watchdog.
        let mut config = gated_config(None, Duration::from_millis(700));
        config.interrupt_grace = Duration::from_secs(2);
        let mut w = wrapper(
            "sh",
            &["-c", "printf 'unfenced answer\\n'; sleep 30"],
            config,
        );

        let start = Instant::now();
        let outcome = w.wait_until_idle().expect("wait");
        let elapsed = start.elapsed();
        assert_eq!(outcome, Outcome::TimedOut);
        assert!(
            elapsed >= Duration::from_millis(600),
            "returned before the watchdog: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(6),
            "watchdog late: {elapsed:?}"
        );
    }

    #[test]
    fn zero_grace_matches_ungated_settle_completion() {
        // `--extract-grace-ms 0` is the operator escape hatch and the A/B control
        // arm: a settled screen is accepted immediately, exactly like the ungated
        // (pre-0.13.0 `--extract-structural`) behaviour. Run both and compare.
        // (The marker fast-path stays armed under a zero grace; it can only
        // complete EARLIER, on the same fenced reply.)
        let script = "printf 'unfenced answer\\n'; sleep 30";

        let mut ungated = wrapper(
            "sh",
            &["-c", script],
            WrapperConfig {
                idle_gate: None,
                ..gated_config(None, Duration::from_secs(20))
            },
        );
        let start = Instant::now();
        assert_eq!(ungated.wait_until_idle().expect("wait"), Outcome::Idle);
        let ungated_elapsed = start.elapsed();

        let mut zero_grace = wrapper(
            "sh",
            &["-c", script],
            gated_config(Some(Duration::ZERO), Duration::from_secs(20)),
        );
        let start = Instant::now();
        assert_eq!(zero_grace.wait_until_idle().expect("wait"), Outcome::Idle);
        let zero_elapsed = start.elapsed();

        // Both complete on the first settled screen (a small multiple of
        // `idle_silence`), so neither pays a grace; the bound is wide for CI.
        assert!(
            ungated_elapsed < Duration::from_secs(5),
            "ungated settle completion regressed: {ungated_elapsed:?}"
        );
        assert!(
            zero_elapsed < Duration::from_secs(5),
            "a zero grace must not delay settle completion: {zero_elapsed:?}"
        );
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

    /// Assemble a realistic multi-section audit prompt (instruction + audit
    /// notes + code) whose delivered size exceeds `target` bytes. This is the
    /// mixed-structure shape that triggers #60's mis-delivery, as opposed to a
    /// monolithic `"x".repeat(_)`.
    fn multisection_prompt(target: usize) -> String {
        let instruction = "You are a security auditor. Review the contract below and \
            report reentrancy, integer-overflow, and access-control findings as a \
            numbered list, each with a one-line justification.\n\n";
        let audit_note = "- prior finding: unchecked external call in withdraw()\n\
            - prior finding: missing onlyOwner modifier on setFeeRecipient()\n\
            - prior finding: rounding loss in convertToShares() truncates dust\n";
        let code = "\n```solidity\ncontract Vault {\n    mapping(address => uint256) bal;\n    function withdraw(uint256 amt) external {\n        (bool ok,) = msg.sender.call{value: amt}(\"\");\n        require(ok, \"transfer failed\");\n        bal[msg.sender] -= amt;\n    }\n}\n```\n";
        let mut prompt = String::from(instruction);
        prompt.push_str("Audit notes:\n");
        while prompt.len() < target {
            prompt.push_str(audit_note);
        }
        prompt.push_str(code);
        prompt
    }

    #[test]
    fn send_burst_rejects_oversized_input_loudly() {
        // A clearly-oversized burst body is refused with the remedy named,
        // rather than sent into #60's silent mis-delivery.
        let config = WrapperConfig {
            burst_input: true,
            ..WrapperConfig::default()
        };
        // The guard returns before any PTY write, so the target only needs to
        // exist; a live `sleep` keeps the slave open for symmetry with the
        // accept test below (dropping `w` SIGKILLs it).
        let mut w = wrapper("sh", &["-c", "sleep 5"], config);
        let err = w
            .send(&"x".repeat(BURST_MAX_BYTES + 1))
            .expect_err("oversized burst input must be rejected");
        assert!(
            err.to_string().contains("--paste-input"),
            "the error must name the remedy; got: {err}"
        );
    }

    #[test]
    fn send_burst_accepts_input_at_the_ceiling() {
        // Exactly `BURST_MAX_BYTES` is still delivered (the bound is inclusive):
        // pins the guard is off-by-one-correct, not an over-eager `>=`. A live
        // target keeps the PTY slave open so the burst writes succeed.
        let config = WrapperConfig {
            burst_input: true,
            ..WrapperConfig::default()
        };
        let mut w = wrapper("sh", &["-c", "sleep 5"], config);
        w.send(&"x".repeat(BURST_MAX_BYTES))
            .expect("input at the ceiling must be accepted");
    }

    #[test]
    fn send_burst_rejects_an_oversized_multisection_prompt() {
        // Probe the guardrail with the realistic multi-section shape that
        // triggers #60 (instruction + audit notes + code), not just a monolithic
        // `"x".repeat`. Sized past the guardrail it must be refused loudly with
        // the remedy named. NOTE: this does not prove the shape-dependent failure
        // is fully covered — a sub-guardrail instance can still slip through
        // (see `BURST_MAX_BYTES`); it pins that the guard fires on realistic
        // structured input, not only on a repeated-byte payload.
        let prompt = multisection_prompt(BURST_MAX_BYTES + 256);
        assert!(
            prompt.len() > BURST_MAX_BYTES,
            "the test prompt must exceed the guardrail; len={}",
            prompt.len()
        );
        let config = WrapperConfig {
            burst_input: true,
            ..WrapperConfig::default()
        };
        let mut w = wrapper("sh", &["-c", "sleep 5"], config);
        let err = w
            .send(&prompt)
            .expect_err("an oversized multi-section prompt must be rejected");
        assert!(
            err.to_string().contains("--paste-input"),
            "the error must name the remedy; got: {err}"
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

    #[test]
    fn wrap_input_leaves_a_short_command_intact() {
        // With `--wrap-input` set, a command already under the width is delivered
        // unchanged (no spurious folding) and still executes.
        let config = WrapperConfig {
            idle_silence: Duration::from_millis(250),
            exec_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_millis(50),
            prompt_tokens: vec!["READY> ".to_string()],
            burst_input: true,
            wrap_input: 72,
            ..WrapperConfig::default()
        };
        let mut w = wrapper(
            "sh",
            &["-c", "PS1='READY> '; export PS1; exec sh -i"],
            config,
        );
        let first = w.wait_until_idle().expect("first idle");
        assert_eq!(first, Outcome::Idle);

        w.send("echo hello_wrap").expect("send");
        let outcome = w.wait_until_idle().expect("wait");
        assert_eq!(outcome, Outcome::Idle);
        assert!(
            w.clean_log().contains("hello_wrap"),
            "wrapped short command did not run; log: {:?}",
            w.clean_log()
        );
    }

    #[test]
    fn fold_text_zero_width_is_identity() {
        let s = "a very long line that should not be touched at all in this case";
        assert_eq!(fold_text(s, 0), s);
    }

    #[test]
    fn fold_text_short_line_unchanged() {
        assert_eq!(fold_text("short line", 72), "short line");
    }

    #[test]
    fn fold_text_breaks_at_word_boundaries_and_preserves_words() {
        let s = "the quick brown fox jumps over the lazy dog again and again now";
        let folded = fold_text(s, 20);
        for line in folded.lines() {
            assert!(line.chars().count() <= 20, "line exceeds width: {line:?}");
        }
        // Folding only inserts newlines at blanks, so no word is broken.
        assert_eq!(
            s.split_whitespace().collect::<Vec<_>>(),
            folded.split_whitespace().collect::<Vec<_>>(),
            "words must be preserved: {folded:?}"
        );
    }

    #[test]
    fn fold_text_hard_splits_an_overlong_word() {
        let s = "x".repeat(50);
        let folded = fold_text(&s, 20);
        for line in folded.lines() {
            assert!(line.chars().count() <= 20, "line exceeds width: {line:?}");
        }
        // No characters are lost when a word is hard-split.
        assert_eq!(folded.replace('\n', ""), s);
    }

    #[test]
    fn fold_text_preserves_existing_newlines() {
        let s = "first paragraph\n\nsecond paragraph fits";
        assert_eq!(fold_text(s, 72), s);
    }

    #[test]
    fn fold_text_counts_columns_in_chars_not_bytes() {
        // Multibyte chars count as one column each.
        let s = "ččččč ččččč ččččč";
        let folded = fold_text(s, 11);
        for line in folded.lines() {
            assert!(line.chars().count() <= 11, "line exceeds width: {line:?}");
        }
    }

    #[test]
    fn idle_gate_matches_a_standalone_marker_line_only() {
        let begin = "FCB_abc_BEGIN";
        // The marker named mid-sentence in the echoed instruction must NOT open
        // the gate (otherwise the gate is satisfied before the model replies).
        let echo = "wrap your reply between FCB_abc_BEGIN and the closing marker.";
        assert_eq!(transcript_line_hits(echo, begin), 0);
        // The model emitting it on its own line — even indented — opens the gate.
        let reply = "thinking…\n  FCB_abc_BEGIN\nthe answer line\n";
        assert_eq!(transcript_line_hits(reply, begin), 1);
    }

    #[test]
    fn idle_gate_opens_on_indented_closing_marker_claude_layout() {
        // claude v2.1.177 renders the reply as an indented bullet block: the
        // closing END marker lands two spaces under the `●` block, never flush
        // left. The sentinel gate (and so the #53 on-output completion) must
        // still recognise it as a standalone marker line — otherwise the gate
        // never opens and `--extract` waits out the watchdog, by which time the
        // animated idle TUI has scrolled the reply out of the alt-screen. The
        // gate keys on the CLOSING marker — the last thing the model emits — so
        // it being on its own indented line is what matters; the BEGIN marker
        // shares the `●` bullet's line and need not stand alone.
        let transcript = "● FCB_X_BEGIN\n  2+2 = 4.\n  FCB_X_END\n";
        assert_eq!(transcript_line_hits(transcript, "FCB_X_END"), 1);
        // The bullet-prefixed BEGIN line is not a standalone marker, and the gate
        // does not require it to be.
        assert_eq!(transcript_line_hits(transcript, "FCB_X_BEGIN"), 0);
    }

    #[test]
    fn bracketed_paste_wraps_body_in_markers_without_submit() {
        let seq = bracketed_paste("line one\nline two");
        assert!(
            seq.starts_with(b"\x1b[200~"),
            "must start with the paste-begin marker"
        );
        assert!(
            seq.ends_with(b"\x1b[201~"),
            "must end with the paste-end marker"
        );
        // The body is carried verbatim (newlines preserved, no \r submit inside).
        let inner = &seq[b"\x1b[200~".len()..seq.len() - b"\x1b[201~".len()];
        assert_eq!(inner, b"line one\nline two");
        assert!(!seq.contains(&b'\r'), "the paste sequence must not submit");
    }

    #[test]
    fn bracketed_paste_handles_empty_body() {
        assert_eq!(bracketed_paste(""), b"\x1b[200~\x1b[201~");
    }
}
