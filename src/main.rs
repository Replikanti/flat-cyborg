//! Demo entry point for the flat-cyborg PTY wrapper.
//!
//! The full interactive driver lands in a later change. For now this prints
//! the crate version so the binary target builds and links against the lib.

fn main() {
    println!("flat-cyborg {}", flat_cyborg::VERSION);
}
