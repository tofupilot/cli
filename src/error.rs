//! The crate-wide error type.
//!
//! Non-entrypoint functions return `Result<T, CliError>`. Command handlers in
//! `main.rs` convert the error to a process exit code at the boundary. The
//! `From` impls let `?` propagate the common foreign errors (IO, JSON,
//! reqwest, redb) without per-call `.map_err`.

use std::fmt;

/// A unified error for CLI internals.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// Filesystem / process IO.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// JSON (de)serialization.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// HTTP transport error from reqwest.
    #[error(transparent)]
    Http(#[from] reqwest::Error),

    /// A non-success HTTP response, carrying the status for retry
    /// classification and a (possibly truncated) body for display.
    #[error("HTTP {status}: {body}")]
    Status { status: u16, body: String },

    /// Any other failure described by a message. Database and other
    /// component errors flow through here with a descriptive prefix.
    #[error("{0}")]
    Message(String),
}

impl CliError {
    /// Construct a message error from anything displayable.
    pub fn msg(m: impl fmt::Display) -> Self {
        CliError::Message(m.to_string())
    }

    /// The human message body without the status prefix. For a [`Self::Status`]
    /// error this is the server's message alone; for any other variant it is
    /// the full `Display`. Lets callers reproduce their original
    /// status-stripped error text.
    pub fn body(&self) -> String {
        match self {
            CliError::Status { body, .. } => body.clone(),
            other => other.to_string(),
        }
    }
}

impl From<String> for CliError {
    fn from(s: String) -> Self {
        CliError::Message(s)
    }
}

impl From<&str> for CliError {
    fn from(s: &str) -> Self {
        CliError::Message(s.to_string())
    }
}

/// Convenience alias for fallible CLI internals.
pub type CliResult<T> = Result<T, CliError>;
