use super::parser::{Parser, Perform};

/// Upper bound on the addressable column, bounding the per-line allocation so a
/// malicious cursor-forward sequence (`ESC[<huge>C`) cannot exhaust memory.
const MAX_COL: usize = 1 << 16;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::line_ends_with_any;

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
    fn sanitizer_round_trips_multiline_output() {
        let mut s = Sanitizer::new();
        s.feed(b"\x1b[32mline one\x1b[0m\nline two\n> ");
        assert_eq!(s.clean_log(), "line one\nline two\n> ");
        assert_eq!(s.current_line(), "> ");
        assert!(line_ends_with_any(&s.clean_log(), &["> "]));
    }
}
