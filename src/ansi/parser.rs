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
