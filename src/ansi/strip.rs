use super::parser::{Parser, Perform};

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
}
