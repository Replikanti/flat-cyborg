//! Input jittering layer (human input emulation).
//!
//! Commands are not written to the PTY master as a single bulk block (which can
//! flood the input buffer and trip anomaly detection in the Target CLI).
//! Instead, each command is decomposed into individual UTF-8 characters, and a
//! pseudo-random delay is inserted before each one:
//!
//! - standard alphanumeric characters: 40–120 ms
//! - punctuation and word separators (space, `.`, `,`, `?`, ...): 150–300 ms
//!
//! The command is terminated with a carriage return (`\r`), not a line feed.
//!
//! Timing only needs to look human, not be cryptographically random, so a tiny
//! self-contained PRNG is used rather than pulling in an RNG dependency. The
//! plan ([`Jitter::plan`]) is pure and testable; [`Jitter::type_command`]
//! executes it, sleeping between keystrokes.

use std::time::Duration;

/// Inclusive delay range (ms) for standard alphanumeric characters.
pub const ALNUM_DELAY_MS: (u64, u64) = (40, 120);
/// Inclusive delay range (ms) for punctuation and word separators.
pub const PUNCT_DELAY_MS: (u64, u64) = (150, 300);

/// A single emitted keystroke: the UTF-8 bytes of one character (or the
/// terminator), preceded by the human-like delay to wait before sending it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyStroke {
    /// UTF-8 bytes to write to the PTY master.
    pub bytes: Vec<u8>,
    /// Delay to wait *before* writing [`KeyStroke::bytes`].
    pub delay: Duration,
}

/// A small, fast, non-cryptographic PRNG (SplitMix64). Deterministic given a
/// seed, which is what makes the jitter plan unit-testable.
#[derive(Debug, Clone)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `lo..=hi`.
    fn range_inclusive(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + self.next_u64() % (hi - lo + 1)
    }
}

/// Whether `c` should be typed with the slower punctuation/separator cadence.
fn is_separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '.' | ',' | '?' | '!' | ';' | ':')
}

/// Decomposes commands into human-cadenced keystrokes.
#[derive(Debug, Clone)]
pub struct Jitter {
    rng: SplitMix64,
    alnum: (u64, u64),
    punct: (u64, u64),
}

impl Default for Jitter {
    fn default() -> Self {
        Self::new()
    }
}

impl Jitter {
    /// Creates a jitter seeded from the system clock.
    pub fn new() -> Self {
        Self::with_seed(seed_from_time())
    }

    /// Creates a jitter with a fixed seed (deterministic delays).
    pub fn with_seed(seed: u64) -> Self {
        Self {
            rng: SplitMix64::new(seed),
            alnum: ALNUM_DELAY_MS,
            punct: PUNCT_DELAY_MS,
        }
    }

    /// Creates a jitter with custom delay ranges (ms, inclusive). Setting both
    /// ranges to `(0, 0)` makes typing effectively instantaneous, which is
    /// useful in tests.
    pub fn with_delays(seed: u64, alnum: (u64, u64), punct: (u64, u64)) -> Self {
        Self {
            rng: SplitMix64::new(seed),
            alnum,
            punct,
        }
    }

    fn delay_for(&mut self, c: char) -> Duration {
        let (lo, hi) = if c.is_alphanumeric() {
            self.alnum
        } else if is_separator(c) {
            self.punct
        } else {
            // Other symbols default to the faster alphanumeric cadence.
            self.alnum
        };
        Duration::from_millis(self.rng.range_inclusive(lo, hi))
    }

    /// Builds the keystroke plan for `command`: one keystroke per character,
    /// each with a jittered pre-delay, followed by a carriage-return
    /// terminator (also pre-delayed, as a human pauses before pressing Enter).
    pub fn plan(&mut self, command: &str) -> Vec<KeyStroke> {
        let mut out = Vec::with_capacity(command.chars().count() + 1);
        let mut buf = [0u8; 4];
        for c in command.chars() {
            let delay = self.delay_for(c);
            let encoded = c.encode_utf8(&mut buf);
            out.push(KeyStroke {
                bytes: encoded.as_bytes().to_vec(),
                delay,
            });
        }
        let term_delay =
            Duration::from_millis(self.rng.range_inclusive(self.punct.0, self.punct.1));
        out.push(KeyStroke {
            bytes: vec![b'\r'],
            delay: term_delay,
        });
        out
    }

    /// Types `command` by writing each keystroke through `write`, sleeping the
    /// keystroke's delay first. The command is terminated with `\r`.
    ///
    /// `write` is typically a thin wrapper over the PTY session's input queue.
    ///
    /// # Errors
    /// Propagates the first error returned by `write`.
    pub fn type_command<F, E>(&mut self, command: &str, mut write: F) -> Result<(), E>
    where
        F: FnMut(&[u8]) -> Result<(), E>,
    {
        for ks in self.plan(command) {
            if !ks.delay.is_zero() {
                std::thread::sleep(ks.delay);
            }
            write(&ks.bytes)?;
        }
        Ok(())
    }
}

fn seed_from_time() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678_9ABC_DEF0)
        // Mix so successive process starts within the same nanosecond still
        // differ a little.
        .wrapping_mul(0x2545_F491_4F6C_DD1D)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_decomposes_into_per_char_keystrokes_plus_terminator() {
        let mut j = Jitter::with_seed(42);
        let plan = j.plan("hi");
        // 'h', 'i', '\r'
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].bytes, b"h");
        assert_eq!(plan[1].bytes, b"i");
        assert_eq!(plan[2].bytes, vec![b'\r']);
    }

    #[test]
    fn terminator_is_carriage_return_not_newline() {
        let mut j = Jitter::with_seed(1);
        let plan = j.plan("x");
        let last = plan.last().unwrap();
        assert_eq!(last.bytes, vec![b'\r']);
        assert!(!plan.iter().any(|k| k.bytes == vec![b'\n']));
    }

    #[test]
    fn multibyte_characters_are_one_keystroke_each() {
        let mut j = Jitter::with_seed(7);
        let plan = j.plan("č?");
        assert_eq!(plan.len(), 3); // 'č', '?', '\r'
        assert_eq!(plan[0].bytes, "č".as_bytes());
        assert_eq!(plan[0].bytes.len(), 2);
        assert_eq!(plan[1].bytes, b"?");
    }

    #[test]
    fn alphanumeric_delays_fall_in_the_short_range() {
        let mut j = Jitter::with_seed(12345);
        // A long alphanumeric run; every per-char delay must be in [40, 120].
        let plan = j.plan("abcdef0123456789ghijklmnop");
        for ks in plan.iter().take(plan.len() - 1) {
            let ms = ks.delay.as_millis() as u64;
            assert!(
                (ALNUM_DELAY_MS.0..=ALNUM_DELAY_MS.1).contains(&ms),
                "alnum delay {ms} out of range"
            );
        }
    }

    #[test]
    fn separator_delays_fall_in_the_long_range() {
        let mut j = Jitter::with_seed(999);
        // Separators get the slower cadence.
        for c in [' ', '.', ',', '?', '!', ';', ':'] {
            let plan = j.plan(&c.to_string());
            let ms = plan[0].delay.as_millis() as u64;
            assert!(
                (PUNCT_DELAY_MS.0..=PUNCT_DELAY_MS.1).contains(&ms),
                "separator {c:?} delay {ms} out of range"
            );
        }
    }

    #[test]
    fn type_command_emits_bytes_in_order_and_terminates_with_cr() {
        // Zero delays so the test does not actually sleep.
        let mut j = Jitter::with_delays(5, (0, 0), (0, 0));
        let mut written: Vec<u8> = Vec::new();
        j.type_command::<_, ()>("ls -la", |bytes| {
            written.extend_from_slice(bytes);
            Ok(())
        })
        .unwrap();
        assert_eq!(written, b"ls -la\r");
    }

    #[test]
    fn type_command_propagates_write_errors() {
        let mut j = Jitter::with_delays(5, (0, 0), (0, 0));
        let mut count = 0;
        let result: Result<(), &str> = j.type_command("abc", |_| {
            count += 1;
            if count == 2 {
                Err("boom")
            } else {
                Ok(())
            }
        });
        assert_eq!(result, Err("boom"));
        assert_eq!(count, 2); // stopped at the failing write
    }

    #[test]
    fn deterministic_for_a_fixed_seed() {
        let a = Jitter::with_seed(2024).plan("deterministic?");
        let b = Jitter::with_seed(2024).plan("deterministic?");
        assert_eq!(a, b);
    }
}
