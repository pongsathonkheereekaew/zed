//! Typed errors for the OMP boundary. Every failure is explicit and
//! user-visible: protocol mismatch / disconnect / revision drift never fall
//! back silently (plan §§6, 88).

use std::{fmt, time::Duration};

/// Failures at the cedian↔OMP boundary.
#[derive(Debug)]
pub enum OmpError {
    /// `omp` binary missing or failed to spawn.
    Spawn(String),
    /// `ready` frame never arrived (timeout) or was invalid.
    Handshake(String),
    /// Protocol/revision mismatch — hard fail, never silent fallback.
    Mismatch { expected: String, got: String },
    /// Transport broke (EOF, chunk violation, write failure).
    Transport(String),
    /// Server answered `success: false`.
    Command {
        command: String,
        error: String,
        code: Option<String>,
    },
    /// No response within the deadline.
    /// `after` is `None` when the wait is the client's own, unknown here.
    Timeout {
        command: String,
        after: Option<Duration>,
    },
    /// Stream ended before the prompt's `prompt_result`.
    StreamEnded { prompt_id: String },
    /// Spawn profile failed validation or its overlay could not be written
    /// (ADR-0020: fail closed, never a bare spawn).
    InvalidSpawnProfile(String),
}

impl fmt::Display for OmpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "failed to spawn OMP runtime: {e}"),
            Self::Handshake(e) => write!(f, "OMP handshake failed: {e}"),
            Self::Mismatch { expected, got } => {
                write!(
                    f,
                    "OMP runtime version mismatch (expected {expected}, got {got})"
                )
            }
            Self::Transport(e) => write!(f, "OMP transport error: {e}"),
            Self::Command {
                command,
                error,
                code: Some(code),
            } => {
                write!(f, "OMP command {command} failed ({code}): {error}")
            }
            Self::Command {
                command,
                error,
                code: None,
            } => {
                write!(f, "OMP command {command} failed: {error}")
            }
            Self::Timeout {
                command,
                after: Some(after),
            } => write!(f, "OMP command {command} timed out after {after:?}"),
            Self::Timeout {
                command,
                after: None,
            } => write!(f, "OMP command {command} timed out"),
            Self::StreamEnded { prompt_id } => {
                write!(f, "OMP stream ended before prompt {prompt_id} completed")
            }
            Self::InvalidSpawnProfile(e) => write!(f, "invalid OMP spawn profile: {e}"),
        }
    }
}

impl OmpError {
    /// Name the wait on a timeout from a call whose deadline the caller set.
    pub fn with_timeout(self, after: Duration) -> Self {
        match self {
            Self::Timeout { command, .. } => Self::Timeout {
                command,
                after: Some(after),
            },
            other => other,
        }
    }
}

impl std::error::Error for OmpError {}

impl From<omp_rpc::client::Error> for OmpError {
    fn from(e: omp_rpc::client::Error) -> Self {
        match e {
            omp_rpc::client::Error::Io(io) => Self::Transport(io.to_string()),
            omp_rpc::client::Error::Json(json) => Self::Transport(json.to_string()),
            omp_rpc::client::Error::Command {
                command,
                error,
                code,
            } => Self::Command {
                command,
                error,
                code,
            },
            omp_rpc::client::Error::Timeout { command } => Self::Timeout {
                command,
                after: None,
            },
            omp_rpc::client::Error::Closed => Self::Transport("server closed".to_string()),
            omp_rpc::client::Error::Protocol(msg) => Self::Transport(msg),
            omp_rpc::client::Error::InvalidArgument(msg) => Self::Transport(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_timeout() -> OmpError {
        OmpError::from(omp_rpc::client::Error::Timeout {
            command: "prompt".to_string(),
        })
    }

    #[test]
    fn client_timeout_never_claims_a_zero_wait() {
        assert_eq!(client_timeout().to_string(), "OMP command prompt timed out");
    }

    #[test]
    fn timeout_names_the_wait_when_the_caller_knows_it() {
        let e = client_timeout().with_timeout(Duration::from_secs(600));
        assert_eq!(e.to_string(), "OMP command prompt timed out after 600s");
    }
}
