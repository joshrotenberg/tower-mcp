//! Error types for MCP
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
//! MCP uses the server error range (-32000 to -32099) for protocol-specific
//! errors. Codes assigned by the spec are marked **spec**; codes used only by
//! this implementation (in the JSON-RPC "implementation-defined" subrange) are
//! marked *impl*.
//!
//! | Code   | Name                          | Source | Meaning                              |
//! |--------|-------------------------------|--------|--------------------------------------|
//! | -32000 | ConnectionClosed              | *impl* | Transport connection was closed      |
//! | -32002 | ResourceNotFound              | *deprecated* | **SEP-2164** reassigned to -32602; variant kept for backcompat |
//! | -32005 | SessionNotFound               | *impl* | Session not found or expired (legacy; deprecated by SEP-2567) |
//! | -32006 | SessionRequired               | *impl* | Mcp-Session-Id header required (legacy; deprecated by SEP-2567) |
//! | -32007 | Forbidden                     | *impl* | Access forbidden (insufficient scope)|
//! | -32008 | AlreadySubscribed             | *impl* | Resource already subscribed          |
//! | -32009 | NotSubscribed                 | *impl* | Resource not subscribed              |
//! | -32010 | RequestTimeout                | *impl* | Request exceeded timeout             |
//! | -32020 | HeaderMismatch                | **spec** (SEP-2243) | HTTP headers do not match the request body, or required headers are missing/malformed |
//! | -32021 | MissingRequiredClientCapability | **spec** (SEP-2575) | Client lacks a capability required by the request |
//! | -32022 | UnsupportedProtocolVersion    | **spec** (SEP-2575) | Server does not support the request's protocol version |
//! | -32042 | UrlElicitationRequired        | TS SDK | URL elicitation required             |
//!
//! The three **spec** codes were renumbered upstream by the error-code
//! allocation policy (spec PR modelcontextprotocol#2907, 2026-06): the SEP
//! documents originally assigned -32001 (HeaderMismatch), -32003
//! (MissingRequiredClientCapability), and -32004 (UnsupportedProtocolVersion);
//! the draft schema now assigns -32020/-32021/-32022 and is canonical. Verify
//! against the published `schema/2026-07-28` at finalization.

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
#[non_exhaustive]
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

/// MCP-specific error codes (in the -32000 to -32099 range).
///
/// Codes marked **spec** are assigned by an MCP SEP. Others are
/// implementation-defined within the JSON-RPC server-error range; using them
/// is permitted by JSON-RPC 2.0 but they are not part of the MCP wire spec.
///
/// Recently-changed assignments:
/// - `HeaderMismatch` (SEP-2243), `MissingRequiredClientCapability`, and
///   `UnsupportedProtocolVersion` (both SEP-2575) were originally assigned
///   -32001/-32003/-32004 by their SEP documents. The upstream error-code
///   allocation policy (spec PR modelcontextprotocol#2907, 2026-06)
///   renumbered them to -32020/-32021/-32022 in the draft schema, which is
///   canonical. **This is a breaking change on the wire** within the
///   experimental 2026-07-28 surface.
/// - `AlreadySubscribed` and `NotSubscribed` previously occupied -32003 and
///   -32004; they moved to -32008 and -32009 when those slots were believed
///   to be spec-assigned. They stay at -32008/-32009.
/// - `RequestTimeout` previously occupied -32001 and moved to -32010 for the
///   same reason. It stays at -32010.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
#[non_exhaustive]
pub enum McpErrorCode {
    /// Transport connection was closed.
    ConnectionClosed = -32000,
    /// SEP-2243: HTTP headers (e.g. `Mcp-Method`, `Mcp-Name`,
    /// `Mcp-Param-*`) do not match the corresponding values in the request
    /// body, or a required header is missing or malformed.
    ///
    /// Originally -32001 in the SEP text; renumbered to -32020 by the
    /// upstream error-code allocation (spec PR modelcontextprotocol#2907).
    ///
    /// Use [`JsonRpcError::header_mismatch`] to construct.
    HeaderMismatch = -32020,
    /// Resource not found.
    ///
    /// **Deprecated**: SEP-2164 (FINAL) moves this to the standard JSON-RPC
    /// `InvalidParams` code (-32602). The
    /// [`JsonRpcError::resource_not_found`] constructor now emits -32602.
    /// The enum variant is retained so existing pattern matches on
    /// `McpErrorCode::ResourceNotFound` keep compiling, but the variant's
    /// numeric value is no longer what the constructor produces.
    #[deprecated(
        since = "0.12.0",
        note = "SEP-2164 reassigned resource-not-found to InvalidParams (-32602). \
                Use JsonRpcError::resource_not_found or ErrorCode::InvalidParams."
    )]
    ResourceNotFound = -32002,
    /// SEP-2575: client capabilities advertised on the request do not
    /// include a capability required by the called method.
    ///
    /// Originally -32003 in the SEP text; renumbered to -32021 by the
    /// upstream error-code allocation (spec PR modelcontextprotocol#2907).
    MissingRequiredClientCapability = -32021,
    /// SEP-2575: server does not support the protocol version the client
    /// requested (via `MCP-Protocol-Version` header or per-request `_meta`).
    ///
    /// Originally -32004 in the SEP text; renumbered to -32022 by the
    /// upstream error-code allocation (spec PR modelcontextprotocol#2907).
    ///
    /// Use [`JsonRpcError::unsupported_protocol_version`] to construct.
    UnsupportedProtocolVersion = -32022,
    /// Session not found or expired -- client should re-initialize.
    ///
    /// SEP-2567 deprecates sessions entirely. This code stays for the
    /// 2025-11-25 protocol path; sessionless deployments will never emit it.
    SessionNotFound = -32005,
    /// Session ID is required but was not provided.
    ///
    /// SEP-2567 deprecates sessions entirely. Same caveat as `SessionNotFound`.
    SessionRequired = -32006,
    /// Access forbidden (insufficient scope or authorization).
    Forbidden = -32007,
    /// Resource already subscribed. Moved from -32003 when that slot was
    /// believed to be spec-assigned; stays at -32008.
    AlreadySubscribed = -32008,
    /// Resource not subscribed (for unsubscribe). Moved from -32004 when
    /// that slot was believed to be spec-assigned; stays at -32009.
    NotSubscribed = -32009,
    /// Request exceeded timeout. Moved from -32001 when that slot was
    /// believed to be spec-assigned; stays at -32010.
    RequestTimeout = -32010,
    /// URL elicitation is required before processing the request.
    UrlElicitationRequired = -32042,
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

    /// Resource not found. Per SEP-2164 (FINAL) this now uses the
    /// standard JSON-RPC `InvalidParams` code (-32602) rather than the
    /// legacy MCP-specific [`McpErrorCode::ResourceNotFound`] (-32002).
    /// The resource URI is a parameter, so missing-parameter and
    /// unknown-resource-URI are the same error class.
    pub fn resource_not_found(uri: &str) -> Self {
        // SEP-2164: the error data SHOULD carry the requested URI so clients
        // can programmatically identify which resource was missing.
        Self::new(
            ErrorCode::InvalidParams,
            format!("Resource not found: {}", uri),
        )
        .with_data(serde_json::json!({ "uri": uri }))
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

    /// URL elicitation is required before processing the request
    pub fn url_elicitation_required(message: impl Into<String>) -> Self {
        Self::mcp_error(McpErrorCode::UrlElicitationRequired, message)
    }

    /// SEP-2243: HTTP headers do not match the request body, or a required
    /// header is missing or malformed.
    ///
    /// Servers using the Streamable HTTP transport return this error code
    /// (with HTTP status `400 Bad Request`) when any of the following hold:
    ///
    /// - A required standard header (`Mcp-Method`, `Mcp-Name`) is missing
    ///   for the corresponding method.
    /// - A header value does not match the request body value.
    /// - A `Mcp-Param-{Name}` Base64-encoded value cannot be decoded.
    /// - A header value contains invalid characters.
    pub fn header_mismatch(message: impl Into<String>) -> Self {
        Self::mcp_error(McpErrorCode::HeaderMismatch, message)
    }

    /// SEP-2575: server does not support the protocol version the client
    /// requested. The error data carries both the AS-supported versions
    /// and the version the client asked for, matching the spec shape:
    ///
    /// ```text
    /// data: { supported: [...], requested: "..." }
    /// ```
    pub fn unsupported_protocol_version(
        requested: impl Into<String>,
        supported: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let data = UnsupportedProtocolVersionData {
            supported: supported.into_iter().map(Into::into).collect(),
            requested: requested.into(),
        };
        Self {
            code: McpErrorCode::UnsupportedProtocolVersion.code(),
            message: format!(
                "Unsupported protocol version: {} (supported: {})",
                data.requested,
                data.supported.join(", "),
            ),
            data: Some(
                serde_json::to_value(&data)
                    .expect("UnsupportedProtocolVersionData is serializable"),
            ),
        }
    }
}

/// SEP-2575 error data for `UnsupportedProtocolVersion` (-32022).
///
/// Wire shape per the draft schema:
/// ```json
/// {
///   "supported": ["2026-07-28", "2025-11-25"],
///   "requested": "2027-01-01"
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnsupportedProtocolVersionData {
    /// Protocol versions the server supports. The client should pick a
    /// mutually-supported version from this list and retry.
    pub supported: Vec<String>,
    /// The protocol version that was requested by the client.
    pub requested: String,
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
#[non_exhaustive]
pub enum Error {
    #[error("JSON-RPC error: {0:?}")]
    JsonRpc(JsonRpcError),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// A tool execution error.
    ///
    /// When returned from a tool handler, this variant is mapped to JSON-RPC
    /// error code `-32603` (Internal Error) in the router's `Service::call`
    /// implementation. The `ToolError` message becomes the JSON-RPC error message.
    #[error("{0}")]
    Tool(#[from] ToolError),

    #[error("Transport error: {0}")]
    Transport(String),

    /// The server indicated the session has expired or is not found.
    ///
    /// This corresponds to JSON-RPC error code `-32005` (SessionNotFound)
    /// or an HTTP 404 response when a session ID was attached.
    /// Clients should re-initialize the connection.
    #[error("Session expired")]
    SessionExpired,

    /// A single SSE event exceeded the configured maximum size.
    ///
    /// Raised by SSE-consuming client transports when a server streams an
    /// event larger than the configured cap. The stream is terminated
    /// rather than buffering without bound. `size` is the buffered size at
    /// the point the limit was detected; `limit` is the configured cap.
    #[error("SSE event too large: {size} bytes buffered, limit is {limit} bytes")]
    SseEventTooLarge {
        /// Bytes buffered for the event when the limit was exceeded.
        size: usize,
        /// The configured maximum event size in bytes.
        limit: usize,
    },

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
    /// # use tower_mcp_types::Error;
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
    /// This is useful for adding context when converting errors.
    /// For a more ergonomic API, see [`ResultExt::tool_context`] which can be
    /// called directly on `Result` values:
    ///
    /// ```rust
    /// # use tower_mcp_types::error::ResultExt;
    /// # fn example() -> tower_mcp_types::Result<()> {
    /// let result: Result<(), std::io::Error> = Err(std::io::Error::other("connection refused"));
    /// result.tool_context("API request failed")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn tool_context<E: std::fmt::Display>(context: impl Into<String>, err: E) -> Self {
        Error::Tool(ToolError::new(format!("{}: {}", context.into(), err)))
    }

    /// Create a JSON-RPC "Invalid params" error (`-32602`).
    ///
    /// Shorthand for `Error::JsonRpc(JsonRpcError::invalid_params(msg))`.
    ///
    /// ```rust
    /// # use tower_mcp_types::Error;
    /// let err = Error::invalid_params("missing required field 'name'");
    /// ```
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Error::JsonRpc(JsonRpcError::invalid_params(message))
    }

    /// Create a JSON-RPC "Internal error" error (`-32603`).
    ///
    /// Shorthand for `Error::JsonRpc(JsonRpcError::internal_error(msg))`.
    ///
    /// ```rust
    /// # use tower_mcp_types::Error;
    /// let err = Error::internal("unexpected state");
    /// ```
    pub fn internal(message: impl Into<String>) -> Self {
        Error::JsonRpc(JsonRpcError::internal_error(message))
    }
}

/// Extension trait for converting errors into tower-mcp tool errors.
///
/// Provides ergonomic error conversion methods on `Result` types,
/// similar to `anyhow::Context`. Import this trait to use `.tool_err()`
/// and `.tool_context()` on any `Result` whose error type implements `Display`.
///
/// # Examples
///
/// ```rust
/// use tower_mcp_types::error::ResultExt;
///
/// fn query_database() -> tower_mcp_types::Result<String> {
///     let result: Result<String, std::io::Error> =
///         Err(std::io::Error::other("connection refused"));
///     let value = result.tool_context("database query failed")?;
///     Ok(value)
/// }
/// ```
pub trait ResultExt<T> {
    /// Convert the error into a tool error.
    ///
    /// ```rust
    /// use tower_mcp_types::error::ResultExt;
    /// # fn example() -> tower_mcp_types::Result<()> {
    /// let value: Result<i32, std::io::Error> = Err(std::io::Error::other("timeout"));
    /// let value = value.tool_err()?;
    /// # Ok(())
    /// # }
    /// ```
    fn tool_err(self) -> std::result::Result<T, Error>;

    /// Convert the error into a tool error with additional context.
    ///
    /// ```rust
    /// use tower_mcp_types::error::ResultExt;
    /// # fn example() -> tower_mcp_types::Result<()> {
    /// let value: Result<i32, std::io::Error> = Err(std::io::Error::other("timeout"));
    /// let value = value.tool_context("database query failed")?;
    /// # Ok(())
    /// # }
    /// ```
    fn tool_context(self, context: impl Into<String>) -> std::result::Result<T, Error>;
}

impl<T, E: std::fmt::Display> ResultExt<T> for std::result::Result<T, E> {
    fn tool_err(self) -> std::result::Result<T, Error> {
        self.map_err(Error::tool_from)
    }

    fn tool_context(self, context: impl Into<String>) -> std::result::Result<T, Error> {
        self.map_err(|e| Error::tool_context(context, e))
    }
}

impl From<JsonRpcError> for Error {
    fn from(err: JsonRpcError) -> Self {
        Error::JsonRpc(err)
    }
}

impl From<std::convert::Infallible> for Error {
    fn from(err: std::convert::Infallible) -> Self {
        match err {}
    }
}

/// Result type alias for tower-mcp
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // SEP-2575 UnsupportedProtocolVersion (-32022) wire-format tests
    // =========================================================================

    #[test]
    fn unsupported_protocol_version_code_is_negative_32022() {
        // Renumbered from -32004 by the upstream error-code allocation
        // (spec PR modelcontextprotocol#2907).
        assert_eq!(McpErrorCode::UnsupportedProtocolVersion.code(), -32022);
    }

    #[test]
    fn unsupported_protocol_version_constructor_has_spec_shape() {
        let err =
            JsonRpcError::unsupported_protocol_version("2027-01-01", ["2026-07-28", "2025-11-25"]);
        assert_eq!(err.code, -32022);
        let data = err.data.expect("data must be present");
        assert_eq!(
            data["supported"],
            serde_json::json!(["2026-07-28", "2025-11-25"])
        );
        assert_eq!(data["requested"], "2027-01-01");
        assert!(
            data.get("supportedVersions").is_none(),
            "spec field name is 'supported', not 'supportedVersions'"
        );
    }

    #[test]
    fn unsupported_protocol_version_data_round_trip() {
        let original = UnsupportedProtocolVersionData {
            supported: vec!["2026-07-28".into(), "2025-11-25".into()],
            requested: "2027-01-01".into(),
        };
        let json = serde_json::to_value(&original).unwrap();
        let parsed: UnsupportedProtocolVersionData = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(parsed.supported, original.supported);
        assert_eq!(parsed.requested, original.requested);
    }

    // =========================================================================
    // SEP-2164: ResourceNotFound -> InvalidParams (-32602)
    // =========================================================================

    #[test]
    fn resource_not_found_constructor_uses_invalid_params() {
        let err = JsonRpcError::resource_not_found("file:///gone.txt");
        assert_eq!(err.code, ErrorCode::InvalidParams.code());
        assert_eq!(err.code, -32602);
        assert!(err.message.contains("file:///gone.txt"));
        // SEP-2164: data.uri carries the requested URI.
        let data = err.data.expect("data must be present");
        assert_eq!(data["uri"], "file:///gone.txt");
    }

    #[test]
    fn resource_not_found_serializes_with_spec_code() {
        let err = JsonRpcError::resource_not_found("urn:test:x");
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["code"], -32602);
    }

    // =========================================================================
    // Subscribe-code migration (avoid SEP-2575 collision)
    // =========================================================================

    #[test]
    fn subscribe_codes_moved_off_spec_assignments() {
        // Our subscribe codes moved to -32008/-32009 when -32003/-32004 were
        // believed spec-assigned. The SEP-2575 codes have since been
        // renumbered to -32021/-32022 (spec PR modelcontextprotocol#2907);
        // the subscribe codes stay put.
        assert_eq!(McpErrorCode::AlreadySubscribed.code(), -32008);
        assert_eq!(McpErrorCode::NotSubscribed.code(), -32009);
        assert_eq!(McpErrorCode::MissingRequiredClientCapability.code(), -32021);
        assert_eq!(McpErrorCode::UnsupportedProtocolVersion.code(), -32022);
    }

    // =========================================================================
    // SEP-2243 HeaderMismatch (-32020) wire-format tests
    // =========================================================================

    #[test]
    fn header_mismatch_code_is_negative_32020() {
        // Renumbered from -32001 by the upstream error-code allocation
        // (spec PR modelcontextprotocol#2907).
        assert_eq!(McpErrorCode::HeaderMismatch.code(), -32020);
    }

    #[test]
    fn header_mismatch_constructor_uses_spec_code() {
        let err = JsonRpcError::header_mismatch(
            "Mcp-Name header value 'foo' does not match body value 'bar'",
        );
        assert_eq!(err.code, -32020);
        assert_eq!(err.code, McpErrorCode::HeaderMismatch.code());
        assert!(err.message.contains("Mcp-Name"));
    }

    #[test]
    fn request_timeout_moved_off_spec_assignment() {
        // RequestTimeout moved to -32010 when -32001 was believed
        // spec-assigned; HeaderMismatch is now -32020.
        assert_eq!(McpErrorCode::RequestTimeout.code(), -32010);
        assert_eq!(McpErrorCode::HeaderMismatch.code(), -32020);
    }

    #[test]
    #[allow(deprecated)] // ResourceNotFound stays in the enum for backcompat
    fn no_two_mcp_codes_share_a_value() {
        let all = [
            McpErrorCode::ConnectionClosed.code(),
            McpErrorCode::HeaderMismatch.code(),
            McpErrorCode::RequestTimeout.code(),
            McpErrorCode::ResourceNotFound.code(),
            McpErrorCode::MissingRequiredClientCapability.code(),
            McpErrorCode::UnsupportedProtocolVersion.code(),
            McpErrorCode::SessionNotFound.code(),
            McpErrorCode::SessionRequired.code(),
            McpErrorCode::Forbidden.code(),
            McpErrorCode::AlreadySubscribed.code(),
            McpErrorCode::NotSubscribed.code(),
            McpErrorCode::UrlElicitationRequired.code(),
        ];
        let mut sorted = all.to_vec();
        sorted.sort_unstable();
        let original_len = sorted.len();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            original_len,
            "collision in McpErrorCode values: {:?}",
            all
        );
    }

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

    #[test]
    fn test_result_ext_tool_err() {
        let result: std::result::Result<(), std::io::Error> =
            Err(std::io::Error::other("disk full"));
        let err = result.tool_err().unwrap_err();
        assert!(matches!(err, Error::Tool(_)));
        assert!(err.to_string().contains("disk full"));
    }

    #[test]
    fn test_result_ext_tool_context() {
        let result: std::result::Result<(), std::io::Error> =
            Err(std::io::Error::other("connection refused"));
        let err = result.tool_context("database query failed").unwrap_err();
        assert!(matches!(err, Error::Tool(_)));
        assert!(err.to_string().contains("database query failed"));
        assert!(err.to_string().contains("connection refused"));
    }

    #[test]
    fn test_result_ext_ok_passes_through() {
        let result: std::result::Result<i32, std::io::Error> = Ok(42);
        assert_eq!(result.tool_err().unwrap(), 42);
        let result: std::result::Result<i32, std::io::Error> = Ok(42);
        assert_eq!(result.tool_context("should not appear").unwrap(), 42);
    }
}
