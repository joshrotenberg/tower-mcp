//! Error types for tower-mcp
//!
//! ## JSON-RPC Error Codes
//!
//! Standard JSON-RPC 2.0 error codes are defined in the specification:
//! <https://www.jsonrpc.org/specification#error_object>
//!
//! | Code   | Message          | Meaning                                  |
//! |--------|------------------|------------------------------------------|
//! | -32700 | Parse error      | Invalid JSON was received                |
//! | -32600 | Invalid Request  | The JSON sent is not a valid Request     |
//! | -32601 | Method not found | The method does not exist / is not available |
//! | -32602 | Invalid params   | Invalid method parameter(s)              |
//! | -32603 | Internal error   | Internal JSON-RPC error                  |
//!
//! ## MCP-Specific Error Codes
//!
//! MCP uses the server error range (-32000 to -32099) for protocol-specific errors:
//!
//! | Code   | Name            | Meaning                                  |
//! |--------|-----------------|------------------------------------------|
//! | -32000 | ConnectionClosed| Transport connection was closed          |
//! | -32001 | RequestTimeout  | Request exceeded timeout                 |
//! | -32002 | ResourceNotFound| Resource not found                       |
//! | -32003 | AlreadySubscribed| Resource already subscribed             |
//! | -32004 | NotSubscribed   | Resource not subscribed (for unsubscribe)|
//! | -32005 | SessionNotFound | Session not found or expired             |
//! | -32006 | SessionRequired | MCP-Session-Id header is required        |
//! | -32007 | Forbidden       | Access forbidden (insufficient scope)    |

use serde::{Deserialize, Serialize};

/// Type-erased error type used for middleware composition.
///
/// This is the standard error type in the tower ecosystem, used by
/// [`tower`](https://docs.rs/tower), [`tower-http`](https://docs.rs/tower-http),
/// and other tower-compatible crates.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Standard JSON-RPC error codes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ErrorCode {
    /// Invalid JSON was received
    ParseError = -32700,
    /// The JSON sent is not a valid Request object
    InvalidRequest = -32600,
    /// The method does not exist / is not available
    MethodNotFound = -32601,
    /// Invalid method parameter(s)
    InvalidParams = -32602,
    /// Internal JSON-RPC error
    InternalError = -32603,
}

/// MCP-specific error codes (in the -32000 to -32099 range)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum McpErrorCode {
    /// Transport connection was closed
    ConnectionClosed = -32000,
    /// Request exceeded timeout
    RequestTimeout = -32001,
    /// Resource not found
    ResourceNotFound = -32002,
    /// Resource already subscribed
    AlreadySubscribed = -32003,
    /// Resource not subscribed (for unsubscribe)
    NotSubscribed = -32004,
    /// Session not found or expired - client should re-initialize
    SessionNotFound = -32005,
    /// Session ID is required but was not provided
    SessionRequired = -32006,
    /// Access forbidden (insufficient scope or authorization)
    Forbidden = -32007,
}

impl McpErrorCode {
    pub fn code(self) -> i32 {
        self as i32
    }
}

impl ErrorCode {
    pub fn code(self) -> i32 {
        self as i32
    }
}

/// JSON-RPC error object
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.code(),
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ParseError, message)
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, message)
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            ErrorCode::MethodNotFound,
            format!("Method not found: {}", method),
        )
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidParams, message)
    }

    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InternalError, message)
    }

    /// Create an MCP-specific error
    pub fn mcp_error(code: McpErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.code(),
            message: message.into(),
            data: None,
        }
    }

    /// Connection was closed
    pub fn connection_closed(message: impl Into<String>) -> Self {
        Self::mcp_error(McpErrorCode::ConnectionClosed, message)
    }

    /// Request timed out
    pub fn request_timeout(message: impl Into<String>) -> Self {
        Self::mcp_error(McpErrorCode::RequestTimeout, message)
    }

    /// Resource not found
    pub fn resource_not_found(uri: &str) -> Self {
        Self::mcp_error(
            McpErrorCode::ResourceNotFound,
            format!("Resource not found: {}", uri),
        )
    }

    /// Resource already subscribed
    pub fn already_subscribed(uri: &str) -> Self {
        Self::mcp_error(
            McpErrorCode::AlreadySubscribed,
            format!("Already subscribed to: {}", uri),
        )
    }

    /// Resource not subscribed
    pub fn not_subscribed(uri: &str) -> Self {
        Self::mcp_error(
            McpErrorCode::NotSubscribed,
            format!("Not subscribed to: {}", uri),
        )
    }

    /// Session not found or expired
    ///
    /// Clients receiving this error should re-initialize the connection.
    /// The session may have expired due to inactivity or server restart.
    pub fn session_not_found() -> Self {
        Self::mcp_error(
            McpErrorCode::SessionNotFound,
            "Session not found or expired. Please re-initialize the connection.",
        )
    }

    /// Session not found with a specific session ID
    pub fn session_not_found_with_id(session_id: &str) -> Self {
        Self::mcp_error(
            McpErrorCode::SessionNotFound,
            format!(
                "Session '{}' not found or expired. Please re-initialize the connection.",
                session_id
            ),
        )
    }

    /// Session ID is required
    pub fn session_required() -> Self {
        Self::mcp_error(
            McpErrorCode::SessionRequired,
            "MCP-Session-Id header is required for this request.",
        )
    }

    /// Access forbidden (insufficient scope or authorization)
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::mcp_error(McpErrorCode::Forbidden, message)
    }
}

/// Tool execution error with context
#[derive(Debug)]
pub struct ToolError {
    /// The tool name that failed
    pub tool: Option<String>,
    /// Error message
    pub message: String,
    /// Source error if any
    pub source: Option<BoxError>,
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(tool) = &self.tool {
            write!(f, "Tool '{}' error: {}", tool, self.message)
        } else {
            write!(f, "Tool error: {}", self.message)
        }
    }
}

impl std::error::Error for ToolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|e| e.as_ref() as &(dyn std::error::Error + 'static))
    }
}

impl ToolError {
    /// Create a new tool error with just a message
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            tool: None,
            message: message.into(),
            source: None,
        }
    }

    /// Create a tool error with the tool name
    pub fn with_tool(tool: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            tool: Some(tool.into()),
            message: message.into(),
            source: None,
        }
    }

    /// Add a source error
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }
}

/// tower-mcp error type
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("JSON-RPC error: {0:?}")]
    JsonRpc(JsonRpcError),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("{0}")]
    Tool(#[from] ToolError),

    #[error("Transport error: {0}")]
    Transport(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl Error {
    /// Create a simple tool error from a string (for backwards compatibility)
    pub fn tool(message: impl Into<String>) -> Self {
        Error::Tool(ToolError::new(message))
    }

    /// Create a tool error with the tool name
    pub fn tool_with_name(tool: impl Into<String>, message: impl Into<String>) -> Self {
        Error::Tool(ToolError::with_tool(tool, message))
    }

    /// Create a tool error from any `Display` type.
    ///
    /// This is useful for converting errors in a `map_err` chain:
    ///
    /// ```rust
    /// # use tower_mcp::Error;
    /// # fn example() -> Result<(), Error> {
    /// let result: Result<(), std::io::Error> = Err(std::io::Error::other("oops"));
    /// result.map_err(Error::tool_from)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn tool_from<E: std::fmt::Display>(err: E) -> Self {
        Error::Tool(ToolError::new(err.to_string()))
    }

    /// Create a tool error with context prefix.
    ///
    /// This is useful for adding context when converting errors:
    ///
    /// ```rust
    /// # use tower_mcp::Error;
    /// # fn example() -> Result<(), Error> {
    /// let result: Result<(), std::io::Error> = Err(std::io::Error::other("connection refused"));
    /// result.map_err(|e| Error::tool_context("API request failed", e))?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn tool_context<E: std::fmt::Display>(context: impl Into<String>, err: E) -> Self {
        Error::Tool(ToolError::new(format!("{}: {}", context.into(), err)))
    }
}

impl From<JsonRpcError> for Error {
    fn from(err: JsonRpcError) -> Self {
        Error::JsonRpc(err)
    }
}

/// Result type alias for tower-mcp
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_box_error_from_io_error() {
        let io_err = std::io::Error::other("disk full");
        let boxed: BoxError = io_err.into();
        assert_eq!(boxed.to_string(), "disk full");
    }

    #[test]
    fn test_box_error_from_string() {
        let err: BoxError = "something went wrong".into();
        assert_eq!(err.to_string(), "something went wrong");
    }

    #[test]
    fn test_box_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BoxError>();
    }

    #[test]
    fn test_tool_error_source_uses_box_error() {
        let io_err = std::io::Error::other("timeout");
        let tool_err = ToolError::new("failed").with_source(io_err);
        assert!(tool_err.source.is_some());
        assert_eq!(tool_err.source.unwrap().to_string(), "timeout");
    }
}
