//! SEP-1442 Stateless MCP support (experimental)
//!
//! This module provides experimental support for stateless MCP as defined in
//! [SEP-1442](https://github.com/modelcontextprotocol/modelcontextprotocol/issues/1442).
//!
//! ## Feature Flag
//!
//! This module is gated behind the `stateless` feature flag:
//!
//! ```toml
//! tower-mcp = { version = "0.9", features = ["stateless"] }
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
//! SEP-1442 is still in-review. The API may change as the specification evolves.

use serde::{Deserialize, Serialize};

use crate::protocol::{ClientCapabilities, ProgressToken, Root};

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
    /// Progress token for receiving progress notifications.
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
    #[serde(
        rename = "modelcontextprotocol.io/logLevel",
        skip_serializing_if = "Option::is_none"
    )]
    pub log_level: Option<LogLevel>,
}

/// Log levels for stateless per-request log level control.
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

// SEP-1442's `server/discover` shape was promoted to the always-available
// `protocol` module when SEP-2575 finalized. Re-export from here so existing
// imports keep working; new code should import from `protocol` directly.

#[doc(inline)]
pub use crate::protocol::{DiscoverParams, DiscoverResult};

// =============================================================================
// Error codes -- the SEP-1442 draft assignments were wrong; SEP-2575 FINAL
// canonicalized them in the spec's draft schema. Re-export the corrected
// values from the always-available error module.
// =============================================================================

/// Re-exported for SEP-2575 compatibility. Prefer importing from
/// [`crate::error::McpErrorCode::UnsupportedProtocolVersion`] directly.
#[doc(inline)]
pub use crate::error::UnsupportedProtocolVersionData;

/// Legacy SEP-1442 error code constants.
///
/// These were wrong: SEP-1442 (draft) placed `UNSUPPORTED_VERSION` at -32000,
/// which collides with the established `ConnectionClosed` assignment.
/// SEP-2575 (FINAL) moved it to -32004. Use
/// [`crate::error::McpErrorCode::UnsupportedProtocolVersion`] or
/// [`crate::error::JsonRpcError::unsupported_protocol_version`] instead.
pub mod error_codes {
    /// Spec-correct unsupported protocol version code (SEP-2575).
    pub const UNSUPPORTED_PROTOCOL_VERSION: i32 = -32004;

    /// Wrong assignment from SEP-1442 draft. Kept temporarily for
    /// back-compat; new code should use [`UNSUPPORTED_PROTOCOL_VERSION`].
    #[deprecated(
        since = "0.12.0",
        note = "SEP-1442 draft assignment was wrong; SEP-2575 FINAL uses -32004. \
                Use `UNSUPPORTED_PROTOCOL_VERSION` or \
                `crate::error::McpErrorCode::UnsupportedProtocolVersion`."
    )]
    pub const UNSUPPORTED_VERSION: i32 = -32000;

    /// Invalid or missing required session ID (-32001) per SEP-1442.
    /// SEP-2567 (FINAL) removes sessions entirely; this constant is
    /// retained only for the 2025-11-25 protocol path.
    pub const INVALID_SESSION: i32 = -32001;
}

// =============================================================================
// Stateless mode configuration
// =============================================================================

/// Configuration for stateless MCP mode.
///
/// Controls which SEP-1442 features are enabled and their strictness.
#[derive(Debug, Clone)]
pub struct StatelessConfig {
    /// Whether to require protocol version in every request.
    ///
    /// Default: true (as per SEP-1442)
    pub require_protocol_version: bool,

    /// Whether sessions are optional.
    ///
    /// When true, requests without session IDs are allowed and
    /// the initialize handshake is not required.
    ///
    /// Default: true
    pub optional_sessions: bool,

    /// Whether to enable the `server/discover` RPC.
    ///
    /// Default: true
    pub enable_discover: bool,
}

impl Default for StatelessConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl StatelessConfig {
    /// Create a new stateless configuration with SEP-1442 defaults.
    pub fn new() -> Self {
        Self {
            require_protocol_version: true,
            optional_sessions: true,
            enable_discover: true,
        }
    }

    /// Create a configuration that maintains backward compatibility.
    ///
    /// This enables stateless features but doesn't require them,
    /// allowing gradual migration from stateful to stateless.
    pub fn backward_compatible() -> Self {
        Self {
            require_protocol_version: false,
            optional_sessions: true,
            enable_discover: true,
        }
    }
}

// =============================================================================
// Protocol version validation
// =============================================================================

/// Validate a protocol version string against supported versions.
///
/// Returns `Ok(())` if valid, or a JSON-RPC `UnsupportedProtocolVersion`
/// error (-32004 per SEP-2575) with the spec-shape data:
///
/// ```json
/// { "supported": ["..."], "requested": "..." }
/// ```
pub fn validate_protocol_version(
    version: &str,
) -> std::result::Result<(), crate::error::JsonRpcError> {
    use crate::protocol::SUPPORTED_PROTOCOL_VERSIONS;

    if SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
        Ok(())
    } else {
        Err(crate::error::JsonRpcError::unsupported_protocol_version(
            version,
            SUPPORTED_PROTOCOL_VERSIONS.iter().copied(),
        ))
    }
}

// =============================================================================
// Helper functions
// =============================================================================

impl StatelessRequestMeta {
    /// Extract stateless request metadata from JSON-RPC request params.
    ///
    /// The metadata is expected in the `_meta` field of the params object.
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
            protocol_version: Some("2025-11-25".to_string()),
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
            "modelcontextprotocol.io/mcpProtocolVersion": "2025-11-25",
            "modelcontextprotocol.io/sessionId": "test-session",
            "modelcontextprotocol.io/logLevel": "debug"
        }"#;

        let meta: StatelessRequestMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta.protocol_version, Some("2025-11-25".to_string()));
        assert_eq!(meta.session_id, Some("test-session".to_string()));
        assert_eq!(meta.log_level, Some(LogLevel::Debug));
    }

    #[test]
    fn test_discover_result_serialization() {
        use crate::protocol::{Implementation, ServerCapabilities};

        let result = DiscoverResult {
            supported_versions: vec!["2025-11-25".to_string(), "2025-03-26".to_string()],
            capabilities: ServerCapabilities::default(),
            server_info: Implementation {
                name: "test-server".to_string(),
                version: "1.0.0".to_string(),
                title: None,
                description: None,
                icons: None,
                website_url: None,
                meta: None,
            },
            instructions: Some("Test instructions".to_string()),
            meta: None,
        };

        let json = serde_json::to_value(&result).unwrap();
        assert!(json["supportedVersions"].is_array());
        assert_eq!(json["serverInfo"]["name"], "test-server");
    }

    #[test]
    fn test_unsupported_protocol_version_data_shape() {
        // Per SEP-2575 the wire shape is `supported`, not `supportedVersions`,
        // and the data also carries `requested`.
        let data = UnsupportedProtocolVersionData {
            supported: vec!["2026-07-28".to_string(), "2025-11-25".to_string()],
            requested: "2027-01-01".to_string(),
        };

        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json["supported"][0], "2026-07-28");
        assert_eq!(json["requested"], "2027-01-01");
        assert!(
            json.get("supportedVersions").is_none(),
            "spec field name is 'supported', not 'supportedVersions': {json}"
        );
    }

    #[test]
    fn test_config_defaults() {
        let config = StatelessConfig::new();
        assert!(config.require_protocol_version);
        assert!(config.optional_sessions);
        assert!(config.enable_discover);
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
                "modelcontextprotocol.io/mcpProtocolVersion": "2025-11-25",
                "modelcontextprotocol.io/sessionId": "session-123",
                "modelcontextprotocol.io/logLevel": "debug"
            }
        });

        let meta = StatelessRequestMeta::from_params(&params).unwrap();
        assert_eq!(meta.protocol_version, Some("2025-11-25".to_string()));
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
