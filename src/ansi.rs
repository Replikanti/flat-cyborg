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
//! State detection helpers ([`is_confirmation_prompt`], [`line_ends_with_any`])
//! operate on the sanitized stream. The temporal half of state detection
//! (RUNNING vs. IDLE, which depends on a period of silence) lives in the
//! wrapper, since it requires a clock.

/// Sink for the structural events produced by [`Parser`].
trait Perform {
    /// A printable character (already UTF-8 decoded).
    fn print(&mut self, c: char);
    /// A C0 control byte (`\n`, `\r`, `\t`, backspace, ...).
    fn execute(&mut self, byte: u8);
    /// A completed CSI sequence: numeric parameters and the final byte.
    fn csi(&mut self, params: &[u16], final_byte: u8);
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

/// A compact, allocation-light ANSI/VT byte parser.
///
/// Feed it bytes one at a time via [`Parser::advance`]; it decodes UTF-8,
/// recognizes CSI/OSC/other escape sequences, and reports structural events to
/// a [`Perform`] sink. Parsing state (including partial sequences and partial
/// UTF-8 code points) is retained across calls, so it is safe to feed the
/// stream in arbitrary chunks.
#[derive(Debug)]
struct Parser {
    state: PState,
    params: Vec<u16>,
    cur: u32,
    saw_digit: bool,
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
    }

    /// Emits whatever has accumulated in the UTF-8 buffer, substituting the
    /// replacement character if the bytes do not form a valid code point.
    fn flush_utf8<P: Perform>(&mut self, perform: &mut P) {
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
    fn advance<P: Perform>(&mut self, perform: &mut P, byte: u8) {
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

        match self.state {
            PState::Ground => self.ground(perform, byte),
            PState::Esc => self.escape(byte),
            PState::EscIntermediate => self.state = PState::Ground,
            PState::Csi => self.csi(perform, byte),
            PState::OscString => match byte {
                0x07 => self.state = PState::Ground, // BEL terminates OSC
                0x1B => self.state = PState::OscStringEsc,
                _ => {}
            },
            PState::OscStringEsc => {
                // ESC `\` is the String Terminator; any other byte aborts.
                self.state = PState::Ground;
            }
        }
    }

    fn ground<P: Perform>(&mut self, perform: &mut P, byte: u8) {
        match byte {
            0x1B => self.state = PState::Esc,
            // C0 controls and DEL are executed, not printed.
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

    fn escape(&mut self, byte: u8) {
        self.reset_params();
        match byte {
            b'[' => self.state = PState::Csi,
            // OSC and the other string-style sequences (DCS/SOS/PM/APC) all
            // run until BEL or ST; treat them uniformly.
            b']' | b'P' | b'X' | b'^' | b'_' => self.state = PState::OscString,
            // Intermediate byte (e.g. charset designators `ESC ( B`): one more
            // byte follows before the sequence completes.
            0x20..=0x2F => self.state = PState::EscIntermediate,
            // Any other byte is a complete two-byte escape; drop it.
            _ => self.state = PState::Ground,
        }
    }

    fn csi<P: Perform>(&mut self, perform: &mut P, byte: u8) {
        match byte {
            0x30..=0x39 => {
                self.cur = (self.cur * 10 + u32::from(byte - b'0')).min(u32::from(u16::MAX));
                self.saw_digit = true;
            }
            b';' => {
                self.params.push(self.cur as u16);
                self.cur = 0;
                self.saw_digit = false;
            }
            // Private markers ('<' '=' '>' '?') and intermediates: ignored.
            0x3C..=0x3F | 0x20..=0x2F => {}
            // Final byte completes the sequence.
            0x40..=0x7E => {
                if self.saw_digit {
                    self.params.push(self.cur as u16);
                }
                let params = std::mem::take(&mut self.params);
                perform.csi(&params, byte);
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
    fn csi(&mut self, _params: &[u16], _final_byte: u8) {}
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
}

/// Removes ANSI escape sequences from `input` in a single call.
///
/// Equivalent in intent to the spec's `\x1B\[[0-9;]*[a-zA-Z]` strip, but also
/// strips OSC and other escape forms. All non-escape bytes are preserved.
pub fn strip_ansi(input: &str) -> String {
    AnsiStripper::new().feed(input.as_bytes())
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
        self.changed = true;
    }
}

impl Perform for Canvas {
    fn print(&mut self, c: char) {
        self.write(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.commit_line(),
            b'\r' => {
                self.col = 0;
                self.changed = true;
            }
            0x08 => {
                self.col = self.col.saturating_sub(1);
                self.changed = true;
            }
            b'\t' => {
                self.col = (self.col / 8 + 1) * 8;
                self.changed = true;
            }
            _ => {}
        }
    }

    fn csi(&mut self, params: &[u16], final_byte: u8) {
        let first = params.first().copied().unwrap_or(0);
        match final_byte {
            // Cursor horizontal absolute (1-based).
            b'G' => self.col = first.max(1) as usize - 1,
            // Cursor forward / back.
            b'C' => self.col += first.max(1) as usize,
            b'D' => self.col = self.col.saturating_sub(first.max(1) as usize),
            // Erase in line: 0 = to end, 1 = to start, 2 = whole line.
            b'K' => {
                match first {
                    0 => self.line.truncate(self.col),
                    1 => {
                        self.pad_to_col();
                        for cell in self.line.iter_mut().take(self.col) {
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
    /// wrapper to distinguish the RUNNING state from silence).
    pub fn feed(&mut self, input: &[u8]) -> bool {
        self.canvas.changed = false;
        for &b in input {
            self.parser.advance(&mut self.canvas, b);
        }
        self.canvas.changed
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
}

// ---------------------------------------------------------------------------
// Detection primitives.
// ---------------------------------------------------------------------------

/// Confirmation-prompt shapes recognized in sanitized output, lowercased.
const CONFIRMATION_PATTERNS: &[&str] =
    &["[y/n]", "(y/n)", "[yes/no]", "(yes/no)", "y/n?", "yes/no?"];

/// Returns `true` if `text` contains a yes/no confirmation prompt such as
/// `[y/n]`, `(Y/n)`, or `[yes/no]` (case-insensitive).
pub fn is_confirmation_prompt(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    CONFIRMATION_PATTERNS.iter().any(|p| lower.contains(p))
}

/// Returns `true` if the last non-empty line of `text` ends with any of the
/// given prompt tokens, comparing both sides with trailing whitespace removed.
///
/// Used to recognize a Target CLI's trailing prompt (e.g. `> `, `$ `). Trailing
/// whitespace is normalized away on both the line and the token so that a
/// prompt token like `"> "` still matches a line rendered as `">"`.
pub fn line_ends_with_any(text: &str, tokens: &[&str]) -> bool {
    let Some(line) = text.lines().rev().find(|l| !l.trim().is_empty()) else {
        return false;
    };
    let line = line.trim_end();
    tokens.iter().any(|t| {
        let token = t.trim_end();
        !token.is_empty() && line.ends_with(token)
    })
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
    fn strips_cursor_and_erase_sequences() {
        let input = "loading\x1b[2K\x1b[1Gdone\x1b[K";
        assert_eq!(strip_ansi(input), "loadingdone");
    }

    #[test]
    fn strips_osc_title_sequence() {
        // OSC 0 ; title BEL  — set window title.
        let input = "\x1b]0;my title\x07hello";
        assert_eq!(strip_ansi(input), "hello");
        // OSC terminated by ST (ESC \) instead of BEL.
        let st = "\x1b]0;title\x1b\\world";
        assert_eq!(strip_ansi(st), "world");
    }

    #[test]
    fn preserves_control_and_utf8_bytes() {
        let input = "a\tb\r\nčでrest";
        assert_eq!(strip_ansi(input), "a\tb\r\nčでrest");
    }

    #[test]
    fn handles_escape_split_across_chunks() {
        let mut s = AnsiStripper::new();
        // The CSI sequence is split mid-way between two feeds.
        let mut out = s.feed(b"foo\x1b[3");
        out.push_str(&s.feed(b"1mbar"));
        assert_eq!(out, "foobar");
    }

    #[test]
    fn sanitizer_collapses_carriage_return_spinner() {
        let mut s = Sanitizer::new();
        // A real spinner returns to column 0 with `\r`, rewrites the frame, and
        // erases any leftover from a longer previous frame with `\x1b[K`.
        s.feed(b"\r\x1b[36m|\x1b[0m working...\x1b[K");
        s.feed(b"\r\x1b[36m/\x1b[0m working...\x1b[K");
        s.feed(b"\r\x1b[36m-\x1b[0m working...\x1b[K");
        s.feed(b"\rdone!\x1b[K\n");
        // Only the final committed line survives; no spinner frames remain and
        // the wider previous frames leave no trailing artifacts.
        assert_eq!(s.clean_log(), "done!\n");
    }

    #[test]
    fn sanitizer_applies_erase_line_after_carriage_return() {
        let mut s = Sanitizer::new();
        // Longer frame, then a shorter frame after \r + erase-to-end.
        s.feed(b"a long status line\r");
        s.feed(b"short\x1b[K\n");
        assert_eq!(s.clean_log(), "short\n");
    }

    #[test]
    fn sanitizer_handles_backspace() {
        let mut s = Sanitizer::new();
        // Backspace moves the cursor left (it does not delete); the next write
        // overwrites in place, as a real terminal would render it.
        s.feed(b"abc\x08X");
        assert_eq!(s.current_line(), "abX");

        // The common erase idiom `\b \b` blanks the last character, leaving a
        // space in that cell (the cursor ends to its left).
        let mut s2 = Sanitizer::new();
        s2.feed(b"abc\x08 \x08");
        assert_eq!(s2.current_line(), "ab ");
    }

    #[test]
    fn sanitizer_tracks_change_flag() {
        let mut s = Sanitizer::new();
        assert!(s.feed(b"output"));
        assert!(!s.feed(b""));
    }

    #[test]
    fn detects_confirmation_prompts() {
        assert!(is_confirmation_prompt("Proceed? [y/n]"));
        assert!(is_confirmation_prompt("Overwrite file? (Y/n)"));
        assert!(is_confirmation_prompt("Delete all? (y/N) "));
        assert!(is_confirmation_prompt("Continue [yes/no]"));
        assert!(!is_confirmation_prompt("just some output"));
        assert!(!is_confirmation_prompt("the year was 1999"));
    }

    #[test]
    fn detects_trailing_prompt() {
        assert!(line_ends_with_any("welcome\n> ", &["> ", "$ "]));
        assert!(line_ends_with_any("user@host:~$  ", &["$"]));
        assert!(!line_ends_with_any("still running...", &["> ", "$ "]));
        assert!(!line_ends_with_any("", &["> "]));
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
