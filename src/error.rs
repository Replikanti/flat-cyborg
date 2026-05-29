//! Error type shared across the crate.

use std::fmt;

/// Errors produced by the PTY wrapper.
#[derive(Debug)]
pub enum Error {
    /// An error originating from the underlying PTY layer (allocation,
    /// resize, or spawn).
    Pty(pty_process::Error),
    /// An I/O error reading from or writing to the PTY master.
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Pty(e) => write!(f, "pty error: {e}"),
            Error::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Pty(e) => Some(e),
            Error::Io(e) => Some(e),
        }
    }
}

impl From<pty_process::Error> for Error {
    fn from(e: pty_process::Error) -> Self {
        Error::Pty(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// Convenience alias for results returned by this crate.
pub type Result<T> = std::result::Result<T, Error>;
