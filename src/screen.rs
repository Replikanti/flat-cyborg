//! 2D screen-grid terminal emulator for full-screen TUI applications.
//!
//! [`crate::ansi::Sanitizer`] flattens a line-oriented stream and is ideal for
//! shells and REPLs. Full-screen TUIs (e.g. Claude Code's Ink UI) instead use
//! the *alternate screen buffer* and absolute cursor addressing — they paint a
//! 2D screen and repaint regions in place. Flattening that to lines produces
//! noise.
//!
//! [`Screen`] is a small fixed-size grid emulator that interprets cursor
//! movement, line/screen erasure, and the alternate-screen switch, so the
//! visible screen can be rendered as clean text. It is driven by the same
//! [`Parser`](crate::ansi) used elsewhere.
//!
//! # Scope and limitations
//!
//! This is intentionally a *partial* terminal emulator — enough to capture
//! full-repaint and common cursor-addressed TUIs, not a complete VT
//! implementation. It does **not** yet handle: scroll regions (DECSTBM),
//! insert/delete line and character (IL/DL/ICH/DCH/ECH), repeat (REP), or
//! autowrap mode (DECAWM). Wide / CJK / emoji characters are counted as a
//! single cell, so absolute addressing can drift on screens that use them.
//! Apps that fully repaint each frame (ratatui-style) render faithfully;
//! incrementally-edited or scroll-region TUIs may show stale glyphs. These
//! families can be added as needed; richer fidelity would warrant a
//! purpose-built VT crate.

use crate::ansi::{Parser, Perform};

/// A fixed-size character grid with a cursor.
#[derive(Clone)]
struct Grid {
    rows: usize,
    cols: usize,
    cells: Vec<Vec<char>>,
    row: usize,
    col: usize,
    /// Saved cursor for DECSC/DECRC (`ESC[s` / `ESC[u`), per buffer — kept
    /// separate from the alternate-screen save so they cannot clobber each
    /// other.
    decsc: (usize, usize),
}

impl Grid {
    fn new(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            cells: vec![vec![' '; cols]; rows],
            row: 0,
            col: 0,
            decsc: (0, 0),
        }
    }

    fn clear(&mut self) {
        for line in &mut self.cells {
            for cell in line.iter_mut() {
                *cell = ' ';
            }
        }
        self.row = 0;
        self.col = 0;
    }

    fn put(&mut self, c: char) {
        if self.col >= self.cols {
            // Wrap to the next line.
            self.col = 0;
            self.line_feed();
        }
        // `row`/`col` are kept in bounds by the movement handlers, but guard
        // defensively against a malformed stream.
        if self.row < self.rows && self.col < self.cols {
            self.cells[self.row][self.col] = c;
        }
        self.col += 1;
    }

    /// Moves the cursor down one row, scrolling if at the bottom. Returns
    /// `true` if a scroll occurred (a visible content change).
    fn line_feed(&mut self) -> bool {
        if self.row + 1 >= self.rows {
            // Scroll up: drop the top line, append a blank bottom line.
            self.cells.remove(0);
            self.cells.push(vec![' '; self.cols]);
            true
        } else {
            self.row += 1;
            false
        }
    }

    fn carriage_return(&mut self) {
        self.col = 0;
    }

    fn tab(&mut self) {
        self.col = ((self.col / 8 + 1) * 8).min(self.cols.saturating_sub(1));
    }

    fn backspace(&mut self) {
        self.col = self.col.saturating_sub(1);
    }

    fn move_to(&mut self, row: usize, col: usize) {
        self.row = row.min(self.rows.saturating_sub(1));
        self.col = col.min(self.cols.saturating_sub(1));
    }

    /// Erase in line: 0 = cursor→end, 1 = start→cursor, 2 = whole line.
    fn erase_line(&mut self, mode: u16) {
        if self.row >= self.rows {
            return;
        }
        let (lo, hi) = match mode {
            1 => (0, (self.col + 1).min(self.cols)),
            2 => (0, self.cols),
            _ => (self.col.min(self.cols), self.cols),
        };
        for cell in self.cells[self.row][lo..hi].iter_mut() {
            *cell = ' ';
        }
    }

    /// Erase in display: 0 = cursor→end, 1 = start→cursor, 2/3 = whole screen.
    fn erase_display(&mut self, mode: u16) {
        match mode {
            2 | 3 => {
                for line in &mut self.cells {
                    for cell in line.iter_mut() {
                        *cell = ' ';
                    }
                }
            }
            1 => {
                for r in 0..self.row.min(self.rows) {
                    for cell in self.cells[r].iter_mut() {
                        *cell = ' ';
                    }
                }
                self.erase_line(1);
            }
            _ => {
                self.erase_line(0);
                for r in (self.row + 1)..self.rows {
                    for cell in self.cells[r].iter_mut() {
                        *cell = ' ';
                    }
                }
            }
        }
    }

    /// Renders the grid as text: trailing spaces trimmed per line, trailing
    /// blank lines dropped.
    fn render(&self) -> String {
        let mut lines: Vec<String> = self
            .cells
            .iter()
            .map(|row| {
                let s: String = row.iter().collect();
                s.trim_end().to_string()
            })
            .collect();
        while lines.last().map(String::is_empty).unwrap_or(false) {
            lines.pop();
        }
        lines.join("\n")
    }
}

/// A screen-grid terminal emulator.
///
/// Feed raw PTY bytes with [`Screen::feed`]; read the visible screen with
/// [`Screen::text`]. The alternate screen buffer is tracked, so for a TUI in
/// alternate-screen mode, `text()` reflects the TUI's current screen.
pub struct Screen {
    parser: Parser,
    primary: Grid,
    alternate: Grid,
    in_alternate: bool,
    saved_cursor: (usize, usize),
}

impl Screen {
    /// Creates a screen of the given geometry.
    pub fn new(rows: u16, cols: u16) -> Self {
        let rows = rows.max(1) as usize;
        let cols = cols.max(1) as usize;
        Self {
            parser: Parser::default(),
            primary: Grid::new(rows, cols),
            alternate: Grid::new(rows, cols),
            in_alternate: false,
            saved_cursor: (0, 0),
        }
    }

    fn active(&mut self) -> &mut Grid {
        if self.in_alternate {
            &mut self.alternate
        } else {
            &mut self.primary
        }
    }

    /// Feeds a chunk of raw bytes, returning `true` if the *visible content*
    /// changed.
    ///
    /// Change is detected by diffing a hash of the screen cells before and
    /// after, not by "did any write happen". This is what makes settle
    /// detection robust: a TUI that repaints identical content on a timer
    /// (clocks, progress bars, same-frame redraws) or merely moves/blinks the
    /// cursor is correctly reported as unchanged, so it can settle.
    pub fn feed(&mut self, input: &[u8]) -> bool {
        let before = self.content_hash();
        // Drive the parser; it calls back into `self` via `Perform`. Use a
        // temporary to satisfy the borrow checker (parser is a field).
        let mut parser = std::mem::take(&mut self.parser);
        for &b in input {
            parser.advance(self, b);
        }
        self.parser = parser;
        self.content_hash() != before
    }

    /// Hash of the visible screen's cells. The cursor position is deliberately
    /// excluded, so cursor motion and blinking do not count as a change.
    fn content_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let grid = if self.in_alternate {
            &self.alternate
        } else {
            &self.primary
        };
        for row in &grid.cells {
            for c in row {
                c.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    /// The current visible screen rendered as text.
    pub fn text(&self) -> String {
        if self.in_alternate {
            self.alternate.render()
        } else {
            self.primary.render()
        }
    }
}

impl Perform for Screen {
    fn print(&mut self, c: char) {
        self.active().put(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.active().line_feed();
            }
            b'\r' => self.active().carriage_return(),
            0x08 => self.active().backspace(),
            b'\t' => self.active().tab(),
            _ => {}
        }
    }

    fn csi(&mut self, params: &[u16], private: Option<u8>, final_byte: u8) {
        // Alternate-screen switch: ESC[?1049h / ?47h / ?1047h (and the `l`
        // variants to leave). Only ?1049 saves/restores the cursor; ?1049 and
        // ?1047 clear the alternate buffer on entry; ?47 is a plain switch.
        if private == Some(b'?') {
            if let (Some(&p), true) = (params.first(), matches!(final_byte, b'h' | b'l')) {
                if matches!(p, 1049 | 47 | 1047) {
                    let enter = final_byte == b'h';
                    if enter && !self.in_alternate {
                        if p == 1049 {
                            self.saved_cursor = (self.primary.row, self.primary.col);
                        }
                        if p == 1049 || p == 1047 {
                            self.alternate.clear();
                        }
                        self.in_alternate = true;
                    } else if !enter && self.in_alternate {
                        self.in_alternate = false;
                        if p == 1049 {
                            let (r, c) = self.saved_cursor;
                            self.primary.move_to(r, c);
                        }
                    }
                }
            }
            return; // other private modes (cursor visibility, etc.) are ignored
        }

        let p0 = params.first().copied().unwrap_or(0);
        let n = p0.max(1) as usize;
        match final_byte {
            // Cursor moves — no content change.
            b'A' => {
                let g = self.active();
                g.row = g.row.saturating_sub(n);
            }
            b'B' => {
                let g = self.active();
                g.row = (g.row + n).min(g.rows - 1);
            }
            b'C' => {
                let g = self.active();
                g.col = (g.col + n).min(g.cols - 1);
            }
            b'D' => {
                let g = self.active();
                g.col = g.col.saturating_sub(n);
            }
            b'E' => {
                let g = self.active();
                g.col = 0;
                g.row = (g.row + n).min(g.rows - 1);
            }
            b'F' => {
                let g = self.active();
                g.col = 0;
                g.row = g.row.saturating_sub(n);
            }
            b'G' => self.active().col = (p0.max(1) as usize - 1).min(self.active_cols() - 1),
            b'd' => self.active().row = (p0.max(1) as usize - 1).min(self.active_rows() - 1),
            b'H' | b'f' => {
                let row = params.first().copied().unwrap_or(1).max(1) as usize - 1;
                let col = params.get(1).copied().unwrap_or(1).max(1) as usize - 1;
                self.active().move_to(row, col);
            }
            // DECSC/DECRC: per-buffer cursor save/restore (distinct from the
            // alternate-screen save), no content change.
            b's' => {
                let g = self.active();
                g.decsc = (g.row, g.col);
            }
            b'u' => {
                let (r, c) = self.active().decsc;
                self.active().move_to(r, c);
            }
            b'J' => self.active().erase_display(p0),
            b'K' => self.active().erase_line(p0),
            // SGR and anything else affect rendering only.
            _ => {}
        }
    }
}

impl Screen {
    fn active_rows(&self) -> usize {
        if self.in_alternate {
            self.alternate.rows
        } else {
            self.primary.rows
        }
    }
    fn active_cols(&self) -> usize {
        if self.in_alternate {
            self.alternate.cols
        } else {
            self.primary.cols
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_cursor_positioning_paints_a_grid() {
        let mut s = Screen::new(5, 20);
        // CUP to row 2 col 5 (1-based), write "hi".
        s.feed(b"\x1b[2;5Hhi");
        // CUP to row 4 col 1, write "bye".
        s.feed(b"\x1b[4;1Hbye");
        assert_eq!(s.text(), "\n    hi\n\nbye");
    }

    #[test]
    fn erase_display_clears_the_screen() {
        let mut s = Screen::new(4, 10);
        s.feed(b"line1\r\nline2");
        s.feed(b"\x1b[2J\x1b[Hfresh");
        assert_eq!(s.text(), "fresh");
    }

    #[test]
    fn overwrite_in_place_via_cursor_address() {
        let mut s = Screen::new(3, 10);
        s.feed(b"hello");
        // Move cursor to col 1 (CHA) and overwrite the first char.
        s.feed(b"\x1b[1GJ");
        assert_eq!(s.text(), "Jello");
    }

    #[test]
    fn alternate_screen_is_isolated_from_primary() {
        let mut s = Screen::new(4, 20);
        s.feed(b"primary content");
        // Enter alternate screen, paint something else.
        s.feed(b"\x1b[?1049h\x1b[2J\x1b[Halt screen");
        assert_eq!(s.text(), "alt screen");
        // Leave alternate screen; primary content is restored.
        s.feed(b"\x1b[?1049l");
        assert_eq!(s.text(), "primary content");
    }

    #[test]
    fn line_wrap_and_scroll() {
        let mut s = Screen::new(2, 4);
        // Three lines of 4 cols into a 2-row screen: the first scrolls off.
        s.feed(b"aaaa\r\nbbbb\r\ncccc");
        assert_eq!(s.text(), "bbbb\ncccc");
    }

    #[test]
    fn erase_line_to_end_after_cursor_move() {
        let mut s = Screen::new(2, 12);
        // Write 0-9, return, forward 5 columns, overwrite "XX", erase to end.
        s.feed(b"0123456789\r\x1b[5CXX\x1b[K");
        // Cols 0-4 keep "01234", cols 5-6 become "XX", cols 7-9 erased.
        assert_eq!(s.text(), "01234XX");
    }

    #[test]
    fn change_flag_tracks_visible_updates() {
        let mut s = Screen::new(3, 10);
        assert!(s.feed(b"x"));
        assert!(!s.feed(b""));
    }

    #[test]
    fn content_diff_ignores_cursor_only_and_idempotent_repaints() {
        let mut s = Screen::new(5, 20);
        assert!(s.feed(b"hello")); // content appeared
        assert!(!s.feed(b"\r")); // CR — cursor only
        assert!(!s.feed(b"\x1b[2C")); // CUF — cursor only
        assert!(!s.feed(b"\x08")); // BS — cursor only
        // Repainting the exact same content is NOT a change (lets a
        // timer-repaint TUI settle).
        assert!(!s.feed(b"\x1b[1;1Hhello"));
        // Erasing the line that holds content IS a change.
        assert!(s.feed(b"\x1b[1;1H\x1b[2K"));
        // Erasing an already-blank line is NOT a change.
        assert!(!s.feed(b"\x1b[3;1H\x1b[2K"));
    }

    #[test]
    fn decsc_does_not_clobber_alternate_screen_save() {
        let mut s = Screen::new(5, 20);
        // Position the primary cursor and remember where the alt-screen save
        // should restore it to.
        s.feed(b"\x1b[3;7Hseed");
        // Enter alt screen (saves primary cursor), then the alt TUI uses
        // DECSC/DECRC for its own purposes.
        s.feed(b"\x1b[?1049h\x1b[1;1H\x1b[s\x1b[5;5HX\x1b[u");
        // Leaving alt screen must restore the *primary* cursor (after "seed",
        // col 10), not the alt buffer's DECSC slot.
        s.feed(b"\x1b[?1049lZ");
        // Z lands where "seed" left the primary cursor: row 2 (0-based), col 10
        // (directly after the 'd' at col 9).
        let line = s.text().lines().nth(2).unwrap_or("").to_string();
        assert_eq!(line, "      seedZ");
    }

    #[test]
    fn strips_sgr_colors() {
        let mut s = Screen::new(2, 20);
        s.feed(b"\x1b[31mred\x1b[0m text");
        assert_eq!(s.text(), "red text");
    }
}
