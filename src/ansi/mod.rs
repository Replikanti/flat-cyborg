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

mod detect;
mod parser;
mod sanitize;
mod strip;

pub use detect::{is_approval_menu, is_confirmation_prompt, line_ends_with_any};
pub(crate) use parser::{Parser, Perform};
pub use sanitize::Sanitizer;
pub use strip::{strip_ansi, AnsiStripper};
