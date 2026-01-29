//! SEP-1442 Stateless MCP support (experimental)
//!
//! This module provides experimental support for stateless MCP as defined in
//! [SEP-1442](https://github.com/modelcontextprotocol/specification/issues/1442).
//!
//! ## Feature Flag
//!
//! This module is gated behind the `stateless` feature flag:
//!
//! ```toml
//! tower-mcp = { version = "0.2", features = ["stateless"] }
//! ```
//!
//! ## Key Changes from Standard MCP
//!
//! 1. **Per-request protocol version**: Every request includes the protocol version
//! 2. **Optional discovery**: `server/discover` RPC for capability discovery
//! 3. **Optional sessions**: Sessions are no longer mandatory
//! 4. **Per-request client capabilities**: Clients can specify capabilities per-request
//!
//! ## Warning
//!
//! SEP-1442 is still in draft status. The API may change as the specification evolves.

use serde::{Deserialize, Serialize};

use crate::protocol::{
    ClientCapabilities, Implementation, ProgressToken, Root, ServerCapabilities,
};

// =============================================================================
// Extended _meta fields for stateless mode
// =============================================================================

/// Extended request metadata for stateless MCP (SEP-1442).
///
/// In stateless mode, each request must be self-contained. This extended metadata
/// allows passing protocol version, session ID, and client capabilities per-request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatelessRequestMeta {
    /// Progress token for receiving progress notifications
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_token: Option<ProgressToken>,

    /// The MCP protocol version for this request (SEP-1442).
    ///
    /// For HTTP transport, this must match the `MCP-Protocol-Version` header.
    #[serde(
        rename = "modelcontextprotocol.io/mcpProtocolVersion",
        skip_serializing_if = "Option::is_none"
    )]
    pub protocol_version: Option<String>,

    /// Optional session ID for requests that need session affinity (SEP-1442).
    ///
    /// For HTTP transport, this must match the `MCP-Session-Id` header.
    #[serde(
        rename = "modelcontextprotocol.io/sessionId",
        skip_serializing_if = "Option::is_none"
    )]
    pub session_id: Option<String>,

    /// Client capabilities for this specific request (SEP-1442).
    ///
    /// Allows the server to know what optional features the client can handle
    /// for this specific transaction.
    #[serde(
        rename = "modelcontextprotocol.io/clientCapabilities",
        skip_serializing_if = "Option::is_none"
    )]
    pub client_capabilities: Option<ClientCapabilities>,

    /// Client roots for this request (SEP-1442).
    #[serde(
        rename = "modelcontextprotocol.io/roots",
        skip_serializing_if = "Option::is_none"
    )]
    pub roots: Option<Vec<Root>>,

    /// Log level for this request (SEP-1442).
    ///
    /// Replaces the deprecated `logging/setLevel` RPC.
    #[serde(
        rename = "modelcontextprotocol.io/logLevel",
        skip_serializing_if = "Option::is_none"
    )]
    pub log_level: Option<LogLevel>,
}

/// Log levels (matches existing MCP log levels)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Notice,
    Warning,
    Error,
    Critical,
    Alert,
    Emergency,
}

// =============================================================================
// server/discover RPC
// =============================================================================

/// Request for the `server/discover` RPC (SEP-1442).
///
/// Allows clients to query server capabilities without establishing a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscoverParams {}

/// Response for the `server/discover` RPC (SEP-1442).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverResult {
    /// Protocol versions supported by this server.
    pub supported_versions: Vec<String>,

    /// Server capabilities.
    pub capabilities: ServerCapabilities,

    /// Server implementation info.
    pub server_info: Implementation,

    /// Optional instructions for using this server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

// =============================================================================
// messages/listen RPC
// =============================================================================

/// Request for the `messages/listen` RPC (SEP-1442).
///
/// Opens a persistent SSE stream for receiving server notifications.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagesListenParams {
    /// Extended metadata for stateless mode
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<StatelessRequestMeta>,
}

/// Notification sent as first event on a `messages/listen` SSE stream (SEP-1442).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagesListenNotification {
    // Empty for now - just signals the stream is ready
}

// =============================================================================
// Error codes (SEP-1442)
// =============================================================================

/// SEP-1442 error codes.
///
/// Note: These overlap with some existing MCP error codes. When stateless mode
/// is enabled, these take precedence.
pub mod error_codes {
    /// Unsupported protocol version (-32000)
    ///
    /// The error data MUST include `supportedVersions` array.
    pub const UNSUPPORTED_VERSION: i32 = -32000;

    /// Invalid or missing required session ID (-32001)
    pub const INVALID_SESSION: i32 = -32001;
}

/// Error data for unsupported protocol version errors.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnsupportedVersionData {
    /// Protocol versions supported by this server.
    pub supported_versions: Vec<String>,
}

// =============================================================================
// Stateless mode configuration
// =============================================================================

/// Configuration for stateless MCP mode.
#[derive(Debug, Clone, Default)]
pub struct StatelessConfig {
    /// Whether to require protocol version in every request.
    ///
    /// Default: true (as per SEP-1442)
    pub require_protocol_version: bool,

    /// Whether sessions are optional.
    ///
    /// When true, requests without session IDs are allowed.
    /// When false, behaves like standard MCP (sessions required after init).
    ///
    /// Default: true
    pub optional_sessions: bool,

    /// Whether to enable the `server/discover` RPC.
    ///
    /// Default: true
    pub enable_discover: bool,

    /// Whether to enable the `messages/listen` RPC.
    ///
    /// Default: true
    pub enable_messages_listen: bool,
}

impl StatelessConfig {
    /// Create a new stateless configuration with SEP-1442 defaults.
    pub fn new() -> Self {
        Self {
            require_protocol_version: true,
            optional_sessions: true,
            enable_discover: true,
            enable_messages_listen: true,
        }
    }

    /// Create a configuration that maintains backward compatibility.
    ///
    /// This enables stateless features but doesn't require them,
    /// allowing gradual migration.
    pub fn backward_compatible() -> Self {
        Self {
            require_protocol_version: false,
            optional_sessions: true,
            enable_discover: true,
            enable_messages_listen: true,
        }
    }
}

// =============================================================================
// Helper functions for extracting stateless metadata
// =============================================================================

impl StatelessRequestMeta {
    /// Extract stateless request metadata from JSON-RPC request params.
    ///
    /// The metadata is expected to be in the `_meta` field of the params object.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::stateless::StatelessRequestMeta;
    /// use serde_json::json;
    ///
    /// let params = json!({
    ///     "name": "my-tool",
    ///     "_meta": {
    ///         "modelcontextprotocol.io/mcpProtocolVersion": "2025-06-18",
    ///         "modelcontextprotocol.io/sessionId": "abc123"
    ///     }
    /// });
    ///
    /// let meta = StatelessRequestMeta::from_params(&params);
    /// assert!(meta.is_some());
    /// let meta = meta.unwrap();
    /// assert_eq!(meta.protocol_version, Some("2025-06-18".to_string()));
    /// ```
    pub fn from_params(params: &serde_json::Value) -> Option<Self> {
        params
            .get("_meta")
            .and_then(|meta| serde_json::from_value(meta.clone()).ok())
    }

    /// Check if this request includes client capabilities.
    pub fn has_client_capabilities(&self) -> bool {
        self.client_capabilities.is_some()
    }

    /// Check if this request includes roots.
    pub fn has_roots(&self) -> bool {
        self.roots.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stateless_meta_serialization() {
        let meta = StatelessRequestMeta {
            progress_token: None,
            protocol_version: Some("2025-06-18".to_string()),
            session_id: Some("abc123".to_string()),
            client_capabilities: None,
            roots: None,
            log_level: Some(LogLevel::Info),
        };

        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("modelcontextprotocol.io/mcpProtocolVersion"));
        assert!(json.contains("modelcontextprotocol.io/sessionId"));
        assert!(json.contains("modelcontextprotocol.io/logLevel"));
    }

    #[test]
    fn test_stateless_meta_deserialization() {
        let json = r#"{
            "modelcontextprotocol.io/mcpProtocolVersion": "2025-06-18",
            "modelcontextprotocol.io/sessionId": "test-session",
            "modelcontextprotocol.io/logLevel": "debug"
        }"#;

        let meta: StatelessRequestMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta.protocol_version, Some("2025-06-18".to_string()));
        assert_eq!(meta.session_id, Some("test-session".to_string()));
        assert_eq!(meta.log_level, Some(LogLevel::Debug));
    }

    #[test]
    fn test_discover_result_serialization() {
        let result = DiscoverResult {
            supported_versions: vec!["2025-06-18".to_string(), "2025-03-26".to_string()],
            capabilities: ServerCapabilities::default(),
            server_info: Implementation {
                name: "test-server".to_string(),
                version: "1.0.0".to_string(),
                title: None,
                description: None,
                icons: None,
                website_url: None,
            },
            instructions: Some("Test instructions".to_string()),
        };

        let json = serde_json::to_value(&result).unwrap();
        assert!(json["supportedVersions"].is_array());
        assert_eq!(json["serverInfo"]["name"], "test-server");
    }

    #[test]
    fn test_unsupported_version_data() {
        let data = UnsupportedVersionData {
            supported_versions: vec!["2025-06-18".to_string()],
        };

        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json["supportedVersions"][0], "2025-06-18");
    }

    #[test]
    fn test_config_defaults() {
        let config = StatelessConfig::new();
        assert!(config.require_protocol_version);
        assert!(config.optional_sessions);
        assert!(config.enable_discover);
        assert!(config.enable_messages_listen);
    }

    #[test]
    fn test_config_backward_compatible() {
        let config = StatelessConfig::backward_compatible();
        assert!(!config.require_protocol_version);
        assert!(config.optional_sessions);
    }

    #[test]
    fn test_from_params() {
        let params = serde_json::json!({
            "name": "test-tool",
            "_meta": {
                "modelcontextprotocol.io/mcpProtocolVersion": "2025-06-18",
                "modelcontextprotocol.io/sessionId": "session-123",
                "modelcontextprotocol.io/logLevel": "debug"
            }
        });

        let meta = StatelessRequestMeta::from_params(&params).unwrap();
        assert_eq!(meta.protocol_version, Some("2025-06-18".to_string()));
        assert_eq!(meta.session_id, Some("session-123".to_string()));
        assert_eq!(meta.log_level, Some(LogLevel::Debug));
    }

    #[test]
    fn test_from_params_no_meta() {
        let params = serde_json::json!({
            "name": "test-tool"
        });

        let meta = StatelessRequestMeta::from_params(&params);
        assert!(meta.is_none());
    }

    #[test]
    fn test_has_client_capabilities() {
        let meta = StatelessRequestMeta {
            progress_token: None,
            protocol_version: None,
            session_id: None,
            client_capabilities: Some(ClientCapabilities::default()),
            roots: None,
            log_level: None,
        };
        assert!(meta.has_client_capabilities());

        let meta_without = StatelessRequestMeta::default();
        assert!(!meta_without.has_client_capabilities());
    }
}
