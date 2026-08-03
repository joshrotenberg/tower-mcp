//! Process exit status classification for one-shot automation.

use std::sync::atomic::{AtomicU8, Ordering};

use tower_mcp::Error;

/// Public process statuses used by `mcp-repl --exec`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum ExitStatus {
    /// Every command completed successfully.
    #[default]
    Success = 0,
    /// A search or check-style command completed but found nothing.
    NoMatch = 1,
    /// The invocation or command was invalid locally.
    Usage = 2,
    /// The server rejected a request or a tool reported an error result.
    Server = 3,
    /// The transport or protocol connection failed.
    Transport = 4,
    /// Authentication or authorization failed.
    Auth = 5,
}

impl ExitStatus {
    pub const fn code(self) -> i32 {
        self as i32
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::NoMatch => "no_match",
            Self::Usage => "usage",
            Self::Server => "server",
            Self::Transport => "transport",
            Self::Auth => "auth",
        }
    }

    pub fn from_mcp_error(error: &Error) -> Self {
        match error {
            Error::Transport(message) if looks_like_auth_failure(message) => Self::Auth,
            Error::Transport(_) | Error::SessionExpired | Error::SseEventTooLarge { .. } => {
                Self::Transport
            }
            Error::JsonRpc(error) if error.code == -32007 => Self::Auth,
            Error::JsonRpc(_) | Error::Tool(_) | Error::Internal(_) => Self::Server,
            // A peer supplied data that could not be decoded, so this is a
            // wire/protocol failure rather than a local command syntax error.
            Error::Serialization(_) => Self::Transport,
            _ => Self::Server,
        }
    }
}

static EXIT_STATUS: AtomicU8 = AtomicU8::new(ExitStatus::Success as u8);

/// Remember the most severe outcome across repeated `--exec` commands.
pub fn record(status: ExitStatus) {
    EXIT_STATUS.fetch_max(status as u8, Ordering::Relaxed);
}

pub fn current() -> ExitStatus {
    match EXIT_STATUS.load(Ordering::Relaxed) {
        0 => ExitStatus::Success,
        1 => ExitStatus::NoMatch,
        2 => ExitStatus::Usage,
        3 => ExitStatus::Server,
        4 => ExitStatus::Transport,
        _ => ExitStatus::Auth,
    }
}

fn looks_like_auth_failure(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "http 401",
        "http 403",
        "401 unauthorized",
        "403 forbidden",
        "unauthorized",
        "forbidden",
        "invalid_token",
        "insufficient_scope",
        "authentication",
        "authorization",
        "token provider",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_mcp::error::JsonRpcError;

    #[test]
    fn classifies_protocol_and_auth_failures() {
        assert_eq!(
            ExitStatus::from_mcp_error(&Error::Transport("connection closed".into())),
            ExitStatus::Transport
        );
        assert_eq!(
            ExitStatus::from_mcp_error(&Error::Transport(
                "request failed for http://127.0.0.1:4030".into()
            )),
            ExitStatus::Transport
        );
        assert_eq!(
            ExitStatus::from_mcp_error(&Error::Transport(
                "HTTP 401 Unauthorized from server".into()
            )),
            ExitStatus::Auth
        );
        assert_eq!(
            ExitStatus::from_mcp_error(&Error::JsonRpc(JsonRpcError {
                code: -32007,
                message: "insufficient scope".into(),
                data: None,
            })),
            ExitStatus::Auth
        );
        assert_eq!(
            ExitStatus::from_mcp_error(&Error::invalid_params("bad input")),
            ExitStatus::Server
        );
    }
}
