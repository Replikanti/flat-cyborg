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
//! implementation. It **does** handle scroll regions (DECSTBM) and scroll
//! up/down (SU/SD), so a TUI that scrolls content inside a margin (e.g. Claude
//! Code's alternate-screen view) evicts scrolled-off lines into the transcript
//! correctly. It does **not** yet handle: insert/delete line and character
//! (IL/DL/ICH/DCH/ECH), repeat (REP), or autowrap mode (DECAWM). Wide / CJK /
//! emoji characters are counted as a single cell, so absolute addressing can
//! drift on screens that use them. Apps that fully repaint each frame
//! (ratatui-style) render faithfully; incrementally-edited TUIs may show stale
//! glyphs. These families can be added as needed; richer fidelity would warrant
//! a purpose-built VT crate.

use crate::ansi::{Parser, Perform};

/// Upper bound on retained scrolled-off lines. Bounds memory for very long
/// streaming replies; oldest lines are dropped once this is exceeded.
const MAX_SCROLLBACK: usize = 10_000;

/// A fixed-size character grid with a cursor.
#[derive(Clone)]
struct Grid {
    rows: usize,
    cols: usize,
    cells: Vec<Vec<char>>,
    /// Lines that have scrolled off the top of the viewport, oldest first.
    /// Retained so the full transcript of a long reply can be reconstructed.
    scrollback: Vec<Vec<char>>,
    row: usize,
    col: usize,
    /// Top row of the scroll region (0-based, inclusive). Defaults to 0.
    scroll_top: usize,
    /// Bottom row of the scroll region (0-based, inclusive). Defaults to the
    /// last physical row. Set together via DECSTBM (`CSI t;b r`).
    scroll_bottom: usize,
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
            scrollback: Vec::new(),
            row: 0,
            col: 0,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            decsc: (0, 0),
        }
    }

    fn clear(&mut self) {
        for line in &mut self.cells {
            for cell in line.iter_mut() {
                *cell = ' ';
            }
        }
        self.scrollback.clear();
        self.row = 0;
        self.col = 0;
        // Entering the alternate screen resets the scroll region to full-screen.
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
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
        if self.row == self.scroll_bottom {
            self.scroll_region_up(1);
            true
        } else if self.row + 1 < self.rows {
            self.row += 1;
            false
        } else {
            false // below the scroll region at the physical bottom: clamp, no scroll
        }
    }

    /// Scrolls the scroll region up by `n` lines: each line leaving the top of the
    /// region is evicted into `scrollback` (so the transcript is retained), and a
    /// blank line appears at the bottom of the region. Rows outside the region and
    /// the grid length are unchanged.
    fn scroll_region_up(&mut self, n: usize) {
        let height = self.scroll_bottom - self.scroll_top + 1;
        for _ in 0..n.min(height) {
            let evicted = self.cells.remove(self.scroll_top);
            self.scrollback.push(evicted);
            if self.scrollback.len() > MAX_SCROLLBACK {
                self.scrollback.remove(0);
            }
            self.cells.insert(self.scroll_bottom, vec![' '; self.cols]);
        }
    }

    /// Scrolls the scroll region down by `n` lines: a blank line appears at the top
    /// of the region and the bottom region line is discarded. No eviction (the
    /// revealed lines are blank, not history).
    fn scroll_region_down(&mut self, n: usize) {
        let height = self.scroll_bottom - self.scroll_top + 1;
        for _ in 0..n.min(height) {
            self.cells.remove(self.scroll_bottom);
            self.cells.insert(self.scroll_top, vec![' '; self.cols]);
        }
    }

    /// DECSTBM (`CSI t;b r`): set the scroll region to rows `t..=b` (1-based input).
    /// Missing/`0` params default to the full screen. An invalid range resets to the
    /// full screen. Per the VT spec the cursor moves to the home position.
    fn set_scroll_region(&mut self, params: &[u16]) {
        let top = params.first().copied().unwrap_or(0);
        let bottom = params.get(1).copied().unwrap_or(0);
        let t = if top == 0 { 0 } else { (top - 1) as usize };
        let b = if bottom == 0 {
            self.rows - 1
        } else {
            (bottom - 1) as usize
        };
        if t < b && b < self.rows {
            self.scroll_top = t;
            self.scroll_bottom = b;
        } else {
            self.scroll_top = 0;
            self.scroll_bottom = self.rows - 1;
        }
        self.row = 0;
        self.col = 0;
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

    /// Renders the full transcript: scrolled-off lines followed by the current
    /// viewport, each trimmed of trailing spaces and trailing blank lines
    /// dropped. Unlike [`render`], this includes everything that scrolled away.
    ///
    /// The scrollback/viewport seam is reconciled: an app that scrolls its region
    /// with SU and then repaints the just-scrolled lines (e.g. Claude Code) stores
    /// the boundary line(s) twice — once evicted into `scrollback`, once repainted
    /// in the viewport. The longest suffix of `scrollback` that re-appears as the
    /// (leading-blank-skipped) prefix of the viewport is dropped. For ordinary
    /// line-fed streams the overlap is 0, so the transcript is unchanged. (If a
    /// reply genuinely repeats lines exactly at the seam, those collapse too — a
    /// rare, bounded over-merge, strictly better than a guaranteed duplicate.)
    fn render_full(&self) -> String {
        let render = |rows: &[Vec<char>]| -> Vec<String> {
            rows.iter()
                .map(|row| {
                    let s: String = row.iter().collect();
                    s.trim_end().to_string()
                })
                .collect()
        };
        let scrollback = render(&self.scrollback);
        let viewport = render(&self.cells);
        let vp_start = viewport
            .iter()
            .position(|l| !l.is_empty())
            .unwrap_or(viewport.len());
        let overlap = seam_overlap(&scrollback, &viewport[vp_start..]);
        let mut lines = scrollback;
        lines.extend_from_slice(&viewport[..vp_start]);
        lines.extend_from_slice(&viewport[vp_start + overlap..]);
        while lines.last().map(String::is_empty).unwrap_or(false) {
            lines.pop();
        }
        lines.join("\n")
    }
}

/// Longest `L` such that the last `L` lines of `sb` equal the first `L` lines of
/// `vp`. Used to reconcile the scrollback/viewport seam in [`Grid::render_full`].
fn seam_overlap(sb: &[String], vp: &[String]) -> usize {
    let max_l = sb.len().min(vp.len());
    for l in (1..=max_l).rev() {
        if sb[sb.len() - l..] == vp[..l] {
            return l;
        }
    }
    0
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

    /// The full transcript of the active grid, including lines that scrolled off
    /// the top of the viewport. Mirrors [`text`] in choosing primary vs
    /// alternate. Used by `--extract` to capture long multi-line replies.
    pub fn full_text(&self) -> String {
        if self.in_alternate {
            self.alternate.render_full()
        } else {
            self.primary.render_full()
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
            // DECSTBM and scroll up/down (SU/SD) — these change content.
            b'r' => self.active().set_scroll_region(params),
            b'S' => self.active().scroll_region_up(n),
            b'T' => self.active().scroll_region_down(n),
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

    #[test]
    fn full_text_retains_scrolled_off_lines() {
        // 2-row viewport; feed five lines so three scroll off the top.
        let mut s = Screen::new(2, 5);
        s.feed(b"a\r\nb\r\nc\r\nd\r\ne");
        // Viewport keeps only the last two lines.
        assert_eq!(s.text(), "d\ne");
        // Full transcript keeps every line in order.
        assert_eq!(s.full_text(), "a\nb\nc\nd\ne");
    }

    #[test]
    fn full_text_line_count_matches_feeds() {
        // Feed many more lines than the viewport has rows; all must be retained.
        let rows = 3;
        let n = 250;
        let mut s = Screen::new(rows as u16, 8);
        let mut input = String::new();
        for i in 0..n {
            if i > 0 {
                input.push_str("\r\n");
            }
            input.push_str(&format!("L{i}"));
        }
        s.feed(input.as_bytes());
        let full = s.full_text();
        assert_eq!(full.lines().count(), n);
        assert!(full.starts_with("L0\n"));
        assert!(full.ends_with(&format!("L{}", n - 1)));
        // Viewport shows only the last `rows` lines.
        assert_eq!(s.text().lines().count(), rows);
    }

    #[test]
    fn scrollback_cap_keeps_most_recent_without_panic() {
        // Feed far more lines than MAX_SCROLLBACK to exercise front-eviction.
        let total = MAX_SCROLLBACK + 50;
        let mut s = Screen::new(2, 6);
        let mut input = String::new();
        for i in 0..total {
            if i > 0 {
                input.push_str("\r\n");
            }
            input.push_str(&format!("n{i}"));
        }
        s.feed(input.as_bytes());
        let full = s.full_text();
        // Scrollback is capped, so the earliest lines are dropped, but the most
        // recent line is always present.
        assert!(full.ends_with(&format!("n{}", total - 1)));
        assert!(!full.contains("n0\n"));
        // Bounded: at most cap scrollback lines plus the viewport rows.
        assert!(full.lines().count() <= MAX_SCROLLBACK + 2);
    }

    #[test]
    fn decstbm_set_and_reset() {
        // 4-row screen, region rows 2..4 (1-based) → rows 1..=3 (0-based).
        let mut s = Screen::new(4, 6);
        s.feed(b"\x1b[2;4r");
        // Cursor homed by DECSTBM. Move to the region bottom (row 4, 1-based) and
        // paint, then line-feed: a scroll happens *within* the region, so the
        // fixed top row (row 1) is untouched.
        s.feed(b"\x1b[1;1Htop");
        s.feed(b"\x1b[4;1Hbot");
        // Line-feed at the region bottom scrolls the region up and evicts.
        assert!(s.feed(b"\n"));
        // The top margin row is still intact.
        assert!(
            s.text().starts_with("top"),
            "top margin lost: {:?}",
            s.text()
        );
        // "bot" scrolled up within the region.
        assert!(s.full_text().contains("bot"));

        // Reset to full screen and confirm scrolling now happens at the physical
        // bottom (the top row is evicted, not preserved).
        s.feed(b"\x1b[r");
        let mut s2 = Screen::new(2, 4);
        s2.feed(b"\x1b[r"); // reset on a fresh full-screen grid is a no-op
        s2.feed(b"aaaa\r\nbbbb\r\ncccc");
        // Same behaviour as the default full-screen region.
        assert_eq!(s2.text(), "bbbb\ncccc");
        assert_eq!(s2.full_text(), "aaaa\nbbbb\ncccc");
    }

    #[test]
    fn line_feed_scrolls_within_region_preserving_top_and_bottom_margins() {
        // 6-row screen. Region rows 2..4 (1-based) → rows 1..=3 (0-based). Row 1
        // (top margin) and row 6 (bottom margin) must stay fixed.
        let mut s = Screen::new(6, 6);
        s.feed(b"\x1b[2;4r");
        s.feed(b"\x1b[1;1HTOP"); // fixed top margin (above region)
        s.feed(b"\x1b[6;1HBOT"); // fixed bottom margin (below region)
                                 // Fill the region (rows 2,3,4) and scroll past its bottom.
        s.feed(b"\x1b[2;1Hr1\r\nr2\r\nr3\r\nr4\r\nr5");
        let text = s.text();
        // Margins survive untouched.
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.first().copied(), Some("TOP"), "text: {text:?}");
        // BOT is the last non-blank line of the screen (row 6, 0-based 5).
        assert_eq!(lines.get(5).copied(), Some("BOT"), "text: {text:?}");
        // The region scrolled: the latest content (r5) is visible, the earliest
        // (r1) scrolled out of the viewport but is retained in the transcript.
        assert!(text.contains("r5"), "region did not advance: {text:?}");
        assert!(!text.contains("r1"), "stale region line remained: {text:?}");
        assert!(
            s.full_text().contains("r1"),
            "region top not evicted to scrollback"
        );
    }

    #[test]
    fn scroll_region_up_evicts_top_line_into_scrollback() {
        // 4-row screen, small region rows 1..2 (1-based) → 0..=1.
        let mut s = Screen::new(4, 6);
        s.feed(b"\x1b[1;2r");
        s.feed(b"\x1b[1;1H");
        // Feed enough lines that region-top lines are evicted.
        s.feed(b"L0\r\nL1\r\nL2\r\nL3\r\nL4");
        // Viewport (region rows) shows only the recent two lines.
        let text = s.text();
        assert!(
            text.contains("L3"),
            "viewport missing recent line: {text:?}"
        );
        assert!(
            text.contains("L4"),
            "viewport missing recent line: {text:?}"
        );
        assert!(!text.contains("L0"), "old line still in viewport: {text:?}");
        // Full transcript retains every line in order.
        let full = s.full_text();
        let positions: Vec<Option<usize>> = (0..=4).map(|i| full.find(&format!("L{i}"))).collect();
        assert!(
            positions.iter().all(Option::is_some),
            "missing line: {full:?}"
        );
        for w in positions.windows(2) {
            assert!(w[0].unwrap() < w[1].unwrap(), "out of order: {full:?}");
        }
    }

    #[test]
    fn su_scrolls_region_up_and_evicts() {
        // Region rows 1..3 (1-based) → 0..=2 on a 4-row screen.
        let mut s = Screen::new(4, 6);
        s.feed(b"\x1b[1;3r");
        s.feed(b"\x1b[1;1Haaa");
        s.feed(b"\x1b[2;1Hbbb");
        s.feed(b"\x1b[3;1Hccc");
        // Explicit SU by 2: top two region lines evicted, blanks at region bottom.
        s.feed(b"\x1b[2S");
        // "ccc" rose to the region top; "aaa"/"bbb" evicted to scrollback.
        let text = s.text();
        assert!(text.starts_with("ccc"), "SU did not shift region: {text:?}");
        assert!(
            !text.contains("aaa"),
            "evicted line still visible: {text:?}"
        );
        let full = s.full_text();
        assert!(
            full.contains("aaa") && full.contains("bbb"),
            "eviction lost: {full:?}"
        );
        // A blank now sits at the region bottom (row 3, 0-based 2).
        let lines: Vec<&str> = text.lines().collect();
        assert_ne!(lines.get(2).copied(), Some("ccc"));
    }

    #[test]
    fn sd_scrolls_region_down_without_eviction() {
        // Region rows 1..3 (1-based) → 0..=2 on a 4-row screen.
        let mut s = Screen::new(4, 6);
        s.feed(b"\x1b[1;3r");
        s.feed(b"\x1b[1;1Haaa");
        s.feed(b"\x1b[2;1Hbbb");
        s.feed(b"\x1b[3;1Hccc");
        // Nothing has scrolled yet, so there is no scrollback.
        assert!(s.primary.scrollback.is_empty());
        // Explicit SD by 1: blank at region top, bottom region line (ccc) dropped.
        s.feed(b"\x1b[1T");
        let text = s.text();
        let lines: Vec<&str> = text.lines().collect();
        // Region top is now blank (so the first visible line is the old "aaa",
        // pushed down one row).
        assert_eq!(
            lines.first().copied(),
            Some(""),
            "no blank at region top: {text:?}"
        );
        assert_eq!(
            lines.get(1).copied(),
            Some("aaa"),
            "aaa not pushed down: {text:?}"
        );
        assert_eq!(
            lines.get(2).copied(),
            Some("bbb"),
            "bbb not pushed down: {text:?}"
        );
        // The bottom region line "ccc" was discarded entirely.
        assert!(
            !s.full_text().contains("ccc"),
            "ccc should be discarded: {:?}",
            s.full_text()
        );
        // SD does not evict, so the scrollback (transcript history) is unchanged —
        // still empty. The revealed top line is blank, not retained history.
        assert!(
            s.primary.scrollback.is_empty(),
            "SD must not evict into scrollback"
        );
    }

    #[test]
    fn seam_overlap_basic() {
        let v = |xs: &[&str]| -> Vec<String> { xs.iter().map(|s| s.to_string()).collect() };
        // No shared boundary lines → 0.
        assert_eq!(seam_overlap(&v(&["a", "b", "c"]), &v(&["x", "y"])), 0);
        // The single suffix line of `sb` equals the single prefix line of `vp` → 1.
        assert_eq!(seam_overlap(&v(&["a", "b", "c"]), &v(&["c", "d", "e"])), 1);
        // Two suffix lines equal two prefix lines → 2 (and the longest run wins).
        assert_eq!(seam_overlap(&v(&["a", "b", "c"]), &v(&["b", "c", "d"])), 2);
        // Empty inputs (either side) → 0.
        assert_eq!(seam_overlap(&[], &v(&["a"])), 0);
        assert_eq!(seam_overlap(&v(&["a"]), &[]), 0);
        assert_eq!(seam_overlap(&[], &[]), 0);
    }

    #[test]
    fn render_full_dedups_su_repaint_seam() {
        // Reproduce Claude Code's SU-then-repaint shape and force a boundary line to
        // land in BOTH scrollback (evicted by SU) and the viewport (repainted), then
        // assert the seam line survives exactly once in full_text(), in order.
        let mut s = Screen::new(6, 16);
        s.feed(b"\x1b[?1049h\x1b[2J\x1b[H");
        s.feed(b"\x1b[2;5r"); // region rows 2..5 (1-based) => 0-based 1..=4
        let top = 2u16; // 1-based region top
        let bottom = 5u16; // 1-based region bottom
        let height = (bottom - top + 1) as usize; // 4
        let k = 8usize;
        // Repaint the whole visible window: rows top..=bottom show lines
        // (last-height+1)..=last, blanking lines first like a real frame redraw.
        let repaint_window = |s: &mut Screen, last: isize| {
            for r in 0..height {
                let line_no = last - (height as isize - 1) + r as isize;
                s.feed(format!("\x1b[{};1H", top + r as u16).as_bytes());
                s.feed(b"\x1b[2K");
                if line_no >= 1 {
                    s.feed(format!("L{line_no}").as_bytes());
                }
            }
        };
        // Advancing stream: each new line does SU by 1 (evicting the region-top line
        // into scrollback) then repaints the window. Eviction and repaint stay
        // aligned, so no duplicate yet.
        for i in 1..=k {
            s.feed(b"\x1b[1S");
            repaint_window(&mut s, i as isize);
        }
        // Over-scroll on the final frame: one more SU evicts the current viewport
        // top (line k-height+1) into scrollback, but the app repaints the SAME
        // window (content did not advance), so that very line reappears at the
        // viewport top — exactly the scrollback/viewport seam duplicate Claude emits.
        s.feed(b"\x1b[1S");
        repaint_window(&mut s, k as isize);

        let full = s.full_text();
        let lines: Vec<&str> = full.lines().collect();
        // Every streamed line is present, in order, and exactly once.
        for i in 1..=k {
            let label = format!("L{i}");
            let count = lines.iter().filter(|l| **l == label).count();
            assert_eq!(
                count, 1,
                "line {label} should appear exactly once: {full:?}"
            );
        }
        let positions: Vec<usize> = (1..=k)
            .map(|i| lines.iter().position(|l| **l == format!("L{i}")).unwrap())
            .collect();
        for w in positions.windows(2) {
            assert!(w[0] < w[1], "lines out of order: {full:?}");
        }
        // No adjacent duplicate *content* line survived the seam (blank lines may
        // legitimately repeat, e.g. region padding, so they are excluded).
        for pair in lines.windows(2) {
            if !pair[0].is_empty() {
                assert_ne!(pair[0], pair[1], "adjacent duplicate at seam: {full:?}");
            }
        }
    }

    #[test]
    fn render_full_unchanged_without_overlap() {
        // A plain CRLF stream of distinct lines longer than the viewport: the
        // scrollback/viewport seam has no overlap, so full_text() must equal the
        // naive `scrollback ++ viewport` join (every line retained, overlap 0).
        let rows = 3usize;
        let n = 12usize;
        let mut s = Screen::new(rows as u16, 8);
        let mut input = String::new();
        for i in 0..n {
            if i > 0 {
                input.push_str("\r\n");
            }
            input.push_str(&format!("row{i}"));
        }
        s.feed(input.as_bytes());

        // Build the naive join directly from the raw grid, with the same per-line
        // trim and trailing-blank-drop, but WITHOUT seam reconciliation.
        let render = |rows: &[Vec<char>]| -> Vec<String> {
            rows.iter()
                .map(|r| r.iter().collect::<String>().trim_end().to_string())
                .collect()
        };
        let mut naive: Vec<String> = render(&s.primary.scrollback);
        naive.extend(render(&s.primary.cells));
        while naive.last().map(String::is_empty).unwrap_or(false) {
            naive.pop();
        }
        assert_eq!(s.full_text(), naive.join("\n"));
        // Sanity: all n distinct lines are present (no accidental over-merge).
        assert_eq!(s.full_text().lines().count(), n);
    }

    #[test]
    fn claude_like_scroll_region_stream_is_fully_captured() {
        // Faithfully reproduce Claude Code's scrolling shape: it scrolls the
        // region with `CSI <n>S` (SU) and repaints each new line at the region's
        // bottom row with absolute addressing — it does NOT line-feed (`\n`) the
        // content through. The pre-fix emulator ignored `CSI S`, so every repaint
        // overwrote the same row and the earlier lines (and the BEGIN sentinel)
        // were lost; this test therefore FAILS on the old code path and is a true
        // regression guard, not just a `full_text()` tautology.
        let rows: u16 = 6;
        let mut s = Screen::new(rows, 16);
        s.feed(b"\x1b[?1049h\x1b[2J\x1b[H");
        // Region rows 2..5 (1-based) → 0-based 1..=4, height 4 (< stream length).
        s.feed(b"\x1b[2;5r");

        // Painted lines, in order: a leading sentinel, K numbers, a trailing
        // sentinel — each delivered the way Claude does it (SU, then paint at the
        // region's bottom row), never via `\n`.
        let k = 20;
        let mut painted: Vec<String> = Vec::with_capacity(k + 2);
        painted.push("FCB_TST_BEGIN".to_string());
        painted.extend((1..=k).map(|i| i.to_string()));
        painted.push("FCB_TST_END".to_string());
        for line in &painted {
            s.feed(b"\x1b[1S"); // SU: evict the region-top line into scrollback
            s.feed(b"\x1b[5;1H"); // cursor to the region bottom (1-based row 5)
            s.feed(line.as_bytes());
        }

        // full_text() = scrollback + viewport must reconstruct the whole stream.
        let full = s.full_text();
        assert!(
            full.contains("FCB_TST_BEGIN"),
            "BEGIN sentinel lost: {full:?}"
        );
        assert!(full.contains("FCB_TST_END"), "END sentinel lost: {full:?}");
        // Every number 1..=K present and in order, with the sentinels bracketing.
        let mut cursor = full.find("FCB_TST_BEGIN").unwrap();
        for i in 1..=k {
            let needle = format!("\n{i}\n");
            // Search a padded copy so first/last numbers match the \n…\n shape.
            let padded = format!("\n{full}\n");
            let pos = padded[cursor..]
                .find(&needle)
                .unwrap_or_else(|| panic!("missing line {i} in order: {full:?}"));
            cursor += pos + 1;
        }
        let begin_at = full.find("FCB_TST_BEGIN").unwrap();
        let end_at = full.find("FCB_TST_END").unwrap();
        assert!(begin_at < end_at, "sentinels out of order: {full:?}");

        // The scrollback/viewport seam must not leave an adjacent duplicate: no two
        // consecutive *content* transcript lines are identical (a SU-repaint
        // boundary line stored in both scrollback and viewport would otherwise show
        // up twice). Blank lines may legitimately repeat, so they are excluded.
        let captured: Vec<&str> = full.lines().collect();
        for pair in captured.windows(2) {
            if !pair[0].is_empty() {
                assert_ne!(
                    pair[0], pair[1],
                    "adjacent duplicate line at seam: {full:?}"
                );
            }
        }

        // The viewport (text()) tracks the most-recent lines: the trailing
        // sentinel is visible and the leading one has scrolled off.
        let view = s.text();
        assert!(
            view.contains("FCB_TST_END"),
            "END not in viewport: {view:?}"
        );
        assert!(
            !view.contains("FCB_TST_BEGIN"),
            "BEGIN should have scrolled off the viewport: {view:?}"
        );
    }
}
