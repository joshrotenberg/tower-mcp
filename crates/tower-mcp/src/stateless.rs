//! Stateless MCP support: SEP-1442 opt-in and automatic 2026-07-28 dispatch
//!
//! This module contains types and helpers for stateless MCP operation. There are
//! two distinct paths; understanding which one applies to your deployment is
//! important.
//!
//! ## Two stateless paths
//!
//! ### Automatic version-gated path (2026-07-28+)
//!
//! When the `stateless` feature is compiled in, the HTTP transport automatically
//! dispatches any request carrying `MCP-Protocol-Version: 2026-07-28` (or later)
//! statelessly -- no session is created or looked up, and no initialize handshake
//! is required. Client identity and capabilities flow through per-request `_meta`
//! (see [`StatelessRequestMeta`]).
//!
//! **This path is always active when the feature is compiled in, regardless of
//! whether [`StatelessConfig`] was provided to the transport.** Adding
//! `features = ["stateless"]` to your `Cargo.toml` is sufficient; you do not
//! need to call `HttpTransport::stateless()`.
//!
//! ### Legacy SEP-1442 opt-in path
//!
//! [`StatelessConfig`] and `HttpTransport::stateless()` activate the older
//! SEP-1442-style opt-in stateless behavior for clients that do not use the
//! 2026-07-28 protocol. This path controls whether sessions are optional,
//! whether the protocol version must be present in every request, and whether
//! `server/discover` is enabled.
//!
//! ## Feature flag
//!
//! This module is gated behind the `stateless` feature flag:
//!
//! ```toml
//! tower-mcp = { version = "0.10", features = ["stateless"] }
//! ```
//!
//! ## Key properties of stateless mode
//!
//! 1. **Per-request protocol version**: Every request carries the protocol version
//! 2. **Optional discovery**: `server/discover` RPC for capability discovery (SEP-2575)
//! 3. **No sessions required**: Sessions are optional (2026-07-28) or configurable (SEP-1442)
//! 4. **Per-request client capabilities**: Client info and capabilities ride on each request

use serde::{Deserialize, Serialize};

use crate::protocol::{ClientCapabilities, Implementation, ProgressToken};

// =============================================================================
// Per-request _meta (SEP-2575 RequestMetaObject)
// =============================================================================

/// Per-request metadata carried in JSON-RPC `_meta` per SEP-2575.
///
/// In the 2026-07-28 protocol every request is self-contained -- there is no
/// initialize handshake and no session. The fields previously negotiated at
/// init time (protocol version, client identity, client capabilities) ride on
/// each request via the `_meta` object. The MCP-defined keys are reverse-DNS
/// namespaced under `io.modelcontextprotocol/`.
///
/// Wire shape (per the FINAL spec):
///
/// ```json
/// "_meta": {
///   "progressToken": "...",
///   "io.modelcontextprotocol/protocolVersion": "2026-07-28",
///   "io.modelcontextprotocol/clientInfo": { "name": "...", "version": "..." },
///   "io.modelcontextprotocol/clientCapabilities": { ... },
///   "io.modelcontextprotocol/logLevel": "info"
/// }
/// ```
///
/// Three of these are REQUIRED per the spec (`protocolVersion`, `clientInfo`,
/// `clientCapabilities`). They are typed as `Option<_>` here so deserialization
/// stays tolerant of partial/transitional clients; server-side validation
/// should reject requests that lack the required fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatelessRequestMeta {
    /// Progress token for receiving progress notifications.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_token: Option<ProgressToken>,

    /// The MCP protocol version this request targets.
    ///
    /// For HTTP transport this MUST match the `MCP-Protocol-Version` header
    /// (see SEP-2243). Spec key: `io.modelcontextprotocol/protocolVersion`.
    #[serde(
        rename = "io.modelcontextprotocol/protocolVersion",
        skip_serializing_if = "Option::is_none"
    )]
    pub protocol_version: Option<String>,

    /// Identifies the client software making this request. REQUIRED per
    /// SEP-2575 (the spec uses non-Option types; we keep `Option` for
    /// deserialization tolerance).
    #[serde(
        rename = "io.modelcontextprotocol/clientInfo",
        skip_serializing_if = "Option::is_none"
    )]
    pub client_info: Option<Implementation>,

    /// Capabilities the client advertises for this specific request.
    /// REQUIRED per SEP-2575.
    #[serde(
        rename = "io.modelcontextprotocol/clientCapabilities",
        skip_serializing_if = "Option::is_none"
    )]
    pub client_capabilities: Option<ClientCapabilities>,

    /// Optional per-request log-level override.
    #[serde(
        rename = "io.modelcontextprotocol/logLevel",
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

    /// Invalid or missing required session ID (-32001) per the SEP-1442
    /// draft.
    ///
    /// **Deprecated**: SEP-2243 (FINAL 2026-04-15) reassigns -32001 to
    /// `HeaderMismatch`. SEP-2567 (FINAL) also removes sessions
    /// entirely. New code should use one of:
    /// - [`crate::error::McpErrorCode::SessionRequired`] (-32006) for a
    ///   missing session ID, or
    /// - [`crate::error::McpErrorCode::SessionNotFound`] (-32005) for an
    ///   unknown or expired session.
    ///
    /// The numeric value of this constant is unchanged so any existing
    /// match arms keep compiling, but emitting -32001 from new code will
    /// confuse SEP-2243-aware clients.
    #[deprecated(
        since = "0.11.0",
        note = "SEP-2243 reclaims -32001 for HeaderMismatch. Use \
                McpErrorCode::SessionRequired (-32006) or SessionNotFound (-32005)."
    )]
    pub const INVALID_SESSION: i32 = -32001;
}

// =============================================================================
// Stateless mode configuration
// =============================================================================

/// Configuration for the legacy SEP-1442 stateless opt-in path.
///
/// **This configuration applies only to the legacy SEP-1442 opt-in path.**
/// It does NOT control the automatic version-gated path for 2026-07-28+
/// clients: when the `stateless` feature is compiled in, any request with
/// `MCP-Protocol-Version: 2026-07-28` and no `mcp-session-id` is dispatched
/// statelessly regardless of whether this config is set on the transport.
///
/// Use [`HttpTransport::stateless()`](crate::transport::http::HttpTransport::stateless)
/// to attach this configuration to a transport. Omitting it does not disable
/// stateless support for 2026-07-28+ clients; it only disables the
/// additional SEP-1442 opt-in behaviors below.
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

    /// Check if this request includes client identity.
    pub fn has_client_info(&self) -> bool {
        self.client_info.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_serializes_with_spec_keys() {
        use crate::protocol::{ClientCapabilities, Implementation};

        let meta = StatelessRequestMeta {
            progress_token: None,
            protocol_version: Some("2026-07-28".to_string()),
            client_info: Some(Implementation {
                name: "test-client".into(),
                version: "1.0.0".into(),
                title: None,
                description: None,
                icons: None,
                website_url: None,
                meta: None,
            }),
            client_capabilities: Some(ClientCapabilities::default()),
            log_level: Some(LogLevel::Info),
        };

        let json: serde_json::Value = serde_json::to_value(&meta).unwrap();
        // SEP-2575 reverse-DNS namespace `io.modelcontextprotocol/`.
        assert_eq!(
            json["io.modelcontextprotocol/protocolVersion"],
            "2026-07-28"
        );
        assert_eq!(
            json["io.modelcontextprotocol/clientInfo"]["name"],
            "test-client"
        );
        assert!(json["io.modelcontextprotocol/clientCapabilities"].is_object());
        assert_eq!(json["io.modelcontextprotocol/logLevel"], "info");

        // Confirm the dropped SEP-1442 draft keys are NOT emitted.
        assert!(
            json.get("modelcontextprotocol.io/mcpProtocolVersion")
                .is_none()
        );
        assert!(json.get("modelcontextprotocol.io/sessionId").is_none());
        assert!(json.get("io.modelcontextprotocol/sessionId").is_none());
        assert!(json.get("io.modelcontextprotocol/roots").is_none());
    }

    #[test]
    fn meta_deserializes_from_spec_keys() {
        let json = r#"{
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": {
                "name": "test-client",
                "version": "1.0.0"
            },
            "io.modelcontextprotocol/clientCapabilities": {},
            "io.modelcontextprotocol/logLevel": "debug"
        }"#;
        let meta: StatelessRequestMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta.protocol_version.as_deref(), Some("2026-07-28"));
        assert_eq!(meta.client_info.as_ref().unwrap().name, "test-client");
        assert!(meta.has_client_capabilities());
        assert!(meta.has_client_info());
        assert_eq!(meta.log_level, Some(LogLevel::Debug));
    }

    #[test]
    fn from_params_extracts_meta_object() {
        let params = serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28"
            },
            "other": "ignored"
        });
        let meta = StatelessRequestMeta::from_params(&params).expect("meta present");
        assert_eq!(meta.protocol_version.as_deref(), Some("2026-07-28"));
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
            client_info: None,
            client_capabilities: Some(ClientCapabilities::default()),
            log_level: None,
        };
        assert!(meta.has_client_capabilities());

        let meta_without = StatelessRequestMeta::default();
        assert!(!meta_without.has_client_capabilities());
    }

    #[test]
    fn legacy_sep1442_draft_keys_are_silently_ignored() {
        // Old `modelcontextprotocol.io/...` namespace keys from the SEP-1442
        // draft no longer match the spec. They deserialize as None (silently
        // dropped) -- existing tower-mcp clients that emit them will not
        // produce errors but the new fields will appear absent.
        let json = r#"{
            "modelcontextprotocol.io/mcpProtocolVersion": "2025-11-25",
            "modelcontextprotocol.io/sessionId": "old",
            "modelcontextprotocol.io/roots": []
        }"#;
        let meta: StatelessRequestMeta = serde_json::from_str(json).unwrap();
        assert!(meta.protocol_version.is_none());
        assert!(meta.client_info.is_none());
        assert!(meta.client_capabilities.is_none());
    }
}
