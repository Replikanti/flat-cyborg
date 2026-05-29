//! Output ANSI state machine & stream stripping.
//!
//! The PTY master emits raw bytes interleaved with ANSI escape sequences
//! (colors, cursor movement, line erasures, spinners). This module turns that
//! raw stream into clean text and exposes the primitives the wrapper needs to
//! classify the Target CLI's lifecycle state.
//!
//! Two layers are provided, both driven by a single byte-level [`Parser`]:
//!
//! - [`AnsiStripper`] / [`strip_ansi`] — remove ANSI escape sequences while
//!   preserving every other byte. This matches the spec's regex
//!   `\x1B\[[0-9;]*[a-zA-Z]` in intent but also handles OSC and other escape
//!   forms a bare regex would miss.
//! - [`Sanitizer`] — a single-line terminal emulator that additionally applies
//!   carriage returns, backspaces, cursor-column moves, and line erasures so
//!   that overwrite-based progress spinners collapse to their final frame and
//!   leave no artifacts in the sanitized log.
//!
//! The parser is 7-bit oriented (the wrapper forces `TERM=xterm-256color`, so
//! escape sequences arrive in their 7-bit `ESC ...` form). State — including
//! partial sequences and partial UTF-8 code points — is retained across calls,
//! so the stream may be fed in arbitrary chunks.
//!
//! State detection helpers ([`is_confirmation_prompt`], [`line_ends_with_any`])
//! operate on the sanitized stream. The temporal half of state detection
//! (RUNNING vs. IDLE, which depends on a period of silence) lives in the
//! wrapper, since it requires a clock.

/// Upper bound on the addressable column, bounding the per-line allocation so a
/// malicious cursor-forward sequence (`ESC[<huge>C`) cannot exhaust memory.
const MAX_COL: usize = 1 << 16;

/// Upper bound on an OSC/string payload before it is treated as runaway and
/// aborted, so an unterminated OSC cannot swallow the entire stream.
const MAX_OSC: usize = 1 << 16;

/// Sink for the structural events produced by [`Parser`].
pub(crate) trait Perform {
    /// A printable character (already UTF-8 decoded).
    fn print(&mut self, c: char);
    /// A C0 control byte (`\n`, `\r`, `\t`, backspace, ...).
    fn execute(&mut self, byte: u8);
    /// A completed CSI sequence: numeric parameters, the private-marker byte
    /// (e.g. `?` in `ESC[?1049h`) if present, and the final byte.
    fn csi(&mut self, params: &[u16], private: Option<u8>, final_byte: u8);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PState {
    Ground,
    Esc,
    EscIntermediate,
    Csi,
    OscString,
    OscStringEsc,
}

/// Returns `true` for the C0 control bytes that are executed immediately even
/// in the middle of an escape sequence (per ECMA-48). `CAN`/`SUB` (which abort)
/// and `ESC` (which restarts) are handled separately.
fn is_executable_c0(byte: u8) -> bool {
    matches!(byte, 0x00..=0x17 | 0x19 | 0x1C..=0x1F)
}

/// A compact, allocation-light ANSI/VT byte parser.
///
/// Feed it bytes one at a time via [`Parser::advance`]; it decodes UTF-8,
/// recognizes CSI/OSC/other escape sequences, and reports structural events to
/// a [`Perform`] sink.
#[derive(Debug)]
pub(crate) struct Parser {
    state: PState,
    params: Vec<u16>,
    cur: u32,
    saw_digit: bool,
    private: Option<u8>,
    osc_len: usize,
    utf8_buf: [u8; 4],
    utf8_len: usize,
    utf8_need: usize,
}

impl Default for Parser {
    fn default() -> Self {
        Parser {
            state: PState::Ground,
            params: Vec::new(),
            cur: 0,
            saw_digit: false,
            private: None,
            osc_len: 0,
            utf8_buf: [0; 4],
            utf8_len: 0,
            utf8_need: 0,
        }
    }
}

impl Parser {
    fn reset_params(&mut self) {
        self.params.clear();
        self.cur = 0;
        self.saw_digit = false;
        self.private = None;
    }

    /// Emits whatever has accumulated in the UTF-8 buffer, substituting the
    /// replacement character if the bytes do not form a valid code point.
    pub(crate) fn flush_utf8<P: Perform>(&mut self, perform: &mut P) {
        if self.utf8_len == 0 {
            return;
        }
        match std::str::from_utf8(&self.utf8_buf[..self.utf8_len]) {
            Ok(s) => {
                if let Some(c) = s.chars().next() {
                    perform.print(c);
                }
            }
            Err(_) => perform.print('\u{FFFD}'),
        }
        self.utf8_len = 0;
        self.utf8_need = 0;
    }

    /// Advances the parser by a single byte.
    pub(crate) fn advance<P: Perform>(&mut self, perform: &mut P, byte: u8) {
        // Mid UTF-8 multi-byte sequence: only continuation bytes are valid.
        if self.utf8_need > 0 {
            if (0x80..=0xBF).contains(&byte) {
                self.utf8_buf[self.utf8_len] = byte;
                self.utf8_len += 1;
                if self.utf8_len == self.utf8_need {
                    self.flush_utf8(perform);
                }
                return;
            }
            // Invalid continuation: emit what we have, then reprocess `byte`.
            perform.print('\u{FFFD}');
            self.utf8_len = 0;
            self.utf8_need = 0;
        }

        // ESC starts (or restarts) an escape sequence from any state. Inside an
        // OSC string it may instead be the ST introducer.
        if byte == 0x1B {
            if matches!(self.state, PState::OscString) {
                self.state = PState::OscStringEsc;
            } else {
                self.reset_params();
                self.state = PState::Esc;
            }
            return;
        }

        match self.state {
            PState::Ground => self.ground(perform, byte),
            PState::Esc => self.escape(perform, byte),
            PState::EscIntermediate => self.esc_intermediate(perform, byte),
            PState::Csi => self.csi(perform, byte),
            PState::OscString => self.osc(perform, byte),
            PState::OscStringEsc => {
                if byte == b'\\' {
                    self.state = PState::Ground; // ST terminator (ESC \)
                } else {
                    // The ESC began a fresh escape sequence, not an ST.
                    self.reset_params();
                    self.state = PState::Esc;
                    self.escape(perform, byte);
                }
            }
        }
    }

    fn ground<P: Perform>(&mut self, perform: &mut P, byte: u8) {
        match byte {
            // C0 controls and DEL are executed, not printed. (ESC handled in
            // `advance`.)
            b if b < 0x20 || b == 0x7F => perform.execute(b),
            b if b < 0x80 => perform.print(b as char),
            // UTF-8 lead byte: determine how many continuation bytes follow.
            b => {
                let need = match b {
                    0xC0..=0xDF => 2,
                    0xE0..=0xEF => 3,
                    0xF0..=0xF7 => 4,
                    _ => 0, // invalid lead
                };
                if need == 0 {
                    perform.print('\u{FFFD}');
                } else {
                    self.utf8_buf[0] = b;
                    self.utf8_len = 1;
                    self.utf8_need = need;
                }
            }
        }
    }

    fn escape<P: Perform>(&mut self, perform: &mut P, byte: u8) {
        match byte {
            b if is_executable_c0(b) => perform.execute(b), // stay in Esc
            0x18 | 0x1A => self.state = PState::Ground,     // CAN/SUB abort
            b'[' => self.state = PState::Csi,
            // OSC and the other string-style sequences (DCS/SOS/PM/APC) all
            // run until BEL or ST; treat them uniformly.
            b']' | b'P' | b'X' | b'^' | b'_' => {
                self.osc_len = 0;
                self.state = PState::OscString;
            }
            // Intermediate byte (e.g. charset designators `ESC ( B`): one more
            // byte follows before the sequence completes.
            0x20..=0x2F => self.state = PState::EscIntermediate,
            // Any other byte is a complete two-byte escape; drop it.
            _ => self.state = PState::Ground,
        }
    }

    fn esc_intermediate<P: Perform>(&mut self, perform: &mut P, byte: u8) {
        match byte {
            b if is_executable_c0(b) => perform.execute(b), // stay
            _ => self.state = PState::Ground,
        }
    }

    fn csi<P: Perform>(&mut self, perform: &mut P, byte: u8) {
        match byte {
            b if is_executable_c0(b) => perform.execute(b), // execute, stay in CSI
            0x18 | 0x1A => {
                self.reset_params();
                self.state = PState::Ground; // CAN/SUB abort
            }
            0x30..=0x39 => {
                self.cur = (self.cur * 10 + u32::from(byte - b'0')).min(u32::from(u16::MAX));
                self.saw_digit = true;
            }
            b';' => {
                self.params.push(self.cur as u16);
                self.cur = 0;
                self.saw_digit = false;
            }
            // Private markers ('<' '=' '>' '?'): remembered so handlers can
            // distinguish e.g. DECSET `ESC[?1049h` from plain CSI.
            0x3C..=0x3F => self.private = Some(byte),
            // Colon subparameter separators (ISO 8613-6 / colon-form SGR) and
            // intermediates: ignored so the sequence still terminates on its
            // final byte.
            0x3A | 0x20..=0x2F => {}
            // DEL is ignored within a CSI.
            0x7F => {}
            // Final byte completes the sequence.
            0x40..=0x7E => {
                if self.saw_digit {
                    self.params.push(self.cur as u16);
                }
                let params = std::mem::take(&mut self.params);
                perform.csi(&params, self.private, byte);
                self.params = params;
                self.reset_params();
                self.state = PState::Ground;
            }
            _ => {
                self.reset_params();
                self.state = PState::Ground;
            }
        }
    }

    fn osc<P: Perform>(&mut self, perform: &mut P, byte: u8) {
        // (ESC and BEL are handled before reaching here / below.)
        self.osc_len += 1;
        match byte {
            0x07 => self.state = PState::Ground, // BEL terminates OSC
            // A newline almost certainly means the OSC was truncated or
            // malformed; abort and execute the control rather than swallow the
            // rest of the stream.
            b'\n' | b'\r' => {
                self.state = PState::Ground;
                perform.execute(byte);
            }
            _ if self.osc_len > MAX_OSC => self.state = PState::Ground, // runaway guard
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// AnsiStripper — remove escape sequences, preserve everything else.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StripSink {
    out: String,
}

impl Perform for StripSink {
    fn print(&mut self, c: char) {
        self.out.push(c);
    }
    fn execute(&mut self, byte: u8) {
        // Preserve the raw control bytes; only escape sequences are removed.
        self.out.push(byte as char);
    }
    fn csi(&mut self, _params: &[u16], _private: Option<u8>, _final_byte: u8) {}
}

/// Streaming remover of ANSI escape sequences.
///
/// Retains parser state across [`AnsiStripper::feed`] calls, so escape
/// sequences split across chunk boundaries are handled correctly. Bytes that
/// are not part of an escape sequence (including `\r`, `\n`, `\t`) pass
/// through unchanged.
#[derive(Debug, Default)]
pub struct AnsiStripper {
    parser: Parser,
}

impl AnsiStripper {
    /// Creates a fresh stripper.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds a chunk of raw bytes and returns the escape-free text produced by
    /// this chunk.
    pub fn feed(&mut self, input: &[u8]) -> String {
        let mut sink = StripSink::default();
        for &b in input {
            self.parser.advance(&mut sink, b);
        }
        sink.out
    }

    /// Flushes any dangling partial UTF-8 code point at the true end of the
    /// stream, emitting the replacement character for it. Returns the text (if
    /// any) produced.
    pub fn finish(&mut self) -> String {
        let mut sink = StripSink::default();
        self.parser.flush_utf8(&mut sink);
        sink.out
    }
}

/// Removes ANSI escape sequences from `input` in a single call.
///
/// Equivalent in intent to the spec's `\x1B\[[0-9;]*[a-zA-Z]` strip, but also
/// strips OSC and other escape forms. All non-escape bytes are preserved.
pub fn strip_ansi(input: &str) -> String {
    let mut stripper = AnsiStripper::new();
    let mut out = stripper.feed(input.as_bytes());
    out.push_str(&stripper.finish());
    out
}

// ---------------------------------------------------------------------------
// Sanitizer — single-line terminal emulator for clean, artifact-free logs.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Canvas {
    committed: String,
    line: Vec<char>,
    col: usize,
    changed: bool,
    committed_lines: usize,
}

impl Canvas {
    fn pad_to_col(&mut self) {
        while self.line.len() < self.col {
            self.line.push(' ');
        }
    }

    fn write(&mut self, c: char) {
        self.pad_to_col();
        if self.col < self.line.len() {
            self.line[self.col] = c;
        } else {
            self.line.push(c);
        }
        self.col += 1;
        self.changed = true;
    }

    fn commit_line(&mut self) {
        // The line is committed verbatim. Trailing whitespace is preserved so
        // that meaningful trailing content (e.g. a prompt's `> `) survives;
        // detection helpers trim where they need to. Spinner artifacts are
        // already gone, having been overwritten or erased before the newline.
        let s: String = self.line.iter().collect();
        self.committed.push_str(&s);
        self.committed.push('\n');
        self.line.clear();
        self.col = 0;
        self.committed_lines += 1;
        self.changed = true;
    }
}

impl Perform for Canvas {
    fn print(&mut self, c: char) {
        self.write(c);
    }

    fn execute(&mut self, byte: u8) {
        // Pure cursor movements (CR, BS, HT) do not mark the canvas changed —
        // only visible content changes (writes, commits, erases) do, so the
        // wrapper's RUNNING detection is not fooled by cursor churn.
        match byte {
            b'\n' => self.commit_line(),
            b'\r' => self.col = 0,
            0x08 => self.col = self.col.saturating_sub(1),
            b'\t' => self.col = ((self.col / 8 + 1) * 8).min(MAX_COL),
            _ => {}
        }
    }

    fn csi(&mut self, params: &[u16], _private: Option<u8>, final_byte: u8) {
        let first = params.first().copied().unwrap_or(0);
        match final_byte {
            // Cursor horizontal absolute (1-based), clamped.
            b'G' => self.col = (first.max(1) as usize - 1).min(MAX_COL),
            // Cursor forward / back, clamped.
            b'C' => self.col = (self.col + first.max(1) as usize).min(MAX_COL),
            b'D' => self.col = self.col.saturating_sub(first.max(1) as usize),
            // Erase in line: 0 = to end, 1 = to start (inclusive of cursor),
            // 2 = whole line.
            b'K' => {
                match first {
                    0 => self.line.truncate(self.col),
                    1 => {
                        self.pad_to_col();
                        let end = (self.col + 1).min(self.line.len());
                        for cell in self.line.iter_mut().take(end) {
                            *cell = ' ';
                        }
                    }
                    2 => self.line.clear(),
                    _ => {}
                }
                self.changed = true;
            }
            // SGR (colors) and everything else affect rendering only; ignore.
            _ => {}
        }
    }
}

/// A single-line terminal emulator that produces a clean, artifact-free log.
///
/// Unlike [`AnsiStripper`], the sanitizer interprets carriage returns,
/// backspaces, cursor-column moves, and line erasures, applying them to a
/// line canvas. Overwrite-based progress spinners therefore collapse to their
/// final frame instead of leaving a trail of intermediate frames in the log.
#[derive(Debug, Default)]
pub struct Sanitizer {
    parser: Parser,
    canvas: Canvas,
}

impl std::fmt::Debug for Canvas {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Canvas")
            .field("committed_len", &self.committed.len())
            .field("col", &self.col)
            .field("line_len", &self.line.len())
            .finish()
    }
}

impl Sanitizer {
    /// Creates a fresh sanitizer with an empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds a chunk of raw bytes from the PTY master.
    ///
    /// Returns `true` if the visible canvas changed as a result (used by the
    /// wrapper to distinguish the RUNNING state from silence). Pure cursor
    /// movements do not count as a change.
    pub fn feed(&mut self, input: &[u8]) -> bool {
        self.canvas.changed = false;
        for &b in input {
            self.parser.advance(&mut self.canvas, b);
        }
        self.canvas.changed
    }

    /// Flushes any dangling partial UTF-8 at the true end of the stream.
    pub fn finish(&mut self) {
        self.parser.flush_utf8(&mut self.canvas);
    }

    /// The full sanitized log: all committed lines plus the current,
    /// not-yet-newline-terminated line.
    pub fn clean_log(&self) -> String {
        let mut out = self.canvas.committed.clone();
        let current: String = self.canvas.line.iter().collect();
        out.push_str(&current);
        out
    }

    /// The current (uncommitted) line verbatim.
    pub fn current_line(&self) -> String {
        self.canvas.line.iter().collect()
    }

    /// The number of lines committed (i.e. newline-terminated) so far.
    ///
    /// This advances by one per `\n` regardless of how the stream is chunked,
    /// so it serves as a stable identity for "the line currently on screen":
    /// the current prompt belongs to commit index `committed_lines()`.
    pub fn committed_lines(&self) -> usize {
        self.canvas.committed_lines
    }
}

// ---------------------------------------------------------------------------
// Detection primitives.
// ---------------------------------------------------------------------------

/// Returns the last non-empty line of `text`, or `""` if there is none.
fn last_non_empty_line(text: &str) -> &str {
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
}

/// Returns `true` if the last non-empty line of `text` looks like a yes/no
/// confirmation prompt: a bracketed or parenthesized group whose options
/// (split on `/` or `,`) include both a "yes" and a "no" choice — e.g.
/// `[y/n]`, `(Y/n)`, `[yes/no]`, `[y/N/a]`, `[y,N,a,q,?]`. A bare `y/n` or
/// `yes/no` (no brackets) is also accepted.
///
/// Only the last line is inspected, so an already-answered prompt scrolled up
/// in the buffer does not trigger a match.
pub fn is_confirmation_prompt(text: &str) -> bool {
    let line = last_non_empty_line(text).to_ascii_lowercase();

    for (open, close) in [('[', ']'), ('(', ')')] {
        let mut rest = line.as_str();
        while let Some(o) = rest.find(open) {
            let after = &rest[o + 1..];
            let Some(c) = after.find(close) else { break };
            let group = &after[..c];
            if group_is_yes_no(group) {
                return true;
            }
            rest = &after[c + 1..];
        }
    }

    // Bracket-less forms.
    line.contains("y/n") || line.contains("yes/no")
}

/// Whether a bracket group's options include both a yes and a no choice.
fn group_is_yes_no(group: &str) -> bool {
    let mut has_yes = false;
    let mut has_no = false;
    for opt in group.split(['/', ',']) {
        match opt.trim() {
            "y" | "yes" => has_yes = true,
            "n" | "no" => has_no = true,
            _ => {}
        }
    }
    has_yes && has_no
}

/// Returns `true` if the last non-empty line of `text` ends with any of the
/// given prompt tokens, matched verbatim.
///
/// Used to recognize a Target CLI's trailing prompt. Tokens are matched exactly
/// (including any trailing space), so callers should pass distinctive tokens
/// such as `"> "` or `"$ "` rather than a bare `">"`, which would also match
/// ordinary text like `Vec<T>`.
pub fn line_ends_with_any(text: &str, tokens: &[&str]) -> bool {
    let line = last_non_empty_line(text);
    tokens.iter().any(|t| !t.is_empty() && line.ends_with(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_sgr_color_codes() {
        let input = "\x1b[31mred\x1b[0m and \x1b[1;32mgreen\x1b[0m";
        assert_eq!(strip_ansi(input), "red and green");
    }

    #[test]
    fn strips_colon_form_sgr() {
        // ISO 8613-6 colon-form truecolor SGR must be fully consumed, not
        // leaked as `2:255:0:0m...` into the clean text.
        assert_eq!(strip_ansi("\x1b[38:2:255:0:0mX\x1b[0m"), "X");
    }

    #[test]
    fn strips_cursor_and_erase_sequences() {
        let input = "loading\x1b[2K\x1b[1Gdone\x1b[K";
        assert_eq!(strip_ansi(input), "loadingdone");
    }

    #[test]
    fn strips_osc_title_sequence() {
        let input = "\x1b]0;my title\x07hello";
        assert_eq!(strip_ansi(input), "hello");
        let st = "\x1b]0;title\x1b\\world";
        assert_eq!(strip_ansi(st), "world");
    }

    #[test]
    fn unterminated_osc_does_not_swallow_the_stream() {
        // A truncated OSC (no BEL/ST) followed by a newline must abort, not
        // eat everything that follows.
        let input = "\x1b]0;title-without-terminator\nreal output\n";
        assert_eq!(strip_ansi(input), "\nreal output\n");
    }

    #[test]
    fn c0_control_inside_csi_is_not_lost() {
        // A newline arriving mid-CSI is executed (preserved), and the CSI still
        // terminates on its final byte.
        assert_eq!(strip_ansi("a\x1b[3\n1mb"), "a\nb");
    }

    #[test]
    fn esc_restarts_sequence_from_within_csi() {
        // An ESC mid-CSI begins a new escape; nothing leaks.
        assert_eq!(strip_ansi("x\x1b[3\x1b[31my"), "xy");
    }

    #[test]
    fn preserves_control_and_utf8_bytes() {
        let input = "a\tb\r\nčでrest";
        assert_eq!(strip_ansi(input), "a\tb\r\nčでrest");
    }

    #[test]
    fn dangling_utf8_flushed_on_finish() {
        let mut s = AnsiStripper::new();
        // Lead byte of a 2-byte sequence with no continuation before EOS.
        let out = s.feed(&[0xC3]);
        assert_eq!(out, "");
        assert_eq!(s.finish(), "\u{FFFD}");
    }

    #[test]
    fn handles_escape_split_across_chunks() {
        let mut s = AnsiStripper::new();
        let mut out = s.feed(b"foo\x1b[3");
        out.push_str(&s.feed(b"1mbar"));
        assert_eq!(out, "foobar");
    }

    #[test]
    fn sanitizer_collapses_carriage_return_spinner() {
        let mut s = Sanitizer::new();
        s.feed(b"\r\x1b[36m|\x1b[0m working...\x1b[K");
        s.feed(b"\r\x1b[36m/\x1b[0m working...\x1b[K");
        s.feed(b"\r\x1b[36m-\x1b[0m working...\x1b[K");
        s.feed(b"\rdone!\x1b[K\n");
        assert_eq!(s.clean_log(), "done!\n");
    }

    #[test]
    fn sanitizer_applies_erase_line_after_carriage_return() {
        let mut s = Sanitizer::new();
        s.feed(b"a long status line\r");
        s.feed(b"short\x1b[K\n");
        assert_eq!(s.clean_log(), "short\n");
    }

    #[test]
    fn sanitizer_erase_to_start_is_inclusive_of_cursor() {
        let mut s = Sanitizer::new();
        // Cursor at col 2 (on 'c'); ESC[1K erases columns 0..=2 inclusive.
        s.feed(b"abcde\r\x1b[2C\x1b[1K");
        assert_eq!(s.current_line(), "   de");
    }

    #[test]
    fn sanitizer_handles_backspace() {
        let mut s = Sanitizer::new();
        s.feed(b"abc\x08X");
        assert_eq!(s.current_line(), "abX");

        let mut s2 = Sanitizer::new();
        s2.feed(b"abc\x08 \x08");
        assert_eq!(s2.current_line(), "ab ");
    }

    #[test]
    fn sanitizer_change_flag_ignores_pure_cursor_moves() {
        let mut s = Sanitizer::new();
        assert!(s.feed(b"output"));
        assert!(!s.feed(b"")); // nothing
        assert!(!s.feed(b"\r")); // pure cursor move — not a content change
        assert!(!s.feed(b"\x08")); // backspace — cursor only
        assert!(s.feed(b"x")); // a real write
    }

    #[test]
    fn sanitizer_clamps_runaway_cursor_forward() {
        let mut s = Sanitizer::new();
        // A huge cursor-forward must not allocate gigabytes; the column is
        // clamped and a following write stays bounded.
        s.feed(b"\x1b[2000000000CX\n");
        assert!(s.clean_log().len() <= MAX_COL + 2);
    }

    #[test]
    fn detects_confirmation_prompts() {
        assert!(is_confirmation_prompt("Proceed? [y/n]"));
        assert!(is_confirmation_prompt("Overwrite file? (Y/n)"));
        assert!(is_confirmation_prompt("Delete all? (y/N) "));
        assert!(is_confirmation_prompt("Continue [yes/no]"));
        assert!(is_confirmation_prompt("Apply patch [y/N/a]?"));
        assert!(is_confirmation_prompt("Stage this hunk [y,n,q,a,d,e,?]?"));
        assert!(is_confirmation_prompt("really? y/n"));
        assert!(!is_confirmation_prompt("just some output"));
        assert!(!is_confirmation_prompt("the year was 1999"));
        assert!(!is_confirmation_prompt("pick a range [2/3]"));
    }

    #[test]
    fn confirmation_only_matches_last_line() {
        // An already-answered prompt scrolled up must not trigger.
        let scrollback = "Proceed? [y/n]\ny\nDone.";
        assert!(!is_confirmation_prompt(scrollback));
    }

    #[test]
    fn detects_trailing_prompt_verbatim() {
        assert!(line_ends_with_any("welcome\n> ", &["> ", "$ "]));
        assert!(line_ends_with_any("user@host:~$ ", &["$ "]));
        assert!(!line_ends_with_any("still running...", &["> ", "$ "]));
        assert!(!line_ends_with_any("", &["> "]));
        // Verbatim matching avoids over-matching ordinary text.
        assert!(!line_ends_with_any("let v: Vec<T>", &["> "]));
    }

    #[test]
    fn sanitizer_round_trips_multiline_output() {
        let mut s = Sanitizer::new();
        s.feed(b"\x1b[32mline one\x1b[0m\nline two\n> ");
        assert_eq!(s.clean_log(), "line one\nline two\n> ");
        assert_eq!(s.current_line(), "> ");
        assert!(line_ends_with_any(&s.clean_log(), &["> "]));
    }
}
