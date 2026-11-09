//! The error every tool reports: a message for the MCP client.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// A user-facing failure.
    Message(String),
    /// The agent connection broke; the caller may reconnect once.
    Disconnected(String),
    /// The call waits for a background run's news; the server calls it again later instead
    /// of answering now.
    Waiting,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(message) | Self::Disconnected(message) => f.write_str(message),
            Self::Waiting => f.write_str("The call waits for a background run."),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Message(error.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Message(error.to_string())
    }
}

/// Return early with a formatted [`Error::Message`].
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::error::Error::msg(format!($($arg)*)))
    };
}
pub(crate) use bail;
