//! MCP protocol types based on JSON-RPC 2.0
//!
//! These types follow the MCP specification (2025-11-25):
//! <https://modelcontextprotocol.io/specification/2025-11-25>

use std::collections::HashMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::JsonRpcError;

/// A protocol metadata key or extension declaration violated the MCP naming
/// rules.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetaValidationError {
    /// A `_meta` value was not a JSON object.
    ExpectedObject,
    /// An extension identifier omitted its mandatory vendor prefix.
    MissingExtensionPrefix(String),
    /// The prefix before `/` is malformed.
    InvalidPrefix(String),
    /// The name after `/` (or the whole unprefixed key) is malformed.
    InvalidName(String),
    /// An extension's settings value was not a JSON object.
    InvalidExtensionSettings(String),
}

impl std::fmt::Display for MetaValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExpectedObject => write!(f, "_meta must be a JSON object"),
            Self::MissingExtensionPrefix(key) => {
                write!(f, "extension identifier {key:?} requires a prefix")
            }
            Self::InvalidPrefix(key) => {
                write!(f, "metadata key {key:?} has an invalid prefix")
            }
            Self::InvalidName(key) => write!(f, "metadata key {key:?} has an invalid name"),
            Self::InvalidExtensionSettings(key) => {
                write!(f, "extension {key:?} settings must be a JSON object")
            }
        }
    }
}

impl std::error::Error for MetaValidationError {}

/// Validate one MCP `_meta` key.
///
/// A key consists of an optional prefix followed by a name. A prefix is one or
/// more dot-separated labels and ends in `/`; labels start with an ASCII
/// letter, end with an ASCII alphanumeric character, and contain only ASCII
/// alphanumerics or `-`. A non-empty name starts and ends with an ASCII
/// alphanumeric character and may additionally contain `-`, `_`, or `.`.
///
/// This checks syntax only. Prefixes whose second label is `mcp` or
/// `modelcontextprotocol` are reserved for MCP, but remain valid for the
/// protocol's own keys.
pub fn validate_meta_key(key: &str) -> Result<(), MetaValidationError> {
    let (prefix, name) = match key.split_once('/') {
        Some((prefix, name)) => (Some(prefix), name),
        None => (None, key),
    };

    if let Some(prefix) = prefix
        && (prefix.is_empty()
            || prefix.split('.').any(|label| {
                let bytes = label.as_bytes();
                bytes.is_empty()
                    || !bytes[0].is_ascii_alphabetic()
                    || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
                    || !bytes
                        .iter()
                        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
            }))
    {
        return Err(MetaValidationError::InvalidPrefix(key.to_string()));
    }

    let bytes = name.as_bytes();
    if !bytes.is_empty()
        && (!bytes[0].is_ascii_alphanumeric()
            || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
            || !bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.')))
    {
        return Err(MetaValidationError::InvalidName(key.to_string()));
    }

    Ok(())
}

/// Validate an MCP extension identifier.
///
/// Extension identifiers use the `_meta` key grammar, but the prefix is
/// mandatory.
pub fn validate_extension_identifier(identifier: &str) -> Result<(), MetaValidationError> {
    if !identifier.contains('/') {
        return Err(MetaValidationError::MissingExtensionPrefix(
            identifier.to_string(),
        ));
    }
    validate_meta_key(identifier)
}

/// Validate a JSON value used as an MCP `_meta` object.
pub fn validate_meta_object(meta: &Value) -> Result<(), MetaValidationError> {
    let object = meta
        .as_object()
        .ok_or(MetaValidationError::ExpectedObject)?;
    object.keys().try_for_each(|key| validate_meta_key(key))
}

/// Validate an MCP capability extension map.
///
/// In addition to the mandatory-prefix key rule, every extension value must be
/// a JSON settings object.
pub fn validate_extensions(extensions: &HashMap<String, Value>) -> Result<(), MetaValidationError> {
    for (identifier, settings) in extensions {
        validate_extension_identifier(identifier)?;
        if !settings.is_object() {
            return Err(MetaValidationError::InvalidExtensionSettings(
                identifier.clone(),
            ));
        }
    }
    Ok(())
}

pub(crate) mod meta_object_serde {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_json::Value;

    pub fn serialize<T, S>(meta: &Option<T>, serializer: S) -> Result<S::Ok, S::Error>
    where
        T: Serialize,
        S: Serializer,
    {
        if let Some(meta) = meta {
            let value = serde_json::to_value(meta).map_err(serde::ser::Error::custom)?;
            super::validate_meta_object(&value).map_err(serde::ser::Error::custom)?;
        }
        meta.serialize(serializer)
    }

    pub fn deserialize<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
    where
        T: serde::de::DeserializeOwned,
        D: Deserializer<'de>,
    {
        let value = Option::<Value>::deserialize(deserializer)?;
        let Some(value) = value else {
            return Ok(None);
        };
        super::validate_meta_object(&value).map_err(D::Error::custom)?;
        serde_json::from_value(value)
            .map(Some)
            .map_err(D::Error::custom)
    }
}

mod extension_map_serde {
    use std::collections::HashMap;

    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_json::Value;

    pub fn serialize<S>(
        extensions: &Option<HashMap<String, Value>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Some(extensions) = extensions {
            super::validate_extensions(extensions).map_err(serde::ser::Error::custom)?;
        }
        extensions.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<HashMap<String, Value>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let extensions = Option::<HashMap<String, Value>>::deserialize(deserializer)?;
        if let Some(extensions) = extensions.as_ref() {
            super::validate_extensions(extensions).map_err(D::Error::custom)?;
        }
        Ok(extensions)
    }
}

/// The JSON-RPC version. MUST be "2.0".
pub const JSONRPC_VERSION: &str = "2.0";

/// The latest protocol version enabled by default.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

/// Session protocol versions negotiable through `initialize` (newest first).
///
/// During initialization, the server negotiates the protocol version with the
/// client. The server picks the newest version that both sides support.
/// If no common version exists, the connection is rejected.
///
/// `2026-07-28` is deliberately not in this list even though it is stable:
/// it removed the `initialize` handshake, so it is never a valid outcome of
/// session negotiation. Builds that compile it (the `protocol-2026-07-28`
/// feature in `tower-mcp`) enable it through `ProtocolSupport`, which both
/// clients and servers default to the full compiled set.
///
/// ```rust
/// use tower_mcp_types::protocol::{LATEST_PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS};
///
/// assert_eq!(LATEST_PROTOCOL_VERSION, "2025-11-25");
/// assert!(SUPPORTED_PROTOCOL_VERSIONS.contains(&"2025-03-26"));
/// assert!(!SUPPORTED_PROTOCOL_VERSIONS.contains(&"2026-07-28"));
/// ```
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-03-26"];

/// The released 2026-07-28 protocol version.
///
/// This date-specific constant describes the wire revision without implying
/// compile-time or runtime enablement. In `tower-mcp`, compile the implementation
/// with the `protocol-2026-07-28` feature and select it for a particular runtime
/// with `ProtocolSupport`.
pub const PROTOCOL_VERSION_2026_07_28: &str = "2026-07-28";

/// All protocol versions understood by this types crate (newest first).
///
/// "Known" means the crate can represent at least part of the version's wire
/// format. It does not mean a runtime implementation was compiled or enabled.
pub const KNOWN_PROTOCOL_VERSIONS: &[&str] = &[
    PROTOCOL_VERSION_2026_07_28,
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
];

/// Deprecated implementation-status name for [`PROTOCOL_VERSION_2026_07_28`].
#[deprecated(
    since = "0.15.0",
    note = "the 2026-07-28 implementation is released and opt-in; use PROTOCOL_VERSION_2026_07_28"
)]
pub const EXPERIMENTAL_PROTOCOL_VERSION: &str = PROTOCOL_VERSION_2026_07_28;

/// Deprecated alias for [`PROTOCOL_VERSION_2026_07_28`].
#[deprecated(
    since = "0.15.0",
    note = "the 2026-07-28 spec has shipped; use PROTOCOL_VERSION_2026_07_28"
)]
pub const UPCOMING_PROTOCOL_VERSION: &str = PROTOCOL_VERSION_2026_07_28;

/// JSON-RPC 2.0 request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// JSON-RPC version, must be "2.0".
    pub jsonrpc: String,
    /// Request identifier.
    pub id: RequestId,
    /// Method name to invoke.
    pub method: String,
    /// Optional parameters for the method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    /// Create a new JSON-RPC request.
    pub fn new(id: impl Into<RequestId>, method: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.into(),
            method: method.into(),
            params: None,
        }
    }

    /// Add parameters to the request.
    pub fn with_params(mut self, params: Value) -> Self {
        self.params = Some(params);
        self
    }

    /// Validate that this request conforms to JSON-RPC 2.0.
    /// Returns an error if the jsonrpc version is not "2.0".
    pub fn validate(&self) -> Result<(), JsonRpcError> {
        if self.jsonrpc != JSONRPC_VERSION {
            return Err(JsonRpcError::invalid_request(format!(
                "Invalid JSON-RPC version: expected '{}', got '{}'",
                JSONRPC_VERSION, self.jsonrpc
            )));
        }
        Ok(())
    }
}

/// JSON-RPC 2.0 success response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResultResponse {
    /// JSON-RPC version, always "2.0".
    pub jsonrpc: String,
    /// Request identifier (matches the request).
    pub id: RequestId,
    /// The result value.
    pub result: Value,
}

/// JSON-RPC 2.0 error response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcErrorResponse {
    /// JSON-RPC version, always "2.0".
    pub jsonrpc: String,
    /// Request identifier. MUST be present per JSON-RPC 2.0; serialized as
    /// `null` when the id could not be determined (e.g. parse errors).
    #[serde(default)]
    pub id: Option<RequestId>,
    /// The error details.
    pub error: JsonRpcError,
}

/// JSON-RPC 2.0 response (either success or error).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum JsonRpcResponse {
    /// Successful response with result.
    Result(JsonRpcResultResponse),
    /// Error response.
    Error(JsonRpcErrorResponse),
}

impl JsonRpcResponse {
    /// Create a success response.
    pub fn result(id: RequestId, result: Value) -> Self {
        Self::Result(JsonRpcResultResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result,
        })
    }

    /// Create an error response.
    pub fn error(id: Option<RequestId>, error: JsonRpcError) -> Self {
        Self::Error(JsonRpcErrorResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            error,
        })
    }
}

/// JSON-RPC 2.0 message - can be a single request or a batch
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum JsonRpcMessage {
    /// A single request
    Single(JsonRpcRequest),
    /// A batch of requests
    Batch(Vec<JsonRpcRequest>),
}

impl JsonRpcMessage {
    /// Returns true if this is a batch message
    pub fn is_batch(&self) -> bool {
        matches!(self, JsonRpcMessage::Batch(_))
    }

    /// Returns the number of requests in this message
    pub fn len(&self) -> usize {
        match self {
            JsonRpcMessage::Single(_) => 1,
            JsonRpcMessage::Batch(batch) => batch.len(),
        }
    }

    /// Returns true if this message contains no requests
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// JSON-RPC 2.0 response message - can be a single response or a batch
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum JsonRpcResponseMessage {
    /// A single response
    Single(JsonRpcResponse),
    /// A batch of responses
    Batch(Vec<JsonRpcResponse>),
}

impl JsonRpcResponseMessage {
    /// Returns true if this is a batch response
    pub fn is_batch(&self) -> bool {
        matches!(self, JsonRpcResponseMessage::Batch(_))
    }
}

/// JSON-RPC 2.0 notification (no response expected)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    /// JSON-RPC version marker; constructors set this to [`JSONRPC_VERSION`].
    pub jsonrpc: String,
    /// Notification method name.
    pub method: String,
    /// Optional method parameters, omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcNotification {
    /// Create a notification without parameters.
    pub fn new(method: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params: None,
        }
    }

    /// Attach method parameters to this notification.
    pub fn with_params(mut self, params: Value) -> Self {
        self.params = Some(params);
        self
    }
}

/// MCP notification methods
pub mod notifications {
    /// Sent by client after receiving initialize response
    pub const INITIALIZED: &str = "notifications/initialized";
    /// Sent when a request is cancelled
    pub const CANCELLED: &str = "notifications/cancelled";
    /// Acknowledges a `subscriptions/listen` request and its accepted filter.
    pub const SUBSCRIPTIONS_ACKNOWLEDGED: &str = "notifications/subscriptions/acknowledged";
    /// Progress updates for long-running operations
    pub const PROGRESS: &str = "notifications/progress";
    /// Tool list has changed
    pub const TOOLS_LIST_CHANGED: &str = "notifications/tools/list_changed";
    /// Resource list has changed
    pub const RESOURCES_LIST_CHANGED: &str = "notifications/resources/list_changed";
    /// Specific resource has been updated
    pub const RESOURCE_UPDATED: &str = "notifications/resources/updated";
    /// Prompt list has changed
    pub const PROMPTS_LIST_CHANGED: &str = "notifications/prompts/list_changed";
    /// Roots list has changed (client to server)
    pub const ROOTS_LIST_CHANGED: &str = "notifications/roots/list_changed";
    /// Log message notification
    pub const MESSAGE: &str = "notifications/message";
    /// Task status changed
    pub const TASK_STATUS_CHANGED: &str = "notifications/tasks";
    /// Elicitation completed (for URL-based elicitation)
    pub const ELICITATION_COMPLETE: &str = "notifications/elicitation/complete";
}

/// Log severity levels following RFC 5424 (syslog)
///
/// Levels are ordered from most severe (emergency) to least severe (debug).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum LogLevel {
    /// System is unusable
    Emergency,
    /// Action must be taken immediately
    Alert,
    /// Critical conditions
    Critical,
    /// Error conditions
    Error,
    /// Warning conditions
    Warning,
    /// Normal but significant events
    Notice,
    /// General informational messages
    #[default]
    Info,
    /// Detailed debugging information
    Debug,
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogLevel::Emergency => write!(f, "emergency"),
            LogLevel::Alert => write!(f, "alert"),
            LogLevel::Critical => write!(f, "critical"),
            LogLevel::Error => write!(f, "error"),
            LogLevel::Warning => write!(f, "warning"),
            LogLevel::Notice => write!(f, "notice"),
            LogLevel::Info => write!(f, "info"),
            LogLevel::Debug => write!(f, "debug"),
        }
    }
}

/// Parameters for logging message notification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingMessageParams {
    /// Severity level of the message
    pub level: LogLevel,
    /// Optional logger name (e.g., "database", "auth", "tools")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logger: Option<String>,
    /// Structured data to be logged
    #[serde(default)]
    pub data: Value,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

impl LoggingMessageParams {
    /// Create a new logging message with the given level and data
    pub fn new(level: LogLevel, data: impl Into<Value>) -> Self {
        Self {
            level,
            logger: None,
            data: data.into(),
            meta: None,
        }
    }

    /// Set the logger name
    pub fn with_logger(mut self, logger: impl Into<String>) -> Self {
        self.logger = Some(logger.into());
        self
    }

    /// Set the structured data
    pub fn with_data(mut self, data: impl Into<Value>) -> Self {
        self.data = data.into();
        self
    }
}

/// Parameters for setting log level
#[derive(Debug, Clone, Deserialize)]
pub struct SetLogLevelParams {
    /// Minimum log level to receive
    pub level: LogLevel,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// Request ID - can be string or number per JSON-RPC spec
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum RequestId {
    /// A string request identifier.
    String(String),
    /// An integer request identifier.
    Number(i64),
}

impl From<String> for RequestId {
    fn from(s: String) -> Self {
        RequestId::String(s)
    }
}

impl From<&str> for RequestId {
    fn from(s: &str) -> Self {
        RequestId::String(s.to_string())
    }
}

impl From<i64> for RequestId {
    fn from(n: i64) -> Self {
        RequestId::Number(n)
    }
}

impl From<i32> for RequestId {
    fn from(n: i32) -> Self {
        RequestId::Number(n as i64)
    }
}

// =============================================================================
// MCP-specific request/response types
// =============================================================================

/// High-level MCP request (parsed from JSON-RPC)
// Variant sizes are dictated by the spec, not optimizable without API churn.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum McpRequest {
    /// Initialize session
    Initialize(InitializeParams),
    /// List available tools
    ListTools(ListToolsParams),
    /// Call a tool
    CallTool(CallToolParams),
    /// List available resources
    ListResources(ListResourcesParams),
    /// List resource templates
    ListResourceTemplates(ListResourceTemplatesParams),
    /// Read a resource
    ReadResource(ReadResourceParams),
    /// Subscribe to resource updates
    SubscribeResource(SubscribeResourceParams),
    /// Unsubscribe from resource updates
    UnsubscribeResource(UnsubscribeResourceParams),
    /// List available prompts
    ListPrompts(ListPromptsParams),
    /// Get a prompt
    GetPrompt(GetPromptParams),
    /// Get task info (`tasks/get`).
    GetTaskInfo(GetTaskInfoParams),
    /// Update an in-flight task with `inputResponses` (`tasks/update`).
    ///
    /// Introduced by SEP-2663 to replace the blocking `tasks/result` method
    /// from the 2025-11-25 experimental spec. The response is an empty ack;
    /// task state is observed via `tasks/get`.
    UpdateTask(UpdateTaskParams),
    /// Cancel a task (`tasks/cancel`).
    CancelTask(CancelTaskParams),
    /// Ping (keepalive)
    Ping,
    /// Set logging level
    SetLoggingLevel(SetLogLevelParams),
    /// Request completion suggestions
    Complete(CompleteParams),
    /// SEP-2575: discover server capabilities without an initialize handshake
    Discover(DiscoverParams),
    /// SEP-2575 / SEP-2567: open a server-to-client notification stream
    /// (`subscriptions/listen`).
    ///
    /// The HTTP transport intercepts this before it reaches the router and
    /// returns an SSE stream. The variant is defined here for client-side
    /// type-safe construction and for any future transport implementations.
    SubscriptionsListen(SubscriptionsListenParams),
    /// Unknown method
    Unknown {
        /// Unrecognized JSON-RPC method name.
        method: String,
        /// Raw parameters, if the request supplied them.
        params: Option<Value>,
    },
}

impl McpRequest {
    /// Get the method name for this request
    pub fn method_name(&self) -> &str {
        match self {
            McpRequest::Initialize(_) => "initialize",
            McpRequest::ListTools(_) => "tools/list",
            McpRequest::CallTool(_) => "tools/call",
            McpRequest::ListResources(_) => "resources/list",
            McpRequest::ListResourceTemplates(_) => "resources/templates/list",
            McpRequest::ReadResource(_) => "resources/read",
            McpRequest::SubscribeResource(_) => "resources/subscribe",
            McpRequest::UnsubscribeResource(_) => "resources/unsubscribe",
            McpRequest::ListPrompts(_) => "prompts/list",
            McpRequest::GetPrompt(_) => "prompts/get",
            McpRequest::GetTaskInfo(_) => "tasks/get",
            McpRequest::UpdateTask(_) => "tasks/update",
            McpRequest::CancelTask(_) => "tasks/cancel",
            McpRequest::Ping => "ping",
            McpRequest::SetLoggingLevel(_) => "logging/setLevel",
            McpRequest::Complete(_) => "completion/complete",
            McpRequest::Discover(_) => "server/discover",
            McpRequest::SubscriptionsListen(_) => "subscriptions/listen",
            McpRequest::Unknown { method, .. } => method,
        }
    }
}

/// High-level MCP notification (parsed from JSON-RPC)
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum McpNotification {
    /// Client has completed initialization
    Initialized,
    /// Request cancellation
    Cancelled(CancelledParams),
    /// Progress update
    Progress(ProgressParams),
    /// Roots list has changed (client to server)
    RootsListChanged,
    /// Unknown notification
    Unknown {
        /// Unrecognized notification method name.
        method: String,
        /// Raw parameters, if the notification supplied them.
        params: Option<Value>,
    },
}

/// Parameters for cancellation notification.
///
/// **Directionality changed in the final 2026-07-28 schema.** Through
/// 2025-11-25, either side could send this notification to cancel a
/// previously-issued request. The final 2026-07-28 schema restricts it to
/// client-to-server only (`requestId` MUST reference a request the client
/// issued); on stdio, the server may still send it, but solely to terminate
/// a `subscriptions/listen` stream by referencing that request's id -- it
/// MUST NOT use it to cancel any other request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelledParams {
    /// The ID of the request to cancel.
    ///
    /// Per the MCP spec, `requestId` MUST be present on
    /// `notifications/cancelled`. The field is `Option<RequestId>` for
    /// backward compatibility on the receive side -- if a peer sends a
    /// malformed notification without an id, deserialization still
    /// succeeds and we log and drop it. When `None`, the field
    /// serializes as `"requestId": null`, which a spec-strict receiver
    /// will reject (the correct behavior for a malformed send).
    #[serde(default)]
    pub request_id: Option<RequestId>,
    /// Optional reason for cancellation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// Parameters for progress notification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressParams {
    /// The progress token from the original request
    pub progress_token: ProgressToken,
    /// Current progress value (must increase with each notification)
    pub progress: f64,
    /// Total expected value (if known)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    /// Human-readable progress message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// Progress token - can be string or number
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum ProgressToken {
    /// A string progress token.
    String(String),
    /// An integer progress token.
    Number(i64),
}

/// Request metadata (`_meta`).
///
/// Carries the progress token (all versions) and, under 2026-07-28 (SEP-2575),
/// the per-request protocol version, client identity, client capabilities, and
/// log level that replaced the `initialize` handshake. Every 2026-07-28 key is
/// optional here so a single type serves both protocol versions: a 2025-11-25
/// request carries none of them and serializes byte-identically. Use
/// [`RequestMeta::validate_for_version`] to enforce the keys a version requires.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestMeta {
    /// Progress token for receiving progress notifications.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_token: Option<ProgressToken>,
    /// SEP-2575: the MCP protocol version for this request. Required on
    /// 2026-07-28; on the HTTP transport it MUST match the
    /// `MCP-Protocol-Version` header.
    #[serde(
        rename = "io.modelcontextprotocol/protocolVersion",
        skip_serializing_if = "Option::is_none"
    )]
    pub protocol_version: Option<String>,
    /// SEP-2575: self-reported client software identity. Optional.
    #[serde(
        rename = "io.modelcontextprotocol/clientInfo",
        skip_serializing_if = "Option::is_none"
    )]
    pub client_info: Option<Implementation>,
    /// SEP-2575: the client's capabilities for this specific request. Required
    /// on 2026-07-28; declared per-request rather than once at initialization.
    #[serde(
        rename = "io.modelcontextprotocol/clientCapabilities",
        skip_serializing_if = "Option::is_none"
    )]
    pub client_capabilities: Option<ClientCapabilities>,
    /// SEP-2575: desired log level for this request. Optional.
    ///
    /// Deprecated as of 2026-07-28 (SEP-2577); it replaced the `logging/setLevel`
    /// RPC. If absent, the server MUST NOT emit `notifications/message` for the
    /// request.
    #[serde(
        rename = "io.modelcontextprotocol/logLevel",
        skip_serializing_if = "Option::is_none"
    )]
    pub log_level: Option<LogLevel>,
}

impl RequestMeta {
    /// Validate that the `_meta` keys the given protocol version requires are
    /// present. Under 2026-07-28 (SEP-2575), `protocolVersion` and
    /// `clientCapabilities` are required; earlier versions carry neither and
    /// always pass.
    pub fn validate_for_version(&self, protocol_version: &str) -> Result<(), MissingMetaKey> {
        if protocol_version == PROTOCOL_VERSION_2026_07_28 {
            if self.protocol_version.is_none() {
                return Err(MissingMetaKey("io.modelcontextprotocol/protocolVersion"));
            }
            if self.client_capabilities.is_none() {
                return Err(MissingMetaKey("io.modelcontextprotocol/clientCapabilities"));
            }
        }
        Ok(())
    }
}

/// High-level MCP response
// Variant sizes are dictated by the spec, not optimizable without API churn.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum McpResponse {
    /// Successful `initialize` response.
    Initialize(InitializeResult),
    /// Successful `tools/list` response.
    ListTools(ListToolsResult),
    /// Successful `tools/call` response.
    CallTool(CallToolResult),
    /// SEP-2322 response indicating that the original request needs one or
    /// more client inputs before it can complete.
    InputRequired(InputRequiredResult),
    /// Successful `resources/list` response.
    ListResources(ListResourcesResult),
    /// Successful `resources/templates/list` response.
    ListResourceTemplates(ListResourceTemplatesResult),
    /// Successful `resources/read` response.
    ReadResource(ReadResourceResult),
    /// Empty acknowledgement for `resources/subscribe`.
    SubscribeResource(EmptyResult),
    /// Empty acknowledgement for `resources/unsubscribe`.
    UnsubscribeResource(EmptyResult),
    /// Successful `prompts/list` response.
    ListPrompts(ListPromptsResult),
    /// Successful `prompts/get` response.
    GetPrompt(GetPromptResult),
    // The final Tasks extension variants are declared before their legacy
    // counterparts on purpose. This enum is `untagged`, so deserialization
    // tries variants in declaration order, and each final type validates
    // `resultType` in a custom deserializer. Strict-first means a genuine
    // final payload matches its own variant and anything else falls through
    // to the legacy shape; the reverse order would let `TaskObject` silently
    // swallow a final `tasks/get` result.
    /// Flat `resultType: "task"` handle for the final Tasks extension
    /// (SEP-2663), as opposed to the legacy nested [`CreateTaskResult`].
    FinalCreateTask(crate::tasks::CreateTaskResult),
    /// Status-discriminated `tasks/get` result for the final Tasks extension
    /// (SEP-2663).
    FinalGetTask(crate::tasks::GetTaskResult),
    /// Complete acknowledgement for a final `tasks/update` or `tasks/cancel`
    /// (SEP-2663).
    ///
    /// One variant covers both methods because they produce byte-identical
    /// results; two untagged variants that cannot be distinguished would be
    /// worse than one that says so.
    FinalTaskAck(crate::tasks::TaskAcknowledgement),
    /// Legacy task-creation result used by the 2025-11-25 task shape.
    CreateTask(CreateTaskResult),
    /// Legacy task status result used by the 2025-11-25 task shape.
    GetTaskInfo(TaskObject),
    /// Ack-only response for `tasks/update` (SEP-2663).
    UpdateTask(EmptyResult),
    /// Ack-only response for `tasks/cancel` (SEP-2663).
    ///
    /// SEP-2663 (final) requires an empty ack; the observable task status is
    /// polled via `tasks/get` and may remain non-terminal after the ack.
    CancelTask(EmptyResult),
    /// Empty acknowledgement for `logging/setLevel`.
    SetLoggingLevel(EmptyResult),
    /// Successful `completion/complete` response.
    Complete(CompleteResult),
    /// Empty response to `ping`.
    Pong(EmptyResult),
    /// SEP-2575 `server/discover` response.
    Discover(DiscoverResult),
    /// Accepted filter from the pre-upgrade service pass of
    /// `subscriptions/listen`; consumed by the owning transport, never
    /// serialized as the request's wire response.
    SubscriptionsAccepted(SubscriptionsAcceptedResult),
    /// SEP-2575 / SEP-2567 `subscriptions/listen` response.
    ///
    /// In practice the HTTP transport returns an SSE stream for this method
    /// and never produces this variant. It is provided for completeness and
    /// for potential future transport implementations that want a typed
    /// result.
    SubscriptionsListen(SubscriptionsListenResult),
    /// Generic empty response for methods without a result body.
    Empty(EmptyResult),
    /// Raw JSON value for experimental/extension methods.
    Raw(Value),
}

// =============================================================================
// Initialize
// =============================================================================

/// Parameters sent by a client during the legacy `initialize` handshake.
///
/// Protocol 2026-07-28 replaces this handshake with per-request metadata;
/// this type remains the wire shape for earlier negotiated versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// Latest protocol version the client supports.
    pub protocol_version: String,
    /// Features the client can provide to the server.
    pub capabilities: ClientCapabilities,
    /// Client implementation name, version, and optional presentation data.
    pub client_info: Implementation,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// Features a client can provide or accept.
///
/// For protocol 2026-07-28 this value is carried in request `_meta`; earlier
/// versions send it once in [`InitializeParams`]. An absent capability means
/// the client does not advertise support for that feature.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientCapabilities {
    /// Support for server requests that list filesystem roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<RootsCapability>,
    /// Support for server-initiated model sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<SamplingCapability>,
    /// Support for server-initiated user elicitation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elicitation: Option<ElicitationCapability>,
    /// Support for the legacy client-side task capability shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tasks: Option<ClientTasksCapability>,
    /// Experimental, non-standard capabilities
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<HashMap<String, serde_json::Value>>,
    /// Declared extension support (SEP-1724/SEP-2133).
    ///
    /// Keys MUST follow the `_meta` key naming rules (reverse-DNS prefix
    /// mandatory, e.g. `io.modelcontextprotocol/tasks`) per the final
    /// 2026-07-28 schema.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "extension_map_serde"
    )]
    pub extensions: Option<HashMap<String, serde_json::Value>>,
}

/// Client capability for elicitation (requesting user input)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ElicitationCapability {
    /// Support for form-based elicitation
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form: Option<ElicitationFormCapability>,
    /// Support for URL-based elicitation
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<ElicitationUrlCapability>,
}

/// Marker for form-based elicitation support
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ElicitationFormCapability {}

/// Marker for URL-based elicitation support
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ElicitationUrlCapability {}

/// Legacy 2025-11-25 client capability for async task management.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientTasksCapability {
    /// Support for listing tasks
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<ClientTasksListCapability>,
    /// Support for cancelling tasks
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<ClientTasksCancelCapability>,
    /// Legacy task-augmented request support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<ClientTasksRequestsCapability>,
}

/// Marker capability for client tasks/list support
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientTasksListCapability {}

/// Marker capability for client tasks/cancel support
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientTasksCancelCapability {}

/// Legacy client capability declaring task-augmented request support.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientTasksRequestsCapability {
    /// Task support for sampling-related requests
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<ClientTasksSamplingCapability>,
    /// Task support for elicitation-related requests
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elicitation: Option<ClientTasksElicitationCapability>,
}

/// Legacy capability for task-augmented sampling requests.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientTasksSamplingCapability {
    /// Whether the client supports task-augmented sampling/createMessage requests
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_message: Option<ClientTasksSamplingCreateMessageCapability>,
}

/// Legacy task-augmented sampling marker.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientTasksSamplingCreateMessageCapability {}

/// Legacy capability for task-augmented elicitation requests.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientTasksElicitationCapability {
    /// Whether the client supports task-augmented elicitation/create requests
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create: Option<ClientTasksElicitationCreateCapability>,
}

/// Legacy task-augmented elicitation marker.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientTasksElicitationCreateCapability {}

/// Client capability for roots (filesystem access)
/// Capabilities related to filesystem roots.
///
/// When `list_changed` is `true`, the client supports sending
/// `notifications/roots/list_changed` to inform the server that the
/// available roots have been modified. Servers should re-request the
/// roots list when they receive this notification.
///
/// # Example
///
/// ```rust
/// use tower_mcp_types::protocol::RootsCapability;
///
/// // Client advertising roots with change notification support
/// let cap = RootsCapability { list_changed: true, deprecated: None };
/// assert!(cap.list_changed);
///
/// // The notification method is defined as a constant:
/// assert_eq!(
///     tower_mcp_types::protocol::notifications::ROOTS_LIST_CHANGED,
///     "notifications/roots/list_changed"
/// );
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RootsCapability {
    /// Whether the client supports roots list changed notifications
    #[serde(default)]
    pub list_changed: bool,
    /// SEP-2577: roots is deprecated in the 2026-07-28 protocol with a
    /// 12-month minimum support window. Servers that advertise roots
    /// can attach a `DeprecationInfo` to signal to clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<DeprecationInfo>,
}

/// Represents a root directory or file that the server can operate on.
///
/// Roots allow clients to expose filesystem roots to servers, enabling:
/// - Scoped file access
/// - Workspace awareness
/// - Security boundaries
///
/// # Example
///
/// ```rust
/// use tower_mcp_types::protocol::Root;
///
/// let root = Root::new("file:///home/user/project");
/// assert_eq!(root.uri, "file:///home/user/project");
/// assert!(root.name.is_none());
///
/// let root = Root::with_name("file:///workspace", "My Project");
/// assert_eq!(root.name.unwrap(), "My Project");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Root {
    /// The URI identifying the root. Must start with `file://` for now.
    pub uri: String,
    /// Optional human-readable name for the root
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

impl Root {
    /// Create a new root with just a URI
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            name: None,
            meta: None,
        }
    }

    /// Create a new root with a URI and name
    pub fn with_name(uri: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            name: Some(name.into()),
            meta: None,
        }
    }
}

/// Result of a roots/list request from the server.
///
/// Contains the list of roots the client has exposed. Clients notify
/// the server of root changes via `notifications/roots/list_changed`.
///
/// Parameters for roots/list request (server to client)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRootsParams {
    /// Optional metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// Result of roots/list request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListRootsResult {
    /// The list of roots available to the server
    pub roots: Vec<Root>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// Client support for server-initiated sampling requests.
///
/// Sampling is deprecated in protocol 2026-07-28 by SEP-2577 but remains
/// available during its compatibility window.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SamplingCapability {
    /// Support for tool use within sampling
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<SamplingToolsCapability>,
    /// Support for context inclusion within sampling
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<SamplingContextCapability>,
    /// SEP-2577: sampling is deprecated in the 2026-07-28 protocol with
    /// a 12-month minimum support window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<DeprecationInfo>,
}

/// Marker capability for tool use within sampling
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SamplingToolsCapability {}

/// Marker capability for context inclusion within sampling
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SamplingContextCapability {}

/// Server capability for providing completions
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompletionsCapability {}

// =============================================================================
// Completion Types
// =============================================================================

/// Reference to a prompt for completion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptReference {
    /// Type discriminator, always "ref/prompt"
    #[serde(rename = "type")]
    pub ref_type: String,
    /// The name of the prompt or prompt template
    pub name: String,
}

impl PromptReference {
    /// Create a new prompt reference
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            ref_type: "ref/prompt".to_string(),
            name: name.into(),
        }
    }
}

/// Reference to a resource for completion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceReference {
    /// Type discriminator, always "ref/resource"
    #[serde(rename = "type")]
    pub ref_type: String,
    /// The URI or URI template of the resource
    pub uri: String,
}

impl ResourceReference {
    /// Create a new resource reference
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            ref_type: "ref/resource".to_string(),
            uri: uri.into(),
        }
    }
}

/// Reference for completion - either a prompt or resource reference
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum CompletionReference {
    /// Reference to a prompt
    #[serde(rename = "ref/prompt")]
    Prompt {
        /// The name of the prompt
        name: String,
    },
    /// Reference to a resource
    #[serde(rename = "ref/resource")]
    Resource {
        /// The URI of the resource
        uri: String,
    },
}

impl CompletionReference {
    /// Create a prompt reference
    pub fn prompt(name: impl Into<String>) -> Self {
        Self::Prompt { name: name.into() }
    }

    /// Create a resource reference
    pub fn resource(uri: impl Into<String>) -> Self {
        Self::Resource { uri: uri.into() }
    }
}

/// Argument being completed
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionArgument {
    /// The name of the argument
    pub name: String,
    /// The current value of the argument (partial input)
    pub value: String,
}

impl CompletionArgument {
    /// Create a new completion argument
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// Parameters for completion/complete request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteParams {
    /// The reference (prompt or resource) being completed
    #[serde(rename = "ref")]
    pub reference: CompletionReference,
    /// The argument being completed
    pub argument: CompletionArgument,
    /// Additional context for completion, such as previously resolved argument values
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<CompletionContext>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// Context provided alongside a completion request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionContext {
    /// Previously resolved argument name-value pairs
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<std::collections::HashMap<String, String>>,
}

/// Completion suggestions
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Completion {
    /// Suggested completion values
    pub values: Vec<String>,
    /// Total number of available completions (if known)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    /// Whether there are more completions available
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
}

impl Completion {
    /// Create a new completion result
    pub fn new(values: Vec<String>) -> Self {
        Self {
            values,
            total: None,
            has_more: None,
        }
    }

    /// Create a completion result with pagination info
    pub fn with_pagination(values: Vec<String>, total: u32, has_more: bool) -> Self {
        Self {
            values,
            total: Some(total),
            has_more: Some(has_more),
        }
    }
}

/// Result of completion/complete request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteResult {
    /// The completion suggestions
    pub completion: Completion,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

impl CompleteResult {
    /// Create a new completion result
    pub fn new(values: Vec<String>) -> Self {
        Self {
            completion: Completion::new(values),
            meta: None,
        }
    }

    /// Create a completion result with pagination info
    pub fn with_pagination(values: Vec<String>, total: u32, has_more: bool) -> Self {
        Self {
            completion: Completion::with_pagination(values, total, has_more),
            meta: None,
        }
    }
}

// =============================================================================
// Sampling Types
// =============================================================================

/// Hint for model selection
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelHint {
    /// Suggested model name (partial match allowed)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ModelHint {
    /// Create a new model hint
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
        }
    }
}

/// Preferences for model selection during sampling
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPreferences {
    /// Priority for response speed (0.0 to 1.0)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed_priority: Option<f64>,
    /// Priority for model intelligence/capability (0.0 to 1.0)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intelligence_priority: Option<f64>,
    /// Priority for cost efficiency (0.0 to 1.0)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_priority: Option<f64>,
    /// Hints for model selection
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<ModelHint>,
}

impl ModelPreferences {
    /// Create new model preferences
    pub fn new() -> Self {
        Self::default()
    }

    /// Set speed priority (0.0 to 1.0)
    pub fn speed(mut self, priority: f64) -> Self {
        self.speed_priority = Some(priority.clamp(0.0, 1.0));
        self
    }

    /// Set intelligence priority (0.0 to 1.0)
    pub fn intelligence(mut self, priority: f64) -> Self {
        self.intelligence_priority = Some(priority.clamp(0.0, 1.0));
        self
    }

    /// Set cost priority (0.0 to 1.0)
    pub fn cost(mut self, priority: f64) -> Self {
        self.cost_priority = Some(priority.clamp(0.0, 1.0));
        self
    }

    /// Add a model hint
    pub fn hint(mut self, name: impl Into<String>) -> Self {
        self.hints.push(ModelHint::new(name));
        self
    }
}

/// Context inclusion mode for sampling
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum IncludeContext {
    /// Include context from all connected MCP servers
    AllServers,
    /// Include context from this server only
    ThisServer,
    /// Don't include any additional context
    #[default]
    None,
}

/// Message for sampling request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SamplingMessage {
    /// The role of the message sender
    pub role: ContentRole,
    /// The content of the message (single item or array)
    pub content: SamplingContentOrArray,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

impl SamplingMessage {
    /// Create a user message with text content
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: ContentRole::User,
            content: SamplingContentOrArray::Single(SamplingContent::Text {
                text: text.into(),
                annotations: None,
                meta: None,
            }),
            meta: None,
        }
    }

    /// Create an assistant message with text content
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: ContentRole::Assistant,
            content: SamplingContentOrArray::Single(SamplingContent::Text {
                text: text.into(),
                annotations: None,
                meta: None,
            }),
            meta: None,
        }
    }
}

/// Tool definition for use in sampling requests (SEP-1577)
///
/// The MCP spec uses the full `Tool` type for sampling tools.
/// This struct mirrors `ToolDefinition` with all optional fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SamplingTool {
    /// The name of the tool
    pub name: String,
    /// Human-readable title for display purposes
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Description of what the tool does
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema describing the tool's input parameters
    pub input_schema: Value,
    /// Optional JSON Schema defining expected output structure
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// Optional icons for display in user interfaces
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icons: Option<Vec<ToolIcon>>,
    /// Optional annotations describing tool behavior
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
    /// Optional execution configuration for task support
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ToolExecution>,
}

/// Tool choice mode for sampling requests (SEP-1577)
///
/// Controls how the LLM should use the available tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolChoice {
    /// The tool choice mode: "auto", "none", or "tool"
    pub mode: String,
    /// Name of the specific tool to use (required when mode is "tool")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ToolChoice {
    /// Model decides whether to use tools
    pub fn auto() -> Self {
        Self {
            mode: "auto".to_string(),
            name: None,
        }
    }

    /// Model must use a tool
    pub fn required() -> Self {
        Self {
            mode: "required".to_string(),
            name: None,
        }
    }

    /// Model should not use tools
    pub fn none() -> Self {
        Self {
            mode: "none".to_string(),
            name: None,
        }
    }

    /// Force the model to use a specific tool by name
    pub fn tool(name: impl Into<String>) -> Self {
        Self {
            mode: "tool".to_string(),
            name: Some(name.into()),
        }
    }
}

/// Content types for sampling messages
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
#[non_exhaustive]
pub enum SamplingContent {
    /// Text content
    Text {
        /// The text content
        text: String,
        /// Optional annotations for this content
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
        /// Optional protocol-level metadata
        #[serde(
            rename = "_meta",
            default,
            skip_serializing_if = "Option::is_none",
            with = "crate::protocol::meta_object_serde"
        )]
        meta: Option<Value>,
    },
    /// Image content
    Image {
        /// Base64-encoded image data
        data: String,
        /// MIME type of the image
        #[serde(rename = "mimeType")]
        mime_type: String,
        /// Optional annotations for this content
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
        /// Optional protocol-level metadata
        #[serde(
            rename = "_meta",
            default,
            skip_serializing_if = "Option::is_none",
            with = "crate::protocol::meta_object_serde"
        )]
        meta: Option<Value>,
    },
    /// Audio content (if supported)
    Audio {
        /// Base64-encoded audio data
        data: String,
        /// MIME type of the audio
        #[serde(rename = "mimeType")]
        mime_type: String,
        /// Optional annotations for this content
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
        /// Optional protocol-level metadata
        #[serde(
            rename = "_meta",
            default,
            skip_serializing_if = "Option::is_none",
            with = "crate::protocol::meta_object_serde"
        )]
        meta: Option<Value>,
    },
    /// Tool use request from the model (SEP-1577)
    #[serde(rename = "tool_use")]
    ToolUse {
        /// Unique identifier for this tool use
        id: String,
        /// Name of the tool being called
        name: String,
        /// Input arguments for the tool
        input: Value,
        /// Optional protocol-level metadata
        #[serde(
            rename = "_meta",
            default,
            skip_serializing_if = "Option::is_none",
            with = "crate::protocol::meta_object_serde"
        )]
        meta: Option<Value>,
    },
    /// Result of a tool invocation (SEP-1577)
    #[serde(rename = "tool_result")]
    ToolResult {
        /// ID of the tool use this result corresponds to
        #[serde(rename = "toolUseId")]
        tool_use_id: String,
        /// The tool result content
        content: Vec<SamplingContent>,
        /// Structured content from the tool result
        #[serde(
            default,
            rename = "structuredContent",
            skip_serializing_if = "Option::is_none"
        )]
        structured_content: Option<Value>,
        /// Whether the tool execution resulted in an error
        #[serde(default, rename = "isError", skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        /// Optional protocol-level metadata
        #[serde(
            rename = "_meta",
            default,
            skip_serializing_if = "Option::is_none",
            with = "crate::protocol::meta_object_serde"
        )]
        meta: Option<Value>,
    },
}

impl SamplingContent {
    /// Get the text content if this is a text variant.
    ///
    /// Returns `None` if this is an image, audio, tool_use, or tool_result variant.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::protocol::SamplingContent;
    ///
    /// let text_content = SamplingContent::Text { text: "Hello".into(), annotations: None, meta: None };
    /// assert_eq!(text_content.as_text(), Some("Hello"));
    ///
    /// let image_content = SamplingContent::Image {
    ///     data: "base64...".into(),
    ///     mime_type: "image/png".into(),
    ///     annotations: None,
    ///     meta: None,
    /// };
    /// assert_eq!(image_content.as_text(), None);
    /// ```
    pub fn as_text(&self) -> Option<&str> {
        match self {
            SamplingContent::Text { text, .. } => Some(text),
            _ => None,
        }
    }
}

/// Content that can be either a single item or an array
///
/// The MCP spec allows content fields to be either a single
/// SamplingContent or an array of SamplingContent items.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum SamplingContentOrArray {
    /// Single content item
    Single(SamplingContent),
    /// Array of content items
    Array(Vec<SamplingContent>),
}

impl SamplingContentOrArray {
    /// Get content items as a slice
    pub fn items(&self) -> Vec<&SamplingContent> {
        match self {
            Self::Single(c) => vec![c],
            Self::Array(arr) => arr.iter().collect(),
        }
    }

    /// Get owned content items
    pub fn into_items(self) -> Vec<SamplingContent> {
        match self {
            Self::Single(c) => vec![c],
            Self::Array(arr) => arr,
        }
    }
}

/// Parameters for sampling/createMessage request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateMessageParams {
    /// The messages to send to the LLM
    pub messages: Vec<SamplingMessage>,
    /// Maximum number of tokens to generate
    pub max_tokens: u32,
    /// Optional system prompt
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Sampling temperature (0.0 to 1.0)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Stop sequences
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    /// Model preferences
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_preferences: Option<ModelPreferences>,
    /// Context inclusion mode
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_context: Option<IncludeContext>,
    /// Additional metadata
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
    /// Tools available for the model to use (SEP-1577)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<SamplingTool>>,
    /// Tool choice mode (SEP-1577)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Legacy 2025-11-25 task parameters for async execution.
    ///
    /// This field is invalid on the 2026-07-28 protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskRequestParams>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

impl CreateMessageParams {
    /// Create a new sampling request
    pub fn new(messages: Vec<SamplingMessage>, max_tokens: u32) -> Self {
        Self {
            messages,
            max_tokens,
            system_prompt: None,
            temperature: None,
            stop_sequences: Vec::new(),
            model_preferences: None,
            include_context: None,
            metadata: None,
            tools: None,
            tool_choice: None,
            task: None,
            meta: None,
        }
    }

    /// Set the system prompt
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Set the temperature
    pub fn temperature(mut self, temp: f64) -> Self {
        self.temperature = Some(temp.clamp(0.0, 1.0));
        self
    }

    /// Add a stop sequence
    pub fn stop_sequence(mut self, seq: impl Into<String>) -> Self {
        self.stop_sequences.push(seq.into());
        self
    }

    /// Set model preferences
    pub fn model_preferences(mut self, prefs: ModelPreferences) -> Self {
        self.model_preferences = Some(prefs);
        self
    }

    /// Set context inclusion mode
    pub fn include_context(mut self, mode: IncludeContext) -> Self {
        self.include_context = Some(mode);
        self
    }

    /// Set tools available for the model to use (SEP-1577)
    pub fn tools(mut self, tools: Vec<SamplingTool>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Set tool choice mode (SEP-1577)
    pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
        self.tool_choice = Some(choice);
        self
    }
}

/// Result of sampling/createMessage request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateMessageResult {
    /// The generated content (single item or array)
    pub content: SamplingContentOrArray,
    /// The model that generated the response
    pub model: String,
    /// The role of the response (always assistant)
    pub role: ContentRole,
    /// Why the generation stopped
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

impl CreateMessageResult {
    /// Get content items as a vector of references
    pub fn content_items(&self) -> Vec<&SamplingContent> {
        self.content.items()
    }

    /// Get the text from the first text content item.
    ///
    /// Returns `None` if there are no content items or if the first
    /// text-containing item is not found.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::protocol::{CreateMessageResult, SamplingContent, SamplingContentOrArray, ContentRole};
    ///
    /// let result = CreateMessageResult {
    ///     content: SamplingContentOrArray::Single(SamplingContent::Text {
    ///         text: "Hello, world!".into(),
    ///         annotations: None,
    ///         meta: None,
    ///     }),
    ///     model: "claude-3".into(),
    ///     role: ContentRole::Assistant,
    ///     stop_reason: None,
    ///     meta: None,
    /// };
    /// assert_eq!(result.first_text(), Some("Hello, world!"));
    /// ```
    pub fn first_text(&self) -> Option<&str> {
        self.content.items().iter().find_map(|c| c.as_text())
    }
}

/// Information about a client or server implementation
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Implementation {
    /// Name of the implementation
    pub name: String,
    /// Version of the implementation
    pub version: String,
    /// Human-readable title for display purposes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Description of the implementation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Icons for the implementation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icons: Option<Vec<ToolIcon>>,
    /// URL of the implementation's website
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website_url: Option<String>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// Server response to the legacy `initialize` handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    /// Protocol version selected by the server.
    pub protocol_version: String,
    /// Features the server advertises for the negotiated session.
    pub capabilities: ServerCapabilities,
    /// Server implementation name, version, and optional presentation data.
    pub server_info: Implementation,
    /// Optional instructions describing how to use this server.
    /// These hints help LLMs understand the server's features.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

// =============================================================================
// server/discover (SEP-2575)
// =============================================================================

/// Parameters for the `server/discover` RPC (SEP-2575).
///
/// `server/discover` lets clients fetch server capabilities, supported
/// protocol versions, and implementation info **without** establishing a
/// session or going through the initialize handshake. It is the stateless
/// replacement for `initialize` in the 2026-07-28 protocol.
///
/// The final lifecycle carries the same required per-request metadata on
/// discovery that it carries on every subsequent request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscoverParams {
    /// Required per-request metadata for the final protocol.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// Result of the `server/discover` RPC (SEP-2575).
///
/// Shape mirrors `InitializeResult` with the singular `protocol_version`
/// replaced by `supported_versions` -- a `server/discover` call is
/// version-independent, so the server enumerates every version it can
/// speak and the client picks.
///
/// The final 2026-07-28 schema dropped the `serverInfo` body field (server
/// identity now lives in `_meta["io.modelcontextprotocol/serverInfo"]`, via
/// [`ResultMeta`]) and made this a `CacheableResult`-shaped type (the spec's
/// name for the interface; this crate has no such Rust type -- each
/// cacheable result inlines its own `ttl_ms`/`cache_scope` fields). This
/// crate keeps `ttl_ms`/`cache_scope` as `Option` rather than the spec's
/// required `number`/`string` -- the same "`None` means no opinion"
/// convention used by every other cacheable result in this file (see
/// [`CacheScope`]) -- rather than a one-off required-field type here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverResult {
    /// All protocol versions this server can speak. The client picks one
    /// and signals it via `MCP-Protocol-Version` on subsequent requests.
    pub supported_versions: Vec<String>,
    /// Server capabilities (same shape as the `initialize` result).
    pub capabilities: ServerCapabilities,
    /// SEP-2549: client-cache TTL in milliseconds for this response.
    /// `None` means "no opinion -- client policy decides".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    /// SEP-2549: scope the cached result applies to. `None` means scope is
    /// unspecified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_scope: Option<CacheScope>,
    /// Optional instructions describing how to use this server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Protocol-level metadata. Carries server identity
    /// (`io.modelcontextprotocol/serverInfo`) per SEP-2575.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<ResultMeta>,
}

// =============================================================================
// subscriptions/listen (SEP-2575 / SEP-2567)
// =============================================================================

/// Parameters for the `subscriptions/listen` RPC (SEP-2575 / SEP-2567).
///
/// `subscriptions/listen` is sent by the client over HTTP POST to open a
/// server-to-client notification stream (SSE). The server responds with
/// `Content-Type: text/event-stream` and streams zero or more
/// `notifications/*` events until the client disconnects.
///
/// Under SEP-2567 (sessionless), the stream lifetime is scoped to the single
/// request. Only servers whose negotiated protocol version is >= 2026-07-28
/// enable this path; older servers return a `Method Not Found` (-32601) error.
///
/// The struct takes no parameters today; the empty struct is reserved so
/// future SEPs can add optional fields without breaking callers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubscriptionsListenParams {
    /// SEP-2575: the notification types the client opts in to on this stream.
    /// The draft schema requires this field; it is optional here so existing
    /// callers keep compiling, and the transport enforces presence on the
    /// 2026-07-28 path (#952).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notifications: Option<SubscriptionFilter>,
    /// Optional protocol-level metadata.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// Accepted-filter result of the pre-upgrade service pass for
/// `subscriptions/listen` (SEP-2575).
///
/// This is not a wire result. Transports dispatch the listen request through
/// the JSON-RPC service before upgrading the connection into a stream, so
/// `Service<RouterRequest>` middleware observes accepted and rejected listens
/// like any other request; the transport then consumes this result to
/// register the stream and emit `notifications/subscriptions/acknowledged`.
/// The request's eventual wire response remains the graceful-close
/// [`SubscriptionsListenResult`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionsAcceptedResult {
    /// The notification types and task IDs the server agreed to honor.
    pub notifications: SubscriptionFilter,
}

/// Graceful final result of the `subscriptions/listen` RPC.
///
/// A server sends this empty complete result before it closes a subscription
/// stream on its own initiative. An abrupt transport close has no result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionsListenResult {
    /// Always [`ResultType::Complete`].
    #[serde(default)]
    pub result_type: ResultType,
    /// Required result metadata identifying the subscription that ended.
    #[serde(rename = "_meta")]
    pub meta: SubscriptionsListenResultMeta,
}

/// Result metadata for a gracefully closed `subscriptions/listen` request.
///
/// Unlike [`NotificationMeta`], the subscription ID is required because this
/// metadata appears only on the terminal result of a known subscription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionsListenResultMeta {
    /// The JSON-RPC ID of the `subscriptions/listen` request being closed.
    #[serde(rename = "io.modelcontextprotocol/subscriptionId")]
    pub subscription_id: RequestId,
    /// Identifies the server software producing the response.
    #[serde(
        rename = "io.modelcontextprotocol/serverInfo",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub server_info: Option<Implementation>,
}

/// Features exposed by an MCP server.
///
/// An absent capability means the server does not advertise the corresponding
/// operation. Protocol 2026-07-28 returns this from `server/discover`; earlier
/// versions return it from `initialize`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    /// Tool listing and invocation support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
    /// Resource listing, reading, and optional subscription support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourcesCapability>,
    /// Prompt listing and retrieval support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<PromptsCapability>,
    /// Logging capability - servers that emit log notifications declare this
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<LoggingCapability>,
    /// Legacy Tasks capability advertised by pre-final protocol versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tasks: Option<TasksCapability>,
    /// Completion capability - server provides autocomplete suggestions
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completions: Option<CompletionsCapability>,
    /// Experimental, non-standard capabilities
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<HashMap<String, serde_json::Value>>,
    /// Declared extension support (SEP-1724/SEP-2133).
    ///
    /// Keys MUST follow the `_meta` key naming rules (reverse-DNS prefix
    /// mandatory, e.g. `io.modelcontextprotocol/tasks`) per the final
    /// 2026-07-28 schema.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "extension_map_serde"
    )]
    pub extensions: Option<HashMap<String, serde_json::Value>>,
}

/// Logging capability declaration
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoggingCapability {
    /// SEP-2577: logging is deprecated in the 2026-07-28 protocol with
    /// a 12-month minimum support window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<DeprecationInfo>,
}

/// Legacy 2025-11-25 server capability for async task management.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TasksCapability {
    /// Support for listing tasks
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<TasksListCapability>,
    /// Support for cancelling tasks
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<TasksCancelCapability>,
    /// Legacy task-augmented request support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<TasksRequestsCapability>,
}

/// Marker capability for tasks/list support
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TasksListCapability {}

/// Marker capability for tasks/cancel support
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TasksCancelCapability {}

/// Legacy server capability declaring task-augmented request support.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TasksRequestsCapability {
    /// Task support for tool-related requests
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<TasksToolsRequestsCapability>,
}

/// Legacy capability for task-augmented tool requests.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TasksToolsRequestsCapability {
    /// Whether the server supports task-augmented tools/call requests
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<TasksToolsCallCapability>,
}

/// Legacy task-augmented tools/call marker.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TasksToolsCallCapability {}

/// Options advertised for the server's tool capability.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCapability {
    /// Whether the server may emit `notifications/tools/list_changed`.
    #[serde(default)]
    pub list_changed: bool,
}

/// Options advertised for the server's resource capability.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesCapability {
    /// Whether `resources/subscribe` and `resources/unsubscribe` are supported.
    #[serde(default)]
    pub subscribe: bool,
    /// Whether the server may emit `notifications/resources/list_changed`.
    #[serde(default)]
    pub list_changed: bool,
}

/// Options advertised for the server's prompt capability.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptsCapability {
    /// Whether the server may emit `notifications/prompts/list_changed`.
    #[serde(default)]
    pub list_changed: bool,
}

// =============================================================================
// Lifecycle and caching annotations (SEP-2549, SEP-2577, SEP-2596)
// =============================================================================

/// Scope of a cached result (SEP-2549).
///
/// Servers hint to clients how widely they may share a cached
/// `tools/list`, `resources/read`, etc. response. The final SEP-2549 wire
/// values are `"public"` and `"private"`: `Public` means any client,
/// gateway, or proxy may cache and serve the result across authorization
/// contexts; `Private` restricts reuse to the same authorization context
/// (a different access token requires a different cache). `None` on the
/// parent field means the server expresses no opinion.
///
/// Earlier drafts of this crate used `session`/`global` values that never
/// matched the SEP text; they were replaced before any release advertised
/// 2026-07-28 support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum CacheScope {
    /// Any client, gateway, or proxy may cache and serve this result across
    /// authorization contexts.
    Public,
    /// The cached result may only be reused within the same authorization
    /// context.
    Private,
}

/// Deprecation metadata for spec features and capabilities.
///
/// Per SEP-2577 + SEP-2596, the spec now has a formal Active/Deprecated/
/// Removed lifecycle for features. Servers can attach this metadata to
/// capability declarations so clients (and tooling like `manifest.rs`
/// exporters or codegen) can surface deprecation warnings.
///
/// All fields are optional so the struct stays forward-compatible with
/// future SEP-2596 extensions. Setting any field signals the parent
/// feature is deprecated.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeprecationInfo {
    /// Protocol version in which this feature became deprecated
    /// (e.g. `"2026-07-28"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Protocol version in which this feature is scheduled for removal.
    /// Per SEP-2577, the minimum window after `since` is 12 months.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove_in: Option<String>,
    /// Human-readable explanation, e.g. why this feature was deprecated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Pointer to the replacement feature or SEP, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
}

// =============================================================================
// Tools
// =============================================================================

/// Pagination and metadata for a `tools/list` request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListToolsParams {
    /// Opaque cursor returned by the preceding page, or `None` for page one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// One page returned by `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListToolsResult {
    /// Tools available on this page.
    pub tools: Vec<ToolDefinition>,
    /// Opaque cursor for the next page; absence means this is the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// SEP-2549: client-cache TTL in milliseconds for this list response.
    /// `None` means "no opinion -- client policy decides".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    /// SEP-2549: scope the cached result applies to. `None` means
    /// scope is unspecified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_scope: Option<CacheScope>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// Tool definition as returned by tools/list
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    /// Programmatic tool name supplied to `tools/call`.
    pub name: String,
    /// Human-readable title for display purposes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable explanation of the tool's purpose.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema describing accepted tool arguments.
    pub input_schema: Value,
    /// Optional JSON Schema defining expected output structure
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// Optional icons for display in user interfaces
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icons: Option<Vec<ToolIcon>>,
    /// Optional annotations describing tool behavior.
    /// Note: Clients MUST consider these untrusted unless from a trusted server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
    /// Optional execution configuration for task support
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<ToolExecution>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// Icon theme context
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum IconTheme {
    /// Icon designed for light backgrounds
    Light,
    /// Icon designed for dark backgrounds
    Dark,
}

/// Icon for tool display in user interfaces
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolIcon {
    /// URL or data URI of the icon
    pub src: String,
    /// MIME type of the icon (e.g., "image/png", "image/svg+xml")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Available sizes (e.g., ["48x48", "96x96"])
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sizes: Option<Vec<String>>,
    /// Icon theme context ("light" or "dark")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<IconTheme>,
}

/// Annotations describing tool behavior for trust and safety.
/// Clients MUST consider these untrusted unless the server is trusted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    /// Human-readable title for the tool
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// If true, the tool does not modify state. Default: false
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only_hint: bool,
    /// If true, the tool may have destructive effects. Default: true
    /// Only meaningful when read_only_hint is false.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub destructive_hint: bool,
    /// If true, calling repeatedly with same args has same effect. Default: false
    /// Only meaningful when read_only_hint is false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub idempotent_hint: bool,
    /// If true, tool interacts with external entities. Default: true
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub open_world_hint: bool,
}

impl Default for ToolAnnotations {
    fn default() -> Self {
        Self {
            title: None,
            read_only_hint: false,
            destructive_hint: true,
            idempotent_hint: false,
            open_world_hint: true,
        }
    }
}

impl ToolAnnotations {
    /// Returns true if the tool does not modify state.
    pub fn is_read_only(&self) -> bool {
        self.read_only_hint
    }

    /// Returns true if the tool may have destructive effects.
    pub fn is_destructive(&self) -> bool {
        self.destructive_hint
    }

    /// Returns true if calling repeatedly with same args has the same effect.
    pub fn is_idempotent(&self) -> bool {
        self.idempotent_hint
    }

    /// Returns true if the tool interacts with external entities.
    pub fn is_open_world(&self) -> bool {
        self.open_world_hint
    }
}

impl ToolDefinition {
    /// Returns true if the tool does not modify state.
    ///
    /// Returns `false` (the MCP spec default) when annotations are absent.
    pub fn is_read_only(&self) -> bool {
        self.annotations.as_ref().is_some_and(|a| a.read_only_hint)
    }

    /// Returns true if the tool may have destructive effects.
    ///
    /// Returns `true` (the MCP spec default) when annotations are absent.
    pub fn is_destructive(&self) -> bool {
        self.annotations.as_ref().is_none_or(|a| a.destructive_hint)
    }

    /// Returns true if calling repeatedly with same args has the same effect.
    ///
    /// Returns `false` (the MCP spec default) when annotations are absent.
    pub fn is_idempotent(&self) -> bool {
        self.annotations.as_ref().is_some_and(|a| a.idempotent_hint)
    }

    /// Returns true if the tool interacts with external entities.
    ///
    /// Returns `true` (the MCP spec default) when annotations are absent.
    pub fn is_open_world(&self) -> bool {
        self.annotations.as_ref().is_none_or(|a| a.open_world_hint)
    }
}

fn default_true() -> bool {
    true
}

fn is_true(v: &bool) -> bool {
    *v
}

/// Parameters for invoking a tool through `tools/call`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolParams {
    /// Programmatic name of the tool to invoke.
    pub name: String,
    /// Arguments validated against the tool's input schema; defaults to JSON null.
    #[serde(default)]
    pub arguments: Value,
    /// SEP-2322: responses to the server's [`InputRequests`] from a prior
    /// `input_required` result, sent on retry of the original request.
    #[serde(
        rename = "inputResponses",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub input_responses: Option<InputResponses>,
    /// SEP-2322: opaque request state echoed back from a prior `input_required`
    /// result.
    #[serde(
        rename = "requestState",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub request_state: Option<String>,
    /// Request metadata including progress token
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
    /// Legacy 2025-11-25 task parameters for async execution.
    ///
    /// This field is invalid on the 2026-07-28 protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskRequestParams>,
}

/// Result of a tool invocation.
///
/// This is the return type for tool handlers. Use the convenience constructors
/// like [`CallToolResult::text`], [`CallToolResult::json`], or [`CallToolResult::error`]
/// to create results easily.
///
/// # Example
///
/// ```rust
/// use tower_mcp_types::CallToolResult;
///
/// // Simple text result
/// let result = CallToolResult::text("Hello, world!");
///
/// // JSON result with structured content
/// let result = CallToolResult::json(serde_json::json!({"key": "value"}));
///
/// // Error result
/// let result = CallToolResult::error("Something went wrong");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
    /// The content items returned by the tool.
    pub content: Vec<Content>,
    /// Whether this result represents an error.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
    /// Optional structured content for programmatic access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    /// Optional metadata (e.g., for io.modelcontextprotocol/related-task)
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

impl CallToolResult {
    /// Create a text result.
    ///
    /// This is the most common result type for tools that return plain text.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Text {
                text: text.into(),
                annotations: None,
                meta: None,
            }],
            is_error: false,
            structured_content: None,
            meta: None,
        }
    }

    /// Create an error result.
    ///
    /// Use this when the tool encounters an error during execution.
    /// The `is_error` flag will be set to `true`.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Text {
                text: message.into(),
                annotations: None,
                meta: None,
            }],
            is_error: true,
            structured_content: None,
            meta: None,
        }
    }

    /// Create a JSON result with structured content from a [`serde_json::Value`].
    ///
    /// The JSON value is serialized to pretty-printed text for display,
    /// and also stored in `structured_content` for programmatic access.
    ///
    /// If you have a type that implements [`serde::Serialize`], use
    /// [`from_serialize`](Self::from_serialize) instead to avoid manual `to_value()` calls.
    pub fn json(value: Value) -> Self {
        let text = serde_json::to_string_pretty(&value).unwrap_or_default();
        Self {
            content: vec![Content::Text {
                text,
                annotations: None,
                meta: None,
            }],
            is_error: false,
            structured_content: Some(value),
            meta: None,
        }
    }

    /// Create a JSON result from any serializable value.
    ///
    /// This is a fallible alternative to [`json`](Self::json) that accepts any
    /// `serde::Serialize` type and handles serialization errors gracefully.
    /// The value is serialized to a `serde_json::Value`, then delegated to `json()`,
    /// so `structured_content` is populated correctly.
    ///
    /// # Errors
    ///
    /// Returns an error if the value cannot be serialized to JSON.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::CallToolResult;
    /// use serde::Serialize;
    ///
    /// #[derive(Serialize)]
    /// struct SearchResult {
    ///     title: String,
    ///     score: f64,
    /// }
    ///
    /// let result = SearchResult {
    ///     title: "Example".to_string(),
    ///     score: 0.95,
    /// };
    /// let tool_result = CallToolResult::from_serialize(&result).unwrap();
    /// assert!(!tool_result.is_error);
    /// assert!(tool_result.structured_content.is_some());
    /// ```
    pub fn from_serialize(
        value: &impl serde::Serialize,
    ) -> std::result::Result<Self, crate::error::Error> {
        let json_value = serde_json::to_value(value)
            .map_err(|e| crate::error::Error::tool(format!("Serialization failed: {}", e)))?;
        Ok(Self::json(json_value))
    }

    /// Create a result from a list of serializable items.
    ///
    /// Wraps the list in a JSON object with the given key and a `count` field,
    /// since MCP `structuredContent` requires objects, not bare arrays.
    ///
    /// # Examples
    ///
    /// ```
    /// use tower_mcp_types::CallToolResult;
    /// use serde::Serialize;
    ///
    /// #[derive(Serialize)]
    /// struct Database {
    ///     name: String,
    ///     size_mb: u64,
    /// }
    ///
    /// let databases = vec![
    ///     Database { name: "users".to_string(), size_mb: 100 },
    ///     Database { name: "logs".to_string(), size_mb: 500 },
    /// ];
    /// let result = CallToolResult::from_list("databases", &databases).unwrap();
    /// // Produces: {"databases": [...], "count": 2}
    /// assert!(!result.is_error);
    /// assert!(result.structured_content.is_some());
    /// ```
    pub fn from_list<T: serde::Serialize>(
        key: &str,
        items: &[T],
    ) -> std::result::Result<Self, crate::error::Error> {
        Self::from_serialize(&serde_json::json!({ key: items, "count": items.len() }))
    }

    /// Create a result with a base64-encoded image.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::CallToolResult;
    /// use base64::Engine;
    ///
    /// let png_bytes = vec![0x89, 0x50, 0x4E, 0x47]; // PNG header
    /// let encoded = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    ///
    /// let result = CallToolResult::image(encoded, "image/png");
    /// assert!(!result.is_error);
    /// ```
    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Image {
                data: data.into(),
                mime_type: mime_type.into(),
                annotations: None,
                meta: None,
            }],
            is_error: false,
            structured_content: None,
            meta: None,
        }
    }

    /// Create a result with base64-encoded audio.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::CallToolResult;
    /// use base64::Engine;
    ///
    /// let wav_bytes = vec![0x52, 0x49, 0x46, 0x46]; // RIFF header
    /// let encoded = base64::engine::general_purpose::STANDARD.encode(&wav_bytes);
    ///
    /// let result = CallToolResult::audio(encoded, "audio/wav");
    /// assert!(!result.is_error);
    /// ```
    pub fn audio(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Audio {
                data: data.into(),
                mime_type: mime_type.into(),
                annotations: None,
                meta: None,
            }],
            is_error: false,
            structured_content: None,
            meta: None,
        }
    }

    /// Create a result with a resource link (without embedding the content).
    ///
    /// Use this to reference a resource by URI without including its full content
    /// in the tool result.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::CallToolResult;
    ///
    /// let result = CallToolResult::resource_link(
    ///     "file:///var/log/app.log",
    ///     "app-log",
    /// );
    /// assert!(!result.is_error);
    /// ```
    pub fn resource_link(uri: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            content: vec![Content::ResourceLink {
                uri: uri.into(),
                name: name.into(),
                title: None,
                description: None,
                mime_type: None,
                size: None,
                icons: None,
                annotations: None,
                meta: None,
            }],
            is_error: false,
            structured_content: None,
            meta: None,
        }
    }

    /// Create a result with a resource link including metadata
    pub fn resource_link_with_meta(
        uri: impl Into<String>,
        name: impl Into<String>,
        description: Option<String>,
        mime_type: Option<String>,
    ) -> Self {
        Self {
            content: vec![Content::ResourceLink {
                uri: uri.into(),
                name: name.into(),
                title: None,
                description,
                mime_type,
                size: None,
                icons: None,
                annotations: None,
                meta: None,
            }],
            is_error: false,
            structured_content: None,
            meta: None,
        }
    }

    /// Create a result with an embedded resource
    pub fn resource(resource: ResourceContent) -> Self {
        Self {
            content: vec![Content::Resource {
                resource,
                annotations: None,
                meta: None,
            }],
            is_error: false,
            structured_content: None,
            meta: None,
        }
    }

    /// Concatenate all text content items into a single string.
    ///
    /// Non-text content items are skipped. Multiple text items are
    /// joined without a separator.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::CallToolResult;
    ///
    /// let result = CallToolResult::text("hello world");
    /// assert_eq!(result.all_text(), "hello world");
    /// ```
    pub fn all_text(&self) -> String {
        self.content.iter().filter_map(|c| c.as_text()).collect()
    }

    /// Get the text from the first [`Content::Text`] item.
    ///
    /// Returns `None` if there are no text content items.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::CallToolResult;
    ///
    /// let result = CallToolResult::text("hello");
    /// assert_eq!(result.first_text(), Some("hello"));
    /// ```
    pub fn first_text(&self) -> Option<&str> {
        self.content.iter().find_map(|c| c.as_text())
    }

    /// Parse the result as a JSON [`Value`].
    ///
    /// Returns `structured_content` if set (from [`json()`](Self::json) /
    /// [`from_serialize()`](Self::from_serialize)), otherwise parses
    /// [`first_text()`](Self::first_text). Returns `None` if no content is available.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::CallToolResult;
    /// use serde_json::json;
    ///
    /// let result = CallToolResult::json(json!({"key": "value"}));
    /// let value = result.as_json().unwrap().unwrap();
    /// assert_eq!(value["key"], "value");
    /// ```
    pub fn as_json(&self) -> Option<Result<Value, serde_json::Error>> {
        if let Some(ref sc) = self.structured_content {
            return Some(Ok(sc.clone()));
        }
        self.first_text().map(serde_json::from_str)
    }

    /// Deserialize the result into a typed value.
    ///
    /// Uses `structured_content` if set, otherwise parses
    /// [`first_text()`](Self::first_text). Returns `None` if no content is available.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::CallToolResult;
    /// use serde::Deserialize;
    /// use serde_json::json;
    ///
    /// #[derive(Debug, Deserialize, PartialEq)]
    /// struct Output { key: String }
    ///
    /// let result = CallToolResult::json(json!({"key": "value"}));
    /// let output: Output = result.deserialize().unwrap().unwrap();
    /// assert_eq!(output.key, "value");
    /// ```
    pub fn deserialize<T: DeserializeOwned>(&self) -> Option<Result<T, serde_json::Error>> {
        if let Some(ref sc) = self.structured_content {
            return Some(serde_json::from_value(sc.clone()));
        }
        self.first_text().map(serde_json::from_str)
    }
}

/// Content types for tool results, resources, and prompts.
///
/// Content can be text, images, audio, or embedded resources. Each variant
/// supports optional annotations for audience targeting and priority hints.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Content {
    /// Plain text content.
    Text {
        /// The text content.
        text: String,
        /// Optional annotations for this content.
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
        /// Optional protocol-level metadata
        #[serde(
            rename = "_meta",
            default,
            skip_serializing_if = "Option::is_none",
            with = "crate::protocol::meta_object_serde"
        )]
        meta: Option<Value>,
    },
    /// Base64-encoded image content.
    Image {
        /// Base64-encoded image data.
        data: String,
        /// MIME type (e.g., "image/png", "image/jpeg").
        #[serde(rename = "mimeType")]
        mime_type: String,
        /// Optional annotations for this content.
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
        /// Optional protocol-level metadata
        #[serde(
            rename = "_meta",
            default,
            skip_serializing_if = "Option::is_none",
            with = "crate::protocol::meta_object_serde"
        )]
        meta: Option<Value>,
    },
    /// Base64-encoded audio content.
    Audio {
        /// Base64-encoded audio data.
        data: String,
        /// MIME type (e.g., "audio/wav", "audio/mp3").
        #[serde(rename = "mimeType")]
        mime_type: String,
        /// Optional annotations for this content.
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
        /// Optional protocol-level metadata
        #[serde(
            rename = "_meta",
            default,
            skip_serializing_if = "Option::is_none",
            with = "crate::protocol::meta_object_serde"
        )]
        meta: Option<Value>,
    },
    /// Embedded resource content.
    Resource {
        /// The embedded resource.
        resource: ResourceContent,
        /// Optional annotations for this content.
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
        /// Optional protocol-level metadata
        #[serde(
            rename = "_meta",
            default,
            skip_serializing_if = "Option::is_none",
            with = "crate::protocol::meta_object_serde"
        )]
        meta: Option<Value>,
    },
    /// Link to a resource (without embedding the content)
    ResourceLink {
        /// URI of the resource
        uri: String,
        /// Programmatic name of the resource (required per BaseMetadata)
        name: String,
        /// Human-readable title for display purposes
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Description of the resource
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// MIME type of the resource
        #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        /// Raw content size in bytes
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
        /// Optional icons for display in user interfaces
        #[serde(default, skip_serializing_if = "Option::is_none")]
        icons: Option<Vec<ToolIcon>>,
        /// Audience, priority, and modification hints for the embedded resource.
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
        /// Optional protocol-level metadata
        #[serde(
            rename = "_meta",
            default,
            skip_serializing_if = "Option::is_none",
            with = "crate::protocol::meta_object_serde"
        )]
        meta: Option<Value>,
    },
}

/// Annotations for content items
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContentAnnotations {
    /// Intended audience for this content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audience: Option<Vec<ContentRole>>,
    /// Priority hint from 0 (optional) to 1 (required)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<f64>,
    /// ISO 8601 timestamp of when this content was last modified
    #[serde(rename = "lastModified", skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
}

impl Content {
    /// Extract the text from a [`Content::Text`] variant.
    ///
    /// Returns `None` for non-text content variants.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::Content;
    ///
    /// let content = Content::Text { text: "hello".into(), annotations: None, meta: None };
    /// assert_eq!(content.as_text(), Some("hello"));
    /// ```
    /// Create a [`Content::Text`] variant with no annotations.
    ///
    /// This is a shorthand for the common case of creating text content
    /// without annotations.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::Content;
    ///
    /// let content = Content::text("hello world");
    /// assert_eq!(content.as_text(), Some("hello world"));
    /// ```
    pub fn text(text: impl Into<String>) -> Self {
        Content::Text {
            text: text.into(),
            annotations: None,
            meta: None,
        }
    }

    /// Extract the text from a [`Content::Text`] variant.
    ///
    /// Returns `None` for non-text content variants.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::Content;
    ///
    /// let content = Content::Text { text: "hello".into(), annotations: None, meta: None };
    /// assert_eq!(content.as_text(), Some("hello"));
    /// ```
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Content::Text { text, .. } => Some(text),
            _ => None,
        }
    }
}

/// Role indicating who content is intended for.
///
/// Used in content annotations to specify the target audience.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ContentRole {
    /// Content intended for the human user.
    User,
    /// Content intended for the AI assistant.
    Assistant,
}

/// Content of an embedded resource.
///
/// Contains either text or binary (blob) content along with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceContent {
    /// The URI identifying this resource.
    pub uri: String,
    /// MIME type of the content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Text content (for text-based resources).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Base64-encoded binary content (for binary resources).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

// =============================================================================
// Resources
// =============================================================================

/// Pagination and metadata for a `resources/list` request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListResourcesParams {
    /// Opaque cursor returned by the preceding page, or `None` for page one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// One page returned by `resources/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListResourcesResult {
    /// Resources available on this page.
    pub resources: Vec<ResourceDefinition>,
    /// Opaque cursor for the next page; absence means this is the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// SEP-2549: client-cache TTL in milliseconds for this list response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    /// SEP-2549: scope the cached result applies to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_scope: Option<CacheScope>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// Resource metadata returned by `resources/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceDefinition {
    /// URI used to identify and read the resource.
    pub uri: String,
    /// Programmatic or display name of the resource.
    pub name: String,
    /// Human-readable title for display purposes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable explanation of the resource.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional MIME type of the resource contents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Optional icons for display in user interfaces
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icons: Option<Vec<ToolIcon>>,
    /// Size of the resource in bytes (if known)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Annotations for this resource (audience, priority hints)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ContentAnnotations>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// Parameters for a `resources/read` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResourceParams {
    /// URI of the resource to read.
    pub uri: String,
    /// SEP-2322: responses to the server's [`InputRequests`] from a prior
    /// `input_required` result, sent on retry of the original request.
    #[serde(
        rename = "inputResponses",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub input_responses: Option<InputResponses>,
    /// SEP-2322: opaque request state echoed back from a prior `input_required`
    /// result.
    #[serde(
        rename = "requestState",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub request_state: Option<String>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// Contents returned by `resources/read`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReadResourceResult {
    /// Text or binary content blocks produced for the requested resource.
    pub contents: Vec<ResourceContent>,
    /// SEP-2549: client-cache TTL in milliseconds for this read response.
    /// The 2026-07-28 draft requires caching hints on `resources/read`
    /// results; on older protocol versions the field is simply extra data.
    #[serde(rename = "ttlMs", default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    /// SEP-2549: scope the cached result applies to. `None` means
    /// unspecified (clients treat it conservatively).
    #[serde(
        rename = "cacheScope",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_scope: Option<CacheScope>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

impl ReadResourceResult {
    /// Set the SEP-2549 client-cache TTL (milliseconds) for this result.
    pub fn with_ttl_ms(mut self, ttl_ms: u64) -> Self {
        self.ttl_ms = Some(ttl_ms);
        self
    }

    /// Set the SEP-2549 cache scope for this result.
    pub fn with_cache_scope(mut self, scope: CacheScope) -> Self {
        self.cache_scope = Some(scope);
        self
    }

    /// Create a result with text content.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::ReadResourceResult;
    ///
    /// let result = ReadResourceResult::text("file://readme.md", "# Hello World");
    /// ```
    pub fn text(uri: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            contents: vec![ResourceContent {
                uri: uri.into(),
                mime_type: Some("text/plain".to_string()),
                text: Some(content.into()),
                blob: None,
                meta: None,
            }],
            meta: None,
            ..Default::default()
        }
    }

    /// Create a result with text content and a specific MIME type.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::ReadResourceResult;
    ///
    /// let result = ReadResourceResult::text_with_mime(
    ///     "file://readme.md",
    ///     "# Hello World",
    ///     "text/markdown"
    /// );
    /// ```
    pub fn text_with_mime(
        uri: impl Into<String>,
        content: impl Into<String>,
        mime_type: impl Into<String>,
    ) -> Self {
        Self {
            contents: vec![ResourceContent {
                uri: uri.into(),
                mime_type: Some(mime_type.into()),
                text: Some(content.into()),
                blob: None,
                meta: None,
            }],
            meta: None,
            ..Default::default()
        }
    }

    /// Create a result with JSON content.
    ///
    /// The value is serialized to a JSON string automatically.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::ReadResourceResult;
    /// use serde_json::json;
    ///
    /// let data = json!({"name": "example", "count": 42});
    /// let result = ReadResourceResult::json("data://config", &data);
    /// ```
    pub fn json<T: serde::Serialize>(uri: impl Into<String>, value: &T) -> Self {
        let json_string =
            serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_string());
        Self {
            contents: vec![ResourceContent {
                uri: uri.into(),
                mime_type: Some("application/json".to_string()),
                text: Some(json_string),
                blob: None,
                meta: None,
            }],
            meta: None,
            ..Default::default()
        }
    }

    /// Create a result with binary content (base64 encoded).
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::ReadResourceResult;
    ///
    /// let bytes = vec![0x89, 0x50, 0x4E, 0x47]; // PNG magic bytes
    /// let result = ReadResourceResult::blob("file://image.png", &bytes);
    /// ```
    pub fn blob(uri: impl Into<String>, bytes: &[u8]) -> Self {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        Self {
            contents: vec![ResourceContent {
                uri: uri.into(),
                mime_type: Some("application/octet-stream".to_string()),
                text: None,
                blob: Some(encoded),
                meta: None,
            }],
            meta: None,
            ..Default::default()
        }
    }

    /// Create a result with binary content and a specific MIME type.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::ReadResourceResult;
    ///
    /// let bytes = vec![0x89, 0x50, 0x4E, 0x47];
    /// let result = ReadResourceResult::blob_with_mime("file://image.png", &bytes, "image/png");
    /// ```
    pub fn blob_with_mime(
        uri: impl Into<String>,
        bytes: &[u8],
        mime_type: impl Into<String>,
    ) -> Self {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        Self {
            contents: vec![ResourceContent {
                uri: uri.into(),
                mime_type: Some(mime_type.into()),
                text: None,
                blob: Some(encoded),
                meta: None,
            }],
            meta: None,
            ..Default::default()
        }
    }

    /// Get the text from the first content item.
    ///
    /// Returns `None` if there are no contents or the first item has no text.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::ReadResourceResult;
    ///
    /// let result = ReadResourceResult::text("file://readme.md", "# Hello");
    /// assert_eq!(result.first_text(), Some("# Hello"));
    /// ```
    pub fn first_text(&self) -> Option<&str> {
        self.contents.first().and_then(|c| c.text.as_deref())
    }

    /// Get the URI from the first content item.
    ///
    /// Returns `None` if there are no contents.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::ReadResourceResult;
    ///
    /// let result = ReadResourceResult::text("file://readme.md", "# Hello");
    /// assert_eq!(result.first_uri(), Some("file://readme.md"));
    /// ```
    pub fn first_uri(&self) -> Option<&str> {
        self.contents.first().map(|c| c.uri.as_str())
    }

    /// Parse the first text content as a JSON [`Value`].
    ///
    /// Returns `None` if there are no contents or the first item has no text.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::ReadResourceResult;
    /// use serde_json::json;
    ///
    /// let result = ReadResourceResult::json("data://config", &json!({"key": "value"}));
    /// let value = result.as_json().unwrap().unwrap();
    /// assert_eq!(value["key"], "value");
    /// ```
    pub fn as_json(&self) -> Option<Result<Value, serde_json::Error>> {
        self.first_text().map(serde_json::from_str)
    }

    /// Deserialize the first text content into a typed value.
    ///
    /// Returns `None` if there are no contents or the first item has no text.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::ReadResourceResult;
    /// use serde::Deserialize;
    /// use serde_json::json;
    ///
    /// #[derive(Debug, Deserialize, PartialEq)]
    /// struct Config { key: String }
    ///
    /// let result = ReadResourceResult::json("data://config", &json!({"key": "value"}));
    /// let config: Config = result.deserialize().unwrap().unwrap();
    /// assert_eq!(config.key, "value");
    /// ```
    pub fn deserialize<T: DeserializeOwned>(&self) -> Option<Result<T, serde_json::Error>> {
        self.first_text().map(serde_json::from_str)
    }
}

/// Parameters for a `resources/subscribe` request.
#[derive(Debug, Clone, Deserialize)]
pub struct SubscribeResourceParams {
    /// URI whose updates the client wants to receive.
    pub uri: String,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// Parameters for a `resources/unsubscribe` request.
#[derive(Debug, Clone, Deserialize)]
pub struct UnsubscribeResourceParams {
    /// URI whose updates the client no longer wants to receive.
    pub uri: String,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// Parameters for listing resource templates
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListResourceTemplatesParams {
    /// Pagination cursor from previous response
    #[serde(default)]
    pub cursor: Option<String>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// Result of listing resource templates
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListResourceTemplatesResult {
    /// Available resource templates
    pub resource_templates: Vec<ResourceTemplateDefinition>,
    /// Cursor for next page (if more templates available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// SEP-2549: client-cache TTL in milliseconds for this list response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    /// SEP-2549: scope the cached result applies to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_scope: Option<CacheScope>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// Definition of a resource template as returned by resources/templates/list
///
/// Resource templates allow servers to expose parameterized resources using
/// [URI templates (RFC 6570)](https://datatracker.ietf.org/doc/html/rfc6570).
///
/// # Example
///
/// ```json
/// {
///     "uriTemplate": "file:///{path}",
///     "name": "Project Files",
///     "description": "Access files in the project directory",
///     "mimeType": "application/octet-stream"
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceTemplateDefinition {
    /// URI template following RFC 6570 (e.g., `file:///{path}`)
    pub uri_template: String,
    /// Human-readable name for this template
    pub name: String,
    /// Human-readable title for display purposes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Description of what resources this template provides
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// MIME type hint for resources from this template
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Optional icons for display in user interfaces
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icons: Option<Vec<ToolIcon>>,
    /// Annotations for this resource template (audience, priority hints)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ContentAnnotations>,
    /// Arguments accepted by this template for URI expansion
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<PromptArgument>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

// =============================================================================
// Prompts
// =============================================================================

/// Pagination and metadata for a `prompts/list` request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListPromptsParams {
    /// Opaque cursor returned by the preceding page, or `None` for page one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// One page returned by `prompts/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListPromptsResult {
    /// Prompt definitions available on this page.
    pub prompts: Vec<PromptDefinition>,
    /// Opaque cursor for the next page; absence means this is the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// SEP-2549: client-cache TTL in milliseconds for this list response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    /// SEP-2549: scope the cached result applies to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_scope: Option<CacheScope>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// Prompt metadata returned by `prompts/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptDefinition {
    /// Programmatic prompt name supplied to `prompts/get`.
    pub name: String,
    /// Human-readable title for display purposes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable explanation of the prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional icons for display in user interfaces
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icons: Option<Vec<ToolIcon>>,
    /// Arguments accepted when rendering this prompt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<PromptArgument>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// One named argument accepted by a prompt template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptArgument {
    /// Argument name supplied as a key in [`GetPromptParams::arguments`].
    pub name: String,
    /// Optional human-readable explanation of the argument.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether clients must supply this argument.
    #[serde(default)]
    pub required: bool,
}

/// Parameters for rendering a prompt through `prompts/get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetPromptParams {
    /// Programmatic name of the prompt to render.
    pub name: String,
    /// String arguments substituted into the prompt template.
    #[serde(default)]
    pub arguments: std::collections::HashMap<String, String>,
    /// SEP-2322: responses to the server's [`InputRequests`] from a prior
    /// `input_required` result, sent on retry of the original request.
    #[serde(
        rename = "inputResponses",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub input_responses: Option<InputResponses>,
    /// SEP-2322: opaque request state echoed back from a prior `input_required`
    /// result.
    #[serde(
        rename = "requestState",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub request_state: Option<String>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// The result of a prompts/get request.
///
/// Contains a list of messages that form the prompt, along with an optional
/// description. Messages can include text, images, and embedded resources.
///
/// # Example: prompt with image content
///
/// ```rust
/// use tower_mcp_types::protocol::{
///     GetPromptResult, PromptMessage, PromptRole, Content,
/// };
/// use base64::Engine;
///
/// let image_data = base64::engine::general_purpose::STANDARD.encode(b"fake-png");
///
/// let result = GetPromptResult {
///     description: Some("Analyze this image".to_string()),
///     messages: vec![
///         PromptMessage {
///             role: PromptRole::User,
///             content: Content::Image {
///                 data: image_data,
///                 mime_type: "image/png".to_string(),
///                 annotations: None,
///                 meta: None,
///             },
///             meta: None,
///         },
///     ],
///     meta: None,
/// };
/// assert_eq!(result.messages.len(), 1);
/// ```
///
/// # Example: prompt with embedded resource
///
/// ```rust
/// use tower_mcp_types::protocol::{
///     GetPromptResult, PromptMessage, PromptRole, Content, ResourceContent,
/// };
///
/// let result = GetPromptResult {
///     description: Some("Review this file".to_string()),
///     messages: vec![
///         PromptMessage {
///             role: PromptRole::User,
///             content: Content::Resource {
///                 resource: ResourceContent {
///                     uri: "file:///src/main.rs".to_string(),
///                     mime_type: Some("text/x-rust".to_string()),
///                     text: Some("fn main() {}".to_string()),
///                     blob: None,
///                     meta: None,
///                 },
///                 annotations: None,
///                 meta: None,
///             },
///             meta: None,
///         },
///     ],
///     meta: None,
/// };
/// assert_eq!(result.messages.len(), 1);
/// ```
pub struct GetPromptResult {
    /// Optional human-readable description of the rendered prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Ordered conversation messages produced by the prompt.
    pub messages: Vec<PromptMessage>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

impl GetPromptResult {
    /// Create a result with a single user message.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::GetPromptResult;
    ///
    /// let result = GetPromptResult::user_message("Please analyze this code.");
    /// ```
    pub fn user_message(text: impl Into<String>) -> Self {
        Self {
            description: None,
            messages: vec![PromptMessage {
                role: PromptRole::User,
                content: Content::Text {
                    text: text.into(),
                    annotations: None,
                    meta: None,
                },
                meta: None,
            }],
            meta: None,
        }
    }

    /// Create a result with a single user message and description.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::GetPromptResult;
    ///
    /// let result = GetPromptResult::user_message_with_description(
    ///     "Please analyze this code.",
    ///     "Code analysis prompt"
    /// );
    /// ```
    pub fn user_message_with_description(
        text: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            description: Some(description.into()),
            messages: vec![PromptMessage {
                role: PromptRole::User,
                content: Content::Text {
                    text: text.into(),
                    annotations: None,
                    meta: None,
                },
                meta: None,
            }],
            meta: None,
        }
    }

    /// Create a result with a single assistant message.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::GetPromptResult;
    ///
    /// let result = GetPromptResult::assistant_message("Here is my analysis...");
    /// ```
    pub fn assistant_message(text: impl Into<String>) -> Self {
        Self {
            description: None,
            messages: vec![PromptMessage {
                role: PromptRole::Assistant,
                content: Content::Text {
                    text: text.into(),
                    annotations: None,
                    meta: None,
                },
                meta: None,
            }],
            meta: None,
        }
    }

    /// Create a builder for constructing prompts with multiple messages.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::GetPromptResult;
    ///
    /// let result = GetPromptResult::builder()
    ///     .description("Multi-turn conversation prompt")
    ///     .user("What is the weather today?")
    ///     .assistant("I don't have access to weather data, but I can help you find it.")
    ///     .user("Where should I look?")
    ///     .build();
    /// ```
    pub fn builder() -> GetPromptResultBuilder {
        GetPromptResultBuilder::new()
    }

    /// Get the text from the first message's content.
    ///
    /// Returns `None` if there are no messages or the first message
    /// does not contain text content.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::GetPromptResult;
    ///
    /// let result = GetPromptResult::user_message("Analyze this code.");
    /// assert_eq!(result.first_message_text(), Some("Analyze this code."));
    /// ```
    pub fn first_message_text(&self) -> Option<&str> {
        self.messages.first().and_then(|m| m.content.as_text())
    }

    /// Parse the first message text as a JSON [`Value`].
    ///
    /// Returns `None` if there are no messages or the first message
    /// does not contain text content.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::GetPromptResult;
    ///
    /// let result = GetPromptResult::user_message(r#"{"key": "value"}"#);
    /// let value = result.as_json().unwrap().unwrap();
    /// assert_eq!(value["key"], "value");
    /// ```
    pub fn as_json(&self) -> Option<Result<Value, serde_json::Error>> {
        self.first_message_text().map(serde_json::from_str)
    }

    /// Deserialize the first message text into a typed value.
    ///
    /// Returns `None` if there are no messages or the first message
    /// does not contain text content.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp_types::GetPromptResult;
    /// use serde::Deserialize;
    ///
    /// #[derive(Debug, Deserialize, PartialEq)]
    /// struct Params { key: String }
    ///
    /// let result = GetPromptResult::user_message(r#"{"key": "value"}"#);
    /// let params: Params = result.deserialize().unwrap().unwrap();
    /// assert_eq!(params.key, "value");
    /// ```
    pub fn deserialize<T: DeserializeOwned>(&self) -> Option<Result<T, serde_json::Error>> {
        self.first_message_text().map(serde_json::from_str)
    }
}

/// Builder for constructing [`GetPromptResult`] with multiple messages.
#[derive(Debug, Clone, Default)]
pub struct GetPromptResultBuilder {
    description: Option<String>,
    messages: Vec<PromptMessage>,
}

impl GetPromptResultBuilder {
    /// Create a new builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the prompt description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Add a user message.
    pub fn user(mut self, text: impl Into<String>) -> Self {
        self.messages.push(PromptMessage {
            role: PromptRole::User,
            content: Content::Text {
                text: text.into(),
                annotations: None,
                meta: None,
            },
            meta: None,
        });
        self
    }

    /// Add an assistant message.
    pub fn assistant(mut self, text: impl Into<String>) -> Self {
        self.messages.push(PromptMessage {
            role: PromptRole::Assistant,
            content: Content::Text {
                text: text.into(),
                annotations: None,
                meta: None,
            },
            meta: None,
        });
        self
    }

    /// Build the final result.
    pub fn build(self) -> GetPromptResult {
        GetPromptResult {
            description: self.description,
            messages: self.messages,
            meta: None,
        }
    }
}

/// A role-tagged content block in a rendered prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptMessage {
    /// Whether the message is addressed as the user or assistant.
    pub role: PromptRole,
    /// Text, image, audio, or embedded-resource content of the message.
    pub content: Content,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// Conversation role assigned to a rendered prompt message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum PromptRole {
    /// A message originating from or addressed as the user.
    User,
    /// A message originating from or addressed as the assistant.
    Assistant,
}

// =============================================================================
// Tasks (async operations)
// =============================================================================

/// Reverse-DNS identifier for the SEP-2663 tasks extension.
///
/// This string is the key under which task-extension capabilities are declared
/// inside `ClientCapabilities.extensions` and `ServerCapabilities.extensions`.
/// It is **not** a method prefix: per the spec, methods remain unprefixed
/// (`tasks/get`, `tasks/update`, `tasks/cancel`).
pub const TASKS_EXTENSION_ID: &str = "io.modelcontextprotocol/tasks";

/// SEP-2322 / SEP-2663 `resultType` discriminator value for task results.
///
/// Per SEP-2663, servers **MUST** set `resultType` to `"task"` when returning
/// a [`CreateTaskResult`], and **MUST NOT** set `resultType` to `"task"` on
/// any other result shape.
pub const RESULT_TYPE_TASK: &str = "task";

/// Task support mode for tool execution
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum TaskSupportMode {
    /// Task execution is required.
    ///
    /// Legacy clients must include task parameters. On the final protocol the
    /// server creates a task whenever the extension is negotiated, and rejects
    /// clients that did not declare it.
    Required,
    /// Task execution is optional.
    ///
    /// Legacy clients choose by including task parameters. On the final
    /// protocol this is server policy: the router creates a task when both
    /// peers negotiated the extension and otherwise completes synchronously.
    Optional,
    /// Task execution is forbidden; the tool always completes synchronously.
    #[default]
    Forbidden,
}

/// Legacy 2025-11-25 task execution metadata for a tool definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecution {
    /// Whether the legacy tool supports task-augmented requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_support: Option<TaskSupportMode>,
}

/// Legacy 2025-11-25 task-augmentation parameters.
///
/// The final Tasks extension has no `task` field on `tools/call`; task creation
/// is server-directed after extension negotiation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRequestParams {
    /// Time-to-live for the task in milliseconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<u64>,
}

/// Status of an async task
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TaskStatus {
    /// Task is actively being processed
    Working,
    /// Task requires user input to continue
    InputRequired,
    /// Task completed successfully
    Completed,
    /// Task failed with an error
    Failed,
    /// Task was cancelled by user request
    Cancelled,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskStatus::Working => write!(f, "working"),
            TaskStatus::InputRequired => write!(f, "input_required"),
            TaskStatus::Completed => write!(f, "completed"),
            TaskStatus::Failed => write!(f, "failed"),
            TaskStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl TaskStatus {
    /// Check if this status represents a terminal state
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }
}

/// Task object matching the MCP 2025-11-25 spec
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskObject {
    /// Unique task identifier
    pub task_id: String,
    /// Current task status
    pub status: TaskStatus,
    /// Human-readable status message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    /// ISO 8601 timestamp when the task was created
    pub created_at: String,
    /// ISO 8601 timestamp when the task was last updated
    pub last_updated_at: String,
    /// Time-to-live in milliseconds, null for unlimited
    pub ttl: Option<u64>,
    /// Suggested polling interval in milliseconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_interval: Option<u64>,
    /// SEP-2663 DetailedTask payload for `completed` tasks: exactly the
    /// result the synchronous request would have returned. Absent for
    /// non-terminal and non-completed statuses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<CallToolResult>,
    /// SEP-2663 DetailedTask payload for `failed` tasks: the JSON-RPC error
    /// object describing the execution failure. Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<crate::error::JsonRpcError>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// Backwards-compatible alias for TaskObject
#[deprecated(note = "Use TaskObject instead")]
pub type TaskInfo = TaskObject;

/// Backwards-compatible task-creation result used by the 2025-11-25 API.
///
/// This compatibility type accepts both the historical nested object and the
/// final flat fields so the pre-final high-level client API can keep its return
/// type. The 2026-07-28 runtime never emits it on the wire; it uses the exact
/// [`crate::tasks::CreateTaskResult`] instead.
///
/// Serialization layout:
/// ```jsonc
/// {
///   "resultType": "task",
///   "taskId": "...",
///   "status": "working",
///   "createdAt": "...",
///   "lastUpdatedAt": "...",
///   "ttl": null,
///   "pollInterval": 5000,
///   // The nested `task` field preserves the 2025-11-25 wire format.
///   "task": { "taskId": ..., "status": ..., ... }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct CreateTaskResult {
    /// The created task object. This compatibility serializer emits both the
    /// flat fields and the legacy nested mirror.
    pub task: TaskObject,
    /// Optional protocol-level metadata
    pub meta: Option<Value>,
}

impl CreateTaskResult {
    /// Wire-spec discriminator value (always `"task"`).
    pub const RESULT_TYPE: &'static str = RESULT_TYPE_TASK;

    /// Build a compatibility result from a [`TaskObject`].
    pub fn new(task: TaskObject) -> Self {
        Self { task, meta: None }
    }
}

impl Serialize for CreateTaskResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Serialize the compatibility object with both the flat discriminator
        // and the legacy nested task mirror.
        let mut value = serde_json::to_value(&self.task)
            .map_err(|e| serde::ser::Error::custom(format!("task serialize: {e}")))?;
        let obj = value
            .as_object_mut()
            .ok_or_else(|| serde::ser::Error::custom("task did not serialize to a JSON object"))?;
        obj.insert(
            "resultType".to_string(),
            Value::String(Self::RESULT_TYPE.to_string()),
        );
        obj.insert(
            "task".to_string(),
            serde_json::to_value(&self.task)
                .map_err(|e| serde::ser::Error::custom(format!("task mirror: {e}")))?,
        );
        if let Some(meta) = &self.meta {
            validate_meta_object(meta).map_err(serde::ser::Error::custom)?;
            obj.insert("_meta".to_string(), meta.clone());
        }
        value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CreateTaskResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Accept both the SEP-2663 flat layout and the 2025-11-25 nested
        // layout (`{ "task": { ... } }`) by deserializing as a generic
        // object and disambiguating.
        let value = Value::deserialize(deserializer)?;
        let meta = value.get("_meta").filter(|v| !v.is_null()).cloned();
        if let Some(meta) = meta.as_ref() {
            validate_meta_object(meta).map_err(serde::de::Error::custom)?;
        }
        if let Some(task_val) = value.get("task")
            && task_val.is_object()
            && task_val
                .as_object()
                .is_some_and(|o| o.contains_key("taskId"))
        {
            // Nested back-compat shape; trust the nested object.
            let task: TaskObject =
                serde_json::from_value(task_val.clone()).map_err(serde::de::Error::custom)?;
            return Ok(CreateTaskResult { task, meta });
        }
        // Otherwise treat the flat top-level fields as the TaskObject.
        let task: TaskObject = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(CreateTaskResult { task, meta })
    }
}

/// Parameters for listing tasks
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListTasksParams {
    /// Filter by status (optional)
    #[serde(default)]
    pub status: Option<TaskStatus>,
    /// Pagination cursor
    #[serde(default)]
    pub cursor: Option<String>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// Result of listing tasks
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListTasksResult {
    /// List of tasks
    pub tasks: Vec<TaskObject>,
    /// Next cursor for pagination
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Parameters for getting task info
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTaskInfoParams {
    /// Task ID to query
    pub task_id: String,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// Parameters for getting task result
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTaskResultParams {
    /// Task ID to get result for
    pub task_id: String,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// Parameters for cancelling a task
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelTaskParams {
    /// Task ID to cancel
    pub task_id: String,
    /// Optional reason for cancellation
    #[serde(default)]
    pub reason: Option<String>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// Parameters for the SEP-2663 `tasks/update` request.
///
/// When a task is in the `input_required` state the server publishes one or
/// more requests in the `inputRequests` field of a `tasks/get` response;
/// clients fulfill those requests by posting responses keyed by the request
/// identifier in `inputResponses`. The server acknowledges with an empty
/// result; the post-update task state is observed via a subsequent
/// `tasks/get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTaskParams {
    /// Identifier of the task being updated.
    pub task_id: String,
    /// Responses to outstanding `inputRequests` previously surfaced by the
    /// server, keyed by the request identifier. Opaque to this crate; clients
    /// supply whatever shape the server expects per its `inputRequest`
    /// envelope (e.g. an `ElicitResult` or `CreateMessageResult`).
    #[serde(default)]
    pub input_responses: HashMap<String, Value>,
    /// Optional protocol-level metadata.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// Notification params when task status changes
///
/// Per the spec, `TaskStatusNotificationParams = NotificationParams & Task`,
/// so this includes all fields from the Task object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusParams {
    /// Task ID
    pub task_id: String,
    /// New status
    pub status: TaskStatus,
    /// Human-readable status message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    /// ISO 8601 timestamp when the task was created
    pub created_at: String,
    /// ISO 8601 timestamp when the task was last updated
    pub last_updated_at: String,
    /// Time-to-live in milliseconds, null for unlimited
    pub ttl: Option<u64>,
    /// Suggested polling interval in milliseconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_interval: Option<u64>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

/// Backwards-compatible alias
pub type TaskStatusChangedParams = TaskStatusParams;

// =============================================================================
// Elicitation (server-to-client user input requests)
// =============================================================================

/// Parameters for form-based elicitation request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitFormParams {
    /// The elicitation mode (defaults to form if not specified)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ElicitMode>,
    /// Message to present to the user explaining what information is needed
    pub message: String,
    /// Schema for the form fields (restricted subset of JSON Schema)
    pub requested_schema: ElicitFormSchema,
    /// Request metadata including progress token
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// Parameters for URL-based elicitation request.
///
/// URL-based elicitation allows servers to direct users to an external URL
/// (e.g., an OAuth flow or payment page) and receive completion notification.
///
/// # Example
///
/// ```rust
/// use tower_mcp_types::protocol::{ElicitUrlParams, ElicitMode};
///
/// let params = ElicitUrlParams {
///     mode: Some(ElicitMode::Url),
///     elicitation_id: "auth-flow-123".to_string(),
///     message: "Please sign in to continue".to_string(),
///     url: "https://example.com/auth?session=abc".to_string(),
///     meta: None,
/// };
///
/// assert_eq!(params.url, "https://example.com/auth?session=abc");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitUrlParams {
    /// The elicitation mode (defaults to url if not specified)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ElicitMode>,
    /// Unique ID for this elicitation (opaque to client).
    ///
    /// **2025-11-25 and earlier only.** The final 2026-07-28 schema removes
    /// this field: MRTR (SEP-2322, not yet implemented -- see #950) replaces
    /// the completion-notification-plus-ID pattern with a request retry, so
    /// there is no longer a standalone async completion to correlate. This
    /// field stays required and unconditionally sent for now, since this
    /// crate does not yet distinguish 2026-07-28 elicitation requests from
    /// 2025-11-25 ones -- see [`ElicitationCompleteParams`] for the paired
    /// notification this ID was meant to correlate with.
    pub elicitation_id: String,
    /// Message explaining why the user needs to navigate to the URL
    pub message: String,
    /// The URL the user should navigate to
    pub url: String,
    /// Request metadata including progress token
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<RequestMeta>,
}

/// Elicitation request parameters (union of form and URL modes)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum ElicitRequestParams {
    /// In-band structured form elicitation.
    Form(ElicitFormParams),
    /// Out-of-band interaction reached through a URL.
    Url(ElicitUrlParams),
}

/// Elicitation mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ElicitMode {
    /// Form-based elicitation with structured input
    Form,
    /// URL-based elicitation (out-of-band)
    Url,
}

/// Restricted JSON Schema for elicitation forms.
///
/// Based on [JSON Schema 2020-12](https://json-schema.org/specification), but restricted
/// to a flat object with primitive-typed properties. Complex types (arrays, nested objects)
/// are not supported -- only `string`, `integer`, `number`, `boolean`, and `enum` fields.
///
/// The schema is validated by the client before submitting the form response.
/// Required fields must be present, and each value must match its declared type.
///
/// Use the builder methods to construct a schema with string, integer,
/// number, boolean, and enum fields, optionally with default values.
///
/// # Example
///
/// ```rust
/// use tower_mcp_types::protocol::ElicitFormSchema;
///
/// let schema = ElicitFormSchema::new()
///     .string_field("name", Some("Your full name"), true)
///     .string_field_with_default("greeting", Some("How to greet"), false, "Hello")
///     .integer_field("age", Some("Your age"), false)
///     .enum_field(
///         "role",
///         Some("Select your role"),
///         vec!["admin".into(), "user".into(), "guest".into()],
///         true,
///     )
///     .enum_field_with_default(
///         "theme",
///         Some("Color theme"),
///         false,
///         &["light", "dark", "auto"],
///         "auto",
///     )
///     .boolean_field("subscribe", Some("Subscribe to updates"), false);
///
/// assert_eq!(schema.required, vec!["name", "role"]);
/// assert_eq!(schema.properties.len(), 6);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitFormSchema {
    /// Must be "object"
    #[serde(rename = "type")]
    pub schema_type: String,
    /// Property names mapped to their schema definitions, in the order the
    /// server declared them.
    ///
    /// Insertion order is preserved and is protocol-significant: a client
    /// renders the form fields in this order. A hash map would have made the
    /// rendered order arbitrary and, with `HashMap`'s per-process seed,
    /// unstable between runs of the same server (#1199).
    pub properties: indexmap::IndexMap<String, PrimitiveSchemaDefinition>,
    /// List of required property names
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
}

impl ElicitFormSchema {
    /// Create a new form schema
    pub fn new() -> Self {
        Self {
            schema_type: "object".to_string(),
            properties: indexmap::IndexMap::new(),
            required: Vec::new(),
        }
    }

    /// Add a string field
    pub fn string_field(mut self, name: &str, description: Option<&str>, required: bool) -> Self {
        self.properties.insert(
            name.to_string(),
            PrimitiveSchemaDefinition::String(StringSchema {
                schema_type: "string".to_string(),
                title: None,
                description: description.map(|s| s.to_string()),
                format: None,
                pattern: None,
                min_length: None,
                max_length: None,
                default: None,
            }),
        );
        if required {
            self.required.push(name.to_string());
        }
        self
    }

    /// Add a string field with a default value
    pub fn string_field_with_default(
        mut self,
        name: &str,
        description: Option<&str>,
        required: bool,
        default: &str,
    ) -> Self {
        self.properties.insert(
            name.to_string(),
            PrimitiveSchemaDefinition::String(StringSchema {
                schema_type: "string".to_string(),
                title: None,
                description: description.map(|s| s.to_string()),
                format: None,
                pattern: None,
                min_length: None,
                max_length: None,
                default: Some(default.to_string()),
            }),
        );
        if required {
            self.required.push(name.to_string());
        }
        self
    }

    /// Add an integer field
    pub fn integer_field(mut self, name: &str, description: Option<&str>, required: bool) -> Self {
        self.properties.insert(
            name.to_string(),
            PrimitiveSchemaDefinition::Integer(IntegerSchema {
                schema_type: "integer".to_string(),
                title: None,
                description: description.map(|s| s.to_string()),
                minimum: None,
                maximum: None,
                default: None,
            }),
        );
        if required {
            self.required.push(name.to_string());
        }
        self
    }

    /// Add an integer field with a default value
    pub fn integer_field_with_default(
        mut self,
        name: &str,
        description: Option<&str>,
        required: bool,
        default: i64,
    ) -> Self {
        self.properties.insert(
            name.to_string(),
            PrimitiveSchemaDefinition::Integer(IntegerSchema {
                schema_type: "integer".to_string(),
                title: None,
                description: description.map(|s| s.to_string()),
                minimum: None,
                maximum: None,
                default: Some(default),
            }),
        );
        if required {
            self.required.push(name.to_string());
        }
        self
    }

    /// Add a number field
    pub fn number_field(mut self, name: &str, description: Option<&str>, required: bool) -> Self {
        self.properties.insert(
            name.to_string(),
            PrimitiveSchemaDefinition::Number(NumberSchema {
                schema_type: "number".to_string(),
                title: None,
                description: description.map(|s| s.to_string()),
                minimum: None,
                maximum: None,
                default: None,
            }),
        );
        if required {
            self.required.push(name.to_string());
        }
        self
    }

    /// Add a number field with a default value
    pub fn number_field_with_default(
        mut self,
        name: &str,
        description: Option<&str>,
        required: bool,
        default: f64,
    ) -> Self {
        self.properties.insert(
            name.to_string(),
            PrimitiveSchemaDefinition::Number(NumberSchema {
                schema_type: "number".to_string(),
                title: None,
                description: description.map(|s| s.to_string()),
                minimum: None,
                maximum: None,
                default: Some(default),
            }),
        );
        if required {
            self.required.push(name.to_string());
        }
        self
    }

    /// Add a boolean field
    pub fn boolean_field(mut self, name: &str, description: Option<&str>, required: bool) -> Self {
        self.properties.insert(
            name.to_string(),
            PrimitiveSchemaDefinition::Boolean(BooleanSchema {
                schema_type: "boolean".to_string(),
                title: None,
                description: description.map(|s| s.to_string()),
                default: None,
            }),
        );
        if required {
            self.required.push(name.to_string());
        }
        self
    }

    /// Add a boolean field with a default value
    pub fn boolean_field_with_default(
        mut self,
        name: &str,
        description: Option<&str>,
        required: bool,
        default: bool,
    ) -> Self {
        self.properties.insert(
            name.to_string(),
            PrimitiveSchemaDefinition::Boolean(BooleanSchema {
                schema_type: "boolean".to_string(),
                title: None,
                description: description.map(|s| s.to_string()),
                default: Some(default),
            }),
        );
        if required {
            self.required.push(name.to_string());
        }
        self
    }

    /// Add a single-select enum field
    pub fn enum_field(
        mut self,
        name: &str,
        description: Option<&str>,
        options: Vec<String>,
        required: bool,
    ) -> Self {
        self.properties.insert(
            name.to_string(),
            PrimitiveSchemaDefinition::SingleSelectEnum(SingleSelectEnumSchema {
                schema_type: "string".to_string(),
                title: None,
                description: description.map(|s| s.to_string()),
                enum_values: options,
                default: None,
            }),
        );
        if required {
            self.required.push(name.to_string());
        }
        self
    }

    /// Add a single-select enum field with a default value
    pub fn enum_field_with_default(
        mut self,
        name: &str,
        description: Option<&str>,
        required: bool,
        options: &[&str],
        default: &str,
    ) -> Self {
        self.properties.insert(
            name.to_string(),
            PrimitiveSchemaDefinition::SingleSelectEnum(SingleSelectEnumSchema {
                schema_type: "string".to_string(),
                title: None,
                description: description.map(|s| s.to_string()),
                enum_values: options.iter().map(|s| s.to_string()).collect(),
                default: Some(default.to_string()),
            }),
        );
        if required {
            self.required.push(name.to_string());
        }
        self
    }

    /// Add a raw JSON schema field
    ///
    /// Use this for advanced schema features not covered by the typed builders.
    pub fn raw_field(mut self, name: &str, schema: serde_json::Value, required: bool) -> Self {
        self.properties
            .insert(name.to_string(), PrimitiveSchemaDefinition::Raw(schema));
        if required {
            self.required.push(name.to_string());
        }
        self
    }
}

impl Default for ElicitFormSchema {
    fn default() -> Self {
        Self::new()
    }
}

/// Primitive schema definition for form fields
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum PrimitiveSchemaDefinition {
    /// String field
    String(StringSchema),
    /// Integer field
    Integer(IntegerSchema),
    /// Number (floating-point) field
    Number(NumberSchema),
    /// Boolean field
    Boolean(BooleanSchema),
    /// Single-select enum field
    SingleSelectEnum(SingleSelectEnumSchema),
    /// Multi-select enum field
    MultiSelectEnum(MultiSelectEnumSchema),
    /// Raw JSON schema (for advanced/custom schemas)
    Raw(serde_json::Value),
}

// Dispatch on the JSON Schema `type` (and on `enum` for the select variants)
// rather than letting an untagged enum try variants in order. Every schema
// struct here declares `type` as a plain `String` that accepts any value, so
// the first variant matched everything: a `{"type": "boolean"}` field parsed
// as `String`, and a single-select's `enum` values were dropped entirely
// because `StringSchema` has nowhere to keep them (#1189).
impl<'de> Deserialize<'de> for PrimitiveSchemaDefinition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let schema_type = value.get("type").and_then(Value::as_str);
        let has_enum = value.get("enum").is_some();

        // An unrecognized shape is preserved verbatim as `Raw` rather than
        // rejected, so a server using a schema keyword this crate does not
        // model yet still round-trips through a client.
        fn parse<T, E>(value: Value) -> Result<T, E>
        where
            T: serde::de::DeserializeOwned,
            E: serde::de::Error,
        {
            serde_json::from_value(value).map_err(serde::de::Error::custom)
        }

        Ok(match (schema_type, has_enum) {
            (Some("string"), true) => Self::SingleSelectEnum(parse(value)?),
            (Some("string"), false) => Self::String(parse(value)?),
            (Some("integer"), _) => Self::Integer(parse(value)?),
            (Some("number"), _) => Self::Number(parse(value)?),
            (Some("boolean"), _) => Self::Boolean(parse(value)?),
            // The multi-select carries its choices in `items.enum`, so the
            // top-level `enum` check does not apply.
            (Some("array"), _) => Self::MultiSelectEnum(parse(value)?),
            _ => Self::Raw(value),
        })
    }
}

/// String field schema
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StringSchema {
    /// JSON Schema type marker; must be `"string"`.
    #[serde(rename = "type")]
    pub schema_type: String,
    /// Human-readable title for this field
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable help text for the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional semantic format hint such as `email` or `uri`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Regex pattern for validation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// Minimum permitted string length.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_length: Option<u64>,
    /// Maximum permitted string length.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u64>,
    /// Default value for this field
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// Integer field schema
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegerSchema {
    /// JSON Schema type marker; must be `"integer"`.
    #[serde(rename = "type")]
    pub schema_type: String,
    /// Human-readable title for this field
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable help text for the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Inclusive minimum accepted value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum: Option<i64>,
    /// Inclusive maximum accepted value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum: Option<i64>,
    /// Default value for this field
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<i64>,
}

/// Number field schema
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NumberSchema {
    /// JSON Schema type marker; must be `"number"`.
    #[serde(rename = "type")]
    pub schema_type: String,
    /// Human-readable title for this field
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable help text for the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Inclusive minimum accepted value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    /// Inclusive maximum accepted value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
    /// Default value for this field
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<f64>,
}

/// Boolean field schema
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BooleanSchema {
    /// JSON Schema type marker; must be `"boolean"`.
    #[serde(rename = "type")]
    pub schema_type: String,
    /// Human-readable title for this field
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable help text for the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Default value for this field
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<bool>,
}

/// Single-select enum schema
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SingleSelectEnumSchema {
    /// JSON Schema type marker; must be `"string"`.
    #[serde(rename = "type")]
    pub schema_type: String,
    /// Human-readable title for this field
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable help text for the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Exact string values the user may select.
    #[serde(rename = "enum")]
    pub enum_values: Vec<String>,
    /// Default value for this field
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// Multi-select enum schema
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiSelectEnumSchema {
    /// JSON Schema type marker; must be `"array"`.
    #[serde(rename = "type")]
    pub schema_type: String,
    /// Human-readable title for this field
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional human-readable help text for the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Schema constraining each selected array item.
    pub items: MultiSelectEnumItems,
    /// Whether duplicate selections are forbidden.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unique_items: Option<bool>,
    /// Default value for this field
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Vec<String>>,
}

/// Items definition for multi-select enum
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiSelectEnumItems {
    /// JSON Schema type marker for each item; must be `"string"`.
    #[serde(rename = "type")]
    pub schema_type: String,
    /// Exact string values permitted in the selection array.
    #[serde(rename = "enum")]
    pub enum_values: Vec<String>,
}

/// User action in response to elicitation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ElicitAction {
    /// User submitted the form/confirmed the action
    Accept,
    /// User explicitly declined the action
    Decline,
    /// User dismissed without making an explicit choice
    Cancel,
}

/// Result of an elicitation request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitResult {
    /// The user's action
    pub action: ElicitAction,
    /// Submitted form data (only present when action is Accept and mode was Form)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<std::collections::HashMap<String, ElicitFieldValue>>,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

impl ElicitResult {
    /// Create an accept result with content
    pub fn accept(content: std::collections::HashMap<String, ElicitFieldValue>) -> Self {
        Self {
            action: ElicitAction::Accept,
            content: Some(content),
            meta: None,
        }
    }

    /// Create a decline result
    pub fn decline() -> Self {
        Self {
            action: ElicitAction::Decline,
            content: None,
            meta: None,
        }
    }

    /// Create a cancel result
    pub fn cancel() -> Self {
        Self {
            action: ElicitAction::Cancel,
            content: None,
            meta: None,
        }
    }
}

/// Value from an elicitation form field
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum ElicitFieldValue {
    /// A string response.
    String(String),
    /// A floating-point numeric response.
    Number(f64),
    /// An integer response.
    Integer(i64),
    /// A boolean response.
    Boolean(bool),
    /// A multi-select response containing the selected string values.
    StringArray(Vec<String>),
}

/// Parameters for elicitation complete notification.
///
/// **2025-11-25 and earlier only.** The final 2026-07-28 schema removes this
/// notification (and the `elicitationId` field of URL-mode elicitation, see
/// [`ElicitUrlParams::elicitation_id`]) -- not deprecated, removed outright,
/// unlike Roots/Sampling/Logging's SEP-2577 12-month deprecation window.
/// Under [Multi Round-Trip Requests](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2322)
/// (SEP-2322, not yet implemented by this crate -- see #950), the client
/// learns the outcome of an out-of-band interaction by retrying the original
/// request rather than receiving a server-initiated completion signal, so
/// the correlating ID this type carries no longer fits the protocol.
/// tower-mcp does not currently send or handle this notification on any
/// path; this type exists for callers constructing it directly against the
/// 2025-11-25 wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationCompleteParams {
    /// The ID of the elicitation that completed
    pub elicitation_id: String,
    /// Optional protocol-level metadata
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

// =============================================================================
// Common
// =============================================================================

/// Empty successful result object (`{}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmptyResult {}

// =============================================================================
// Parsing
// =============================================================================

impl McpRequest {
    /// Parse from JSON-RPC request
    pub fn from_jsonrpc(req: &JsonRpcRequest) -> Result<Self, crate::error::Error> {
        let params = req
            .params
            .clone()
            .unwrap_or(Value::Object(Default::default()));

        match req.method.as_str() {
            "initialize" => {
                let p: InitializeParams = serde_json::from_value(params)?;
                Ok(McpRequest::Initialize(p))
            }
            "tools/list" => {
                let p: ListToolsParams = serde_json::from_value(params).unwrap_or_default();
                Ok(McpRequest::ListTools(p))
            }
            "tools/call" => {
                let p: CallToolParams = serde_json::from_value(params)?;
                Ok(McpRequest::CallTool(p))
            }
            "resources/list" => {
                let p: ListResourcesParams = serde_json::from_value(params).unwrap_or_default();
                Ok(McpRequest::ListResources(p))
            }
            "resources/templates/list" => {
                let p: ListResourceTemplatesParams =
                    serde_json::from_value(params).unwrap_or_default();
                Ok(McpRequest::ListResourceTemplates(p))
            }
            "resources/read" => {
                let p: ReadResourceParams = serde_json::from_value(params)?;
                Ok(McpRequest::ReadResource(p))
            }
            "resources/subscribe" => {
                let p: SubscribeResourceParams = serde_json::from_value(params)?;
                Ok(McpRequest::SubscribeResource(p))
            }
            "resources/unsubscribe" => {
                let p: UnsubscribeResourceParams = serde_json::from_value(params)?;
                Ok(McpRequest::UnsubscribeResource(p))
            }
            "prompts/list" => {
                let p: ListPromptsParams = serde_json::from_value(params).unwrap_or_default();
                Ok(McpRequest::ListPrompts(p))
            }
            "prompts/get" => {
                let p: GetPromptParams = serde_json::from_value(params)?;
                Ok(McpRequest::GetPrompt(p))
            }
            // Tasks methods. The canonical method names defined by SEP-2663
            // (`tasks/get`, `tasks/update`, `tasks/cancel`) are unprefixed; the
            // `io.modelcontextprotocol/tasks` identifier is the *extension*
            // label used for capability declarations, not a method prefix.
            // `tasks/list` and `tasks/result` are 2025-11-25 experimental
            // methods that final SEP-2663 removes; they intentionally fall
            // through to `Unknown` so the router answers `MethodNotFound`.
            "tasks/get" => {
                let p: GetTaskInfoParams = serde_json::from_value(params)?;
                Ok(McpRequest::GetTaskInfo(p))
            }
            "tasks/update" => {
                let p: UpdateTaskParams = serde_json::from_value(params)?;
                Ok(McpRequest::UpdateTask(p))
            }
            "tasks/cancel" => {
                let p: CancelTaskParams = serde_json::from_value(params)?;
                Ok(McpRequest::CancelTask(p))
            }
            "ping" => Ok(McpRequest::Ping),
            "logging/setLevel" => {
                let p: SetLogLevelParams = serde_json::from_value(params)?;
                Ok(McpRequest::SetLoggingLevel(p))
            }
            "completion/complete" => {
                let p: CompleteParams = serde_json::from_value(params)?;
                Ok(McpRequest::Complete(p))
            }
            "server/discover" => {
                // SEP-2575: empty-or-missing params is valid.
                let p: DiscoverParams = serde_json::from_value(params).unwrap_or_default();
                Ok(McpRequest::Discover(p))
            }
            "subscriptions/listen" => {
                // SEP-2575 / SEP-2567: client-initiated streaming.
                // Empty-or-missing params is valid.
                let p: SubscriptionsListenParams =
                    serde_json::from_value(params).unwrap_or_default();
                Ok(McpRequest::SubscriptionsListen(p))
            }
            method => Ok(McpRequest::Unknown {
                method: method.to_string(),
                params: req.params.clone(),
            }),
        }
    }
}

impl McpNotification {
    /// Parse from JSON-RPC notification
    pub fn from_jsonrpc(notif: &JsonRpcNotification) -> Result<Self, crate::error::Error> {
        let params = notif
            .params
            .clone()
            .unwrap_or(Value::Object(Default::default()));

        match notif.method.as_str() {
            notifications::INITIALIZED => Ok(McpNotification::Initialized),
            notifications::CANCELLED => {
                let p: CancelledParams = serde_json::from_value(params)?;
                Ok(McpNotification::Cancelled(p))
            }
            notifications::PROGRESS => {
                let p: ProgressParams = serde_json::from_value(params)?;
                Ok(McpNotification::Progress(p))
            }
            notifications::ROOTS_LIST_CHANGED => Ok(McpNotification::RootsListChanged),
            method => Ok(McpNotification::Unknown {
                method: method.to_string(),
                params: notif.params.clone(),
            }),
        }
    }
}

// =============================================================================
// 2026-07-28 draft surface: result discrimination, Multi Round-Trip Requests
// (SEP-2322), subscription streams (SEP-2575), and the meta model split.
//
// Everything here is additive and version-gated by omission: every new field
// uses `skip_serializing_if`, so a value carrying none of it serializes
// byte-identically to 2025-11-25 output. These shapes track the current
// `2026-07-28` draft schema; a re-verify-against-final checkpoint is in #929.
// =============================================================================

/// The type discriminator carried by a 2026-07-28 result (SEP-2322).
///
/// The draft schema requires `resultType` on every result. For backward
/// compatibility a client MUST treat an absent field as [`ResultType::Complete`];
/// use [`ResultType::from_result_value`] to read it with that rule applied.
/// `Complete` is the [`Default`].
///
/// The wire form is an open string. `"complete"` and `"input_required"` are
/// defined by SEP-2322; extensions (for example the Tasks extension's
/// `"task"`) may define further values, captured by [`ResultType::Other`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ResultType {
    /// The request completed; the result carries the final content.
    #[default]
    Complete,
    /// The server needs more input before it can complete the request; the
    /// result is an [`InputRequiredResult`].
    InputRequired,
    /// A value not defined by SEP-2322, for example an extension discriminator.
    Other(String),
}

impl ResultType {
    /// The wire string for this discriminator.
    pub fn as_str(&self) -> &str {
        match self {
            ResultType::Complete => "complete",
            ResultType::InputRequired => "input_required",
            ResultType::Other(s) => s.as_str(),
        }
    }

    /// Whether this is the default `"complete"` discriminator.
    pub fn is_complete(&self) -> bool {
        matches!(self, ResultType::Complete)
    }

    /// Read the discriminator from a raw result object, applying the SEP-2322
    /// backward-compatibility rule: a missing (or non-string) `resultType`
    /// reads as [`ResultType::Complete`].
    pub fn from_result_value(value: &Value) -> ResultType {
        match value.get("resultType").and_then(Value::as_str) {
            None => ResultType::Complete,
            Some(s) => ResultType::from(s.to_string()),
        }
    }

    /// Stamp this discriminator onto a serialized result object, gated on the
    /// negotiated protocol version. Returns `true` when the field was written.
    ///
    /// This is how `resultType` reaches the result base: the field is required
    /// on 2026-07-28 results but must not appear on 2025-11-25 ones, and serde
    /// cannot see the negotiated version, so the gate lives here rather than in
    /// each result struct. Serialize the result, then stamp the value on its way
    /// out. Nothing is written when
    /// [`version_carries_result_type`] is false, when `result` is not a JSON
    /// object, or when the result already carries a `resultType` (results that
    /// own their discriminator, such as [`InputRequiredResult`] and
    /// [`CreateTaskResult`], keep it on every version).
    pub fn stamp_into(&self, result: &mut Value, protocol_version: &str) -> bool {
        if !version_carries_result_type(protocol_version) {
            return false;
        }
        let Some(obj) = result.as_object_mut() else {
            return false;
        };
        if obj.contains_key("resultType") {
            return false;
        }
        obj.insert(
            "resultType".to_string(),
            Value::String(self.as_str().to_string()),
        );
        true
    }
}

/// Whether results on the given negotiated protocol version carry the SEP-2322
/// `resultType` discriminator.
///
/// `resultType` is part of the 2026-07-28 spec (SEP-2322); on 2025-11-25 it is
/// absent and readers apply the "absent means `complete`" rule. Only known,
/// explicitly implemented protocol versions opt in; an unknown future date
/// must not silently inherit 2026 behavior.
pub fn version_carries_result_type(protocol_version: &str) -> bool {
    protocol_version == PROTOCOL_VERSION_2026_07_28
}

impl From<String> for ResultType {
    fn from(s: String) -> ResultType {
        match s.as_str() {
            "complete" => ResultType::Complete,
            "input_required" => ResultType::InputRequired,
            _ => ResultType::Other(s),
        }
    }
}

impl From<ResultType> for String {
    fn from(r: ResultType) -> String {
        match r {
            ResultType::Complete => "complete".to_string(),
            ResultType::InputRequired => "input_required".to_string(),
            ResultType::Other(s) => s,
        }
    }
}

impl serde::Serialize for ResultType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for ResultType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<ResultType, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(ResultType::from(s))
    }
}

/// A single server-initiated request the client must fulfil during a Multi
/// Round-Trip Request (SEP-2322). Serialized adjacently as `{method, params}`,
/// mirroring the JSON-RPC request embedded in an [`InputRequests`] map.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
#[non_exhaustive]
pub enum InputRequest {
    /// `sampling/createMessage`
    #[serde(rename = "sampling/createMessage")]
    CreateMessage(CreateMessageParams),
    /// `roots/list`
    #[serde(rename = "roots/list")]
    ListRoots(ListRootsParams),
    /// `elicitation/create`
    #[serde(rename = "elicitation/create")]
    Elicit(ElicitRequestParams),
}

impl InputRequest {
    /// The MCP method name for this input request.
    pub fn method_name(&self) -> &str {
        match self {
            InputRequest::CreateMessage(_) => "sampling/createMessage",
            InputRequest::ListRoots(_) => "roots/list",
            InputRequest::Elicit(_) => "elicitation/create",
        }
    }
}

/// A single client response to a server-initiated [`InputRequest`] (SEP-2322).
/// Untagged: the wire value is the bare result object, correlated to its
/// request by its key in the [`InputResponses`] map.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum InputResponse {
    /// Result of a `sampling/createMessage` request.
    CreateMessage(CreateMessageResult),
    /// Result of a `roots/list` request.
    ListRoots(ListRootsResult),
    /// Result of an `elicitation/create` request.
    Elicit(ElicitResult),
}

/// Map of server-initiated requests the client must fulfil, keyed by
/// server-assigned identifiers (SEP-2322).
pub type InputRequests = std::collections::BTreeMap<String, InputRequest>;

/// Map of client responses to [`InputRequests`], keyed by the same
/// identifiers (SEP-2322).
pub type InputResponses = std::collections::BTreeMap<String, InputResponse>;

/// A 2026-07-28 result signalling the server needs more input before it can
/// complete the original request (SEP-2322).
///
/// At least one of `input_requests` or `request_state` is present. The client
/// fulfils the requests, then retries the original request carrying the
/// matching `inputResponses` (and echoing `requestState`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputRequiredResult {
    /// Always [`ResultType::InputRequired`].
    #[serde(rename = "resultType", default = "result_type_input_required")]
    pub result_type: ResultType,
    /// Requests the server needs fulfilled before the client retries.
    #[serde(
        rename = "inputRequests",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub input_requests: Option<InputRequests>,
    /// Opaque blob the client echoes back on retry. The client MUST NOT
    /// interpret it.
    #[serde(
        rename = "requestState",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub request_state: Option<String>,
    /// Result-level metadata (`_meta`).
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<Value>,
}

fn result_type_input_required() -> ResultType {
    ResultType::InputRequired
}

impl InputRequiredResult {
    /// An empty `input_required` result. Set `input_requests` and/or
    /// `request_state` before returning it.
    pub fn new() -> Self {
        InputRequiredResult {
            result_type: ResultType::InputRequired,
            input_requests: None,
            request_state: None,
            meta: None,
        }
    }

    /// Create an input-required result carrying the requests the client must
    /// fulfil before retrying the original method.
    pub fn with_requests(input_requests: InputRequests) -> Self {
        Self {
            input_requests: Some(input_requests),
            ..Self::new()
        }
    }

    /// Attach opaque server state that the client must echo unchanged on its
    /// next attempt.
    pub fn with_request_state(mut self, request_state: impl Into<String>) -> Self {
        self.request_state = Some(request_state.into());
        self
    }

    /// Validate the invariants required by SEP-2322.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.result_type != ResultType::InputRequired {
            return Err("resultType must be \"input_required\"");
        }
        if self.input_requests.is_none() && self.request_state.is_none() {
            return Err("at least one of inputRequests or requestState must be present");
        }
        Ok(())
    }
}

impl Default for InputRequiredResult {
    fn default() -> Self {
        InputRequiredResult::new()
    }
}

/// Result of a request that may either complete or ask the client for
/// additional input (SEP-2322).
///
/// This is the server-handler outcome for `tools/call`, `prompts/get`, and
/// `resources/read`. Ordinary handlers continue returning their established
/// complete result type; MRTR-aware handlers return this enum.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestOutcome<T> {
    /// The request completed normally.
    Complete(T),
    /// The client must fulfil the embedded requests and retry.
    InputRequired(InputRequiredResult),
}

impl<T> RequestOutcome<T> {
    /// Create an input-required outcome.
    pub fn input_required(result: InputRequiredResult) -> Self {
        RequestOutcome::InputRequired(result)
    }

    /// Whether this outcome completed the request.
    pub fn is_complete(&self) -> bool {
        matches!(self, RequestOutcome::Complete(_))
    }

    /// Borrow the complete result, if present.
    pub fn as_complete(&self) -> Option<&T> {
        match self {
            RequestOutcome::Complete(result) => Some(result),
            RequestOutcome::InputRequired(_) => None,
        }
    }

    /// Borrow the input-required result, if present.
    pub fn as_input_required(&self) -> Option<&InputRequiredResult> {
        match self {
            RequestOutcome::Complete(_) => None,
            RequestOutcome::InputRequired(result) => Some(result),
        }
    }
}

impl<T> From<T> for RequestOutcome<T> {
    fn from(result: T) -> Self {
        RequestOutcome::Complete(result)
    }
}

/// Subscription meta (`_meta`) carried by messages delivered on a
/// `subscriptions/listen` stream.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotificationMeta {
    /// Identifies the subscription stream a notification was delivered on: the
    /// JSON-RPC id of the `subscriptions/listen` request that opened it.
    /// Absent on notifications not delivered via a subscription stream.
    #[serde(
        rename = "io.modelcontextprotocol/subscriptionId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub subscription_id: Option<RequestId>,
}

/// Result meta (`_meta`) fields the server may attach to a response (SEP-2575).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResultMeta {
    /// Identifies the server software producing the response. Self-reported and
    /// unverified; intended for display, logging, and debugging.
    #[serde(
        rename = "io.modelcontextprotocol/serverInfo",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub server_info: Option<Implementation>,
}

/// A required `_meta` key was missing for the negotiated protocol version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingMetaKey(pub &'static str);

impl std::fmt::Display for MissingMetaKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "missing required _meta key for the negotiated protocol version: {}",
            self.0
        )
    }
}

impl std::error::Error for MissingMetaKey {}

/// Notification types a client opts in to on a `subscriptions/listen` stream
/// (SEP-2575). Replaces the 2025-11-25 `resources/subscribe` RPC. The server
/// MUST NOT send a notification type the client did not request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionFilter {
    /// Receive `notifications/tools/list_changed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_list_changed: Option<bool>,
    /// Receive `notifications/prompts/list_changed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts_list_changed: Option<bool>,
    /// Receive `notifications/resources/list_changed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources_list_changed: Option<bool>,
    /// Resource URIs to receive `notifications/resources/updated` for.
    /// Replaces the former `resources/subscribe` RPC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_subscriptions: Option<Vec<String>>,
    /// Task IDs to receive `notifications/tasks` for (SEP-2663). Unlike the
    /// list-changed flags this names individual tasks, so a client observes
    /// only the tasks it created. A client that sets this without declaring
    /// the `io.modelcontextprotocol/tasks` extension is answered with
    /// `-32021`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_ids: Option<Vec<String>>,
}

/// Parameters for a `notifications/subscriptions/acknowledged` notification:
/// the subset of requested notification types the server will honor.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubscriptionsAcknowledgedParams {
    /// Identifies the `subscriptions/listen` request being acknowledged.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::protocol::meta_object_serde"
    )]
    pub meta: Option<NotificationMeta>,
    /// The notification types the server agreed to honor. Types the server does
    /// not support are omitted.
    pub notifications: SubscriptionFilter,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::JsonRpcError;

    // =========================================================================
    // Metadata and extension key validation
    // =========================================================================

    #[test]
    fn meta_key_validation_matches_the_spec_grammar() {
        for key in [
            "",
            "progressToken",
            "traceparent",
            "io.modelcontextprotocol/protocolVersion",
            "com.example/feature-name_1.2",
            "vendor/",
        ] {
            assert_eq!(validate_meta_key(key), Ok(()), "{key:?} should be valid");
        }

        for key in [
            "/name",
            "1vendor/name",
            "com..example/name",
            "com.example-/name",
            "com_example/name",
            "com.example//name",
            "com.example/-name",
            "name-",
            "na:me",
            "métadata",
        ] {
            assert!(validate_meta_key(key).is_err(), "{key:?} should be invalid");
        }

        assert!(matches!(
            validate_extension_identifier("tasks"),
            Err(MetaValidationError::MissingExtensionPrefix(key)) if key == "tasks"
        ));
        assert_eq!(
            validate_extension_identifier("io.modelcontextprotocol/tasks"),
            Ok(())
        );
    }

    #[test]
    fn every_typed_meta_field_rejects_invalid_objects_and_keys() {
        let valid: InitializeParams = serde_json::from_value(serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "client", "version": "1" },
            "_meta": {
                "progressToken": 1,
                "com.example/feature": true
            }
        }))
        .unwrap();
        assert!(valid.meta.is_some());

        for meta in [
            serde_json::json!([]),
            serde_json::json!({"bad/key/again": true}),
            serde_json::json!({"-bad": true}),
        ] {
            let value = serde_json::json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "client", "version": "1" },
                "_meta": meta
            });
            assert!(serde_json::from_value::<InitializeParams>(value).is_err());
        }

        let invalid_outbound = InitializeParams {
            protocol_version: "2025-11-25".to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: Implementation {
                name: "client".to_string(),
                version: "1".to_string(),
                title: None,
                description: None,
                icons: None,
                website_url: None,
                meta: None,
            },
            meta: Some(serde_json::json!({"bad key": true})),
        };
        assert!(serde_json::to_value(invalid_outbound).is_err());
    }

    #[test]
    fn capability_extensions_require_prefixed_keys_and_object_settings() {
        let valid: ClientCapabilities = serde_json::from_value(serde_json::json!({
            "extensions": {
                "io.modelcontextprotocol/tasks": {},
                "com.example/feature": { "version": 1 }
            }
        }))
        .unwrap();
        assert_eq!(valid.extensions.as_ref().map(HashMap::len), Some(2));

        for extensions in [
            serde_json::json!({"tasks": {}}),
            serde_json::json!({"com.example/-feature": {}}),
            serde_json::json!({"com.example/feature": true}),
        ] {
            let value = serde_json::json!({ "extensions": extensions });
            assert!(serde_json::from_value::<ClientCapabilities>(value).is_err());
        }

        let invalid_outbound = ServerCapabilities {
            extensions: Some(HashMap::from([(
                "unprefixed".to_string(),
                serde_json::json!({}),
            )])),
            ..ServerCapabilities::default()
        };
        assert!(serde_json::to_value(invalid_outbound).is_err());
    }

    // =========================================================================
    // Protocol version constants
    // =========================================================================

    #[test]
    fn released_version_is_known_and_not_enabled_by_default() {
        assert_eq!(PROTOCOL_VERSION_2026_07_28, "2026-07-28");
        assert!(KNOWN_PROTOCOL_VERSIONS.contains(&PROTOCOL_VERSION_2026_07_28));
        assert!(KNOWN_PROTOCOL_VERSIONS.contains(&"2025-06-18"));
        assert!(!SUPPORTED_PROTOCOL_VERSIONS.contains(&"2025-06-18"));
        assert!(
            !SUPPORTED_PROTOCOL_VERSIONS.contains(&PROTOCOL_VERSION_2026_07_28),
            "the released 2026-07-28 implementation remains explicitly \
             compile-time/runtime selectable; current default set: {:?}",
            SUPPORTED_PROTOCOL_VERSIONS
        );
        assert!(SUPPORTED_PROTOCOL_VERSIONS.contains(&LATEST_PROTOCOL_VERSION));
    }

    #[test]
    #[allow(deprecated)]
    fn deprecated_version_aliases_remain_compatible() {
        assert_eq!(EXPERIMENTAL_PROTOCOL_VERSION, PROTOCOL_VERSION_2026_07_28);
        assert_eq!(UPCOMING_PROTOCOL_VERSION, PROTOCOL_VERSION_2026_07_28);
    }

    // =========================================================================
    // SEP-2575 (server/discover) wire-format tests
    // =========================================================================

    #[test]
    fn discover_request_round_trips_with_no_params() {
        // Per SEP-2575 the params field is empty/optional. Both shapes
        // round-trip into McpRequest::Discover.
        let no_params = r#"{"jsonrpc":"2.0","id":1,"method":"server/discover"}"#;
        let req: JsonRpcRequest = serde_json::from_str(no_params).unwrap();
        let parsed = McpRequest::from_jsonrpc(&req).unwrap();
        assert!(matches!(parsed, McpRequest::Discover(_)));
        assert_eq!(parsed.method_name(), "server/discover");

        let empty_params = r#"{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{}}"#;
        let req: JsonRpcRequest = serde_json::from_str(empty_params).unwrap();
        let parsed = McpRequest::from_jsonrpc(&req).unwrap();
        assert!(matches!(parsed, McpRequest::Discover(_)));
    }

    #[test]
    fn discover_result_serializes_supported_versions_array() {
        let r = DiscoverResult {
            supported_versions: vec!["2026-07-28".into(), "2025-11-25".into()],
            capabilities: ServerCapabilities::default(),
            ttl_ms: None,
            cache_scope: None,
            instructions: None,
            meta: Some(ResultMeta {
                server_info: Some(Implementation {
                    name: "test-server".into(),
                    version: "1.0.0".into(),
                    title: None,
                    description: None,
                    icons: None,
                    website_url: None,
                    meta: None,
                }),
            }),
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(
            json["supportedVersions"],
            serde_json::json!(["2026-07-28", "2025-11-25"])
        );
        assert!(
            json.get("protocolVersion").is_none(),
            "server/discover must use supportedVersions, not protocolVersion, got: {json}"
        );
        assert_eq!(
            json["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            "test-server"
        );
        assert!(
            json.get("instructions").is_none(),
            "instructions must be omitted when None, got: {json}"
        );
    }

    #[test]
    fn discover_result_omits_meta_ttl_and_cache_scope_when_none() {
        let r = DiscoverResult {
            supported_versions: vec!["2026-07-28".into()],
            capabilities: ServerCapabilities::default(),
            ttl_ms: None,
            cache_scope: None,
            instructions: None,
            meta: None,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert!(json.get("_meta").is_none());
        assert!(json.get("ttlMs").is_none());
        assert!(json.get("cacheScope").is_none());
    }

    // =========================================================================
    // SEP-2575 / SEP-2567 (subscriptions/listen) wire-format tests
    // =========================================================================

    #[test]
    fn subscriptions_listen_request_round_trips_with_no_params() {
        // Per SEP-2575 / SEP-2567, params is empty/optional.
        let no_params = r#"{"jsonrpc":"2.0","id":1,"method":"subscriptions/listen"}"#;
        let req: JsonRpcRequest = serde_json::from_str(no_params).unwrap();
        let parsed = McpRequest::from_jsonrpc(&req).unwrap();
        assert!(matches!(parsed, McpRequest::SubscriptionsListen(_)));
        assert_eq!(parsed.method_name(), "subscriptions/listen");

        let empty_params =
            r#"{"jsonrpc":"2.0","id":2,"method":"subscriptions/listen","params":{}}"#;
        let req: JsonRpcRequest = serde_json::from_str(empty_params).unwrap();
        let parsed = McpRequest::from_jsonrpc(&req).unwrap();
        assert!(matches!(parsed, McpRequest::SubscriptionsListen(_)));
    }

    #[test]
    fn subscriptions_listen_request_serializes_with_correct_method() {
        let req = JsonRpcRequest::new(42i64, "subscriptions/listen");
        let json = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["method"], "subscriptions/listen");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 42);
    }

    // =========================================================================
    // SEP-2549 (TTL on list results) wire-format tests
    // =========================================================================

    #[test]
    fn list_tools_result_omits_ttl_and_cache_scope_by_default() {
        let r = ListToolsResult {
            tools: vec![],
            next_cursor: None,
            ttl_ms: None,
            cache_scope: None,
            meta: None,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert!(
            json.get("ttlMs").is_none(),
            "ttlMs must be omitted when None, got: {json}"
        );
        assert!(
            json.get("cacheScope").is_none(),
            "cacheScope must be omitted when None, got: {json}"
        );
    }

    #[test]
    fn list_tools_result_emits_ttl_and_cache_scope_when_set() {
        let r = ListToolsResult {
            tools: vec![],
            next_cursor: None,
            ttl_ms: Some(60_000),
            cache_scope: Some(CacheScope::Public),
            meta: None,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["ttlMs"], 60_000);
        assert_eq!(json["cacheScope"], "public");
    }

    #[test]
    fn cache_scope_roundtrips_via_lowercase_strings() {
        // Final SEP-2549 wire values are "public" and "private".
        for (scope, wire) in [
            (CacheScope::Public, "public"),
            (CacheScope::Private, "private"),
        ] {
            let s = serde_json::to_value(scope).unwrap();
            assert_eq!(s, serde_json::json!(wire));
            let back: CacheScope = serde_json::from_value(s).unwrap();
            assert_eq!(back, scope);
        }
    }

    // =========================================================================
    // SEP-2577 / SEP-2596 (deprecation metadata) wire-format tests
    // =========================================================================

    #[test]
    fn roots_capability_omits_deprecated_by_default() {
        let cap = RootsCapability {
            list_changed: true,
            deprecated: None,
        };
        let json = serde_json::to_value(&cap).unwrap();
        assert!(
            json.get("deprecated").is_none(),
            "deprecated must be omitted when None to avoid changing wire output for existing servers, got: {json}"
        );
    }

    #[test]
    fn capability_emits_deprecation_info_when_flagged() {
        let cap = LoggingCapability {
            deprecated: Some(DeprecationInfo {
                since: Some("2026-07-28".into()),
                remove_in: Some("2027-07-28".into()),
                message: Some("Logging moves to OpenTelemetry per SEP-2577".into()),
                replacement: Some(
                    "https://github.com/modelcontextprotocol/modelcontextprotocol/issues/2577"
                        .into(),
                ),
            }),
        };
        let json = serde_json::to_value(&cap).unwrap();
        let dep = &json["deprecated"];
        assert_eq!(dep["since"], "2026-07-28");
        assert_eq!(dep["removeIn"], "2027-07-28");
        assert!(dep["message"].as_str().unwrap().contains("OpenTelemetry"));
        assert!(dep["replacement"].as_str().unwrap().contains("2577"));
    }

    #[test]
    fn deprecation_info_round_trip_with_partial_fields() {
        // Forward-compatible: a server that only sets `since` round-trips
        // without losing or adding fields.
        let wire = r#"{"since":"2026-07-28"}"#;
        let info: DeprecationInfo = serde_json::from_str(wire).unwrap();
        assert_eq!(info.since.as_deref(), Some("2026-07-28"));
        assert!(info.remove_in.is_none());
        let back = serde_json::to_string(&info).unwrap();
        assert_eq!(back, wire);
    }

    #[test]
    fn cancelled_params_serializes_request_id_when_present() {
        let p = CancelledParams {
            request_id: Some(RequestId::Number(42)),
            reason: Some("user abort".into()),
            meta: None,
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["requestId"], serde_json::json!(42));
        assert_eq!(json["reason"], serde_json::json!("user abort"));
    }

    #[test]
    fn cancelled_params_emits_null_request_id_when_absent() {
        // Spec REQUIRES requestId on notifications/cancelled. We keep
        // Option<RequestId> on the receive side for tolerance, but we
        // must NOT silently omit it on the send side -- emit null so a
        // strict receiver can detect and reject the malformed message.
        let p = CancelledParams {
            request_id: None,
            reason: None,
            meta: None,
        };
        let json = serde_json::to_value(&p).unwrap();
        assert!(
            json.get("requestId").is_some(),
            "requestId field must be present per MCP spec, got: {json}"
        );
        assert!(
            json["requestId"].is_null(),
            "requestId must serialize as null when unset, got: {}",
            json["requestId"]
        );
    }

    #[test]
    fn cancelled_params_deserializes_null_request_id() {
        let wire = r#"{"requestId":null,"reason":"x"}"#;
        let p: CancelledParams = serde_json::from_str(wire).unwrap();
        assert!(p.request_id.is_none());
        assert_eq!(p.reason.as_deref(), Some("x"));
    }

    #[test]
    fn cancelled_params_deserializes_missing_request_id() {
        // Tolerate peers that omit the field entirely.
        let wire = r#"{"reason":"x"}"#;
        let p: CancelledParams = serde_json::from_str(wire).unwrap();
        assert!(p.request_id.is_none());
    }

    #[test]
    fn error_response_serializes_id_null_when_unknown() {
        let resp = JsonRpcResponse::error(None, JsonRpcError::parse_error("bad json"));
        let json = serde_json::to_value(&resp).unwrap();
        assert!(
            json.get("id").is_some(),
            "id field must be present per JSON-RPC 2.0, got: {json}"
        );
        assert!(
            json["id"].is_null(),
            "id must serialize as null when unknown, got: {}",
            json["id"]
        );
    }

    #[test]
    fn error_response_serializes_id_when_known() {
        let resp =
            JsonRpcResponse::error(Some(RequestId::Number(7)), JsonRpcError::parse_error("bad"));
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["id"], serde_json::json!(7));
    }

    #[test]
    fn error_response_deserializes_null_id() {
        let wire = r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"x"}}"#;
        let resp: JsonRpcResponse = serde_json::from_str(wire).unwrap();
        match resp {
            JsonRpcResponse::Error(e) => assert!(e.id.is_none()),
            _ => panic!("expected Error variant"),
        }
    }

    #[test]
    fn test_content_text_constructor() {
        let content = Content::text("hello world");
        assert_eq!(content.as_text(), Some("hello world"));

        // Verify it creates a Text variant with no annotations
        match &content {
            Content::Text {
                text, annotations, ..
            } => {
                assert_eq!(text, "hello world");
                assert!(annotations.is_none());
            }
            _ => panic!("expected Content::Text"),
        }

        // Works with String too
        let content = Content::text(String::from("owned"));
        assert_eq!(content.as_text(), Some("owned"));
    }

    #[test]
    fn test_elicit_form_schema_builder() {
        let schema = ElicitFormSchema::new()
            .string_field("name", Some("Your name"), true)
            .number_field("age", Some("Your age"), false)
            .boolean_field("agree", Some("Do you agree?"), true)
            .enum_field(
                "color",
                Some("Favorite color"),
                vec!["red".to_string(), "green".to_string(), "blue".to_string()],
                false,
            );

        assert_eq!(schema.schema_type, "object");
        assert_eq!(schema.properties.len(), 4);
        assert_eq!(schema.required.len(), 2);
        assert!(schema.required.contains(&"name".to_string()));
        assert!(schema.required.contains(&"agree".to_string()));
    }

    #[test]
    fn test_elicit_form_schema_serialization() {
        let schema = ElicitFormSchema::new().string_field("username", Some("Enter username"), true);

        let json = serde_json::to_value(&schema).unwrap();
        assert_eq!(json["type"], "object");
        assert!(json["properties"]["username"]["type"] == "string");
        assert!(
            json["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("username"))
        );
    }

    #[test]
    fn test_elicit_result_accept() {
        let mut content = std::collections::HashMap::new();
        content.insert(
            "name".to_string(),
            ElicitFieldValue::String("Alice".to_string()),
        );
        content.insert("age".to_string(), ElicitFieldValue::Integer(30));

        let result = ElicitResult::accept(content);
        assert_eq!(result.action, ElicitAction::Accept);
        assert!(result.content.is_some());
    }

    #[test]
    fn test_elicit_result_decline() {
        let result = ElicitResult::decline();
        assert_eq!(result.action, ElicitAction::Decline);
        assert!(result.content.is_none());
    }

    #[test]
    fn test_elicit_result_cancel() {
        let result = ElicitResult::cancel();
        assert_eq!(result.action, ElicitAction::Cancel);
        assert!(result.content.is_none());
    }

    #[test]
    fn test_elicit_mode_serialization() {
        assert_eq!(
            serde_json::to_string(&ElicitMode::Form).unwrap(),
            "\"form\""
        );
        assert_eq!(serde_json::to_string(&ElicitMode::Url).unwrap(), "\"url\"");
    }

    #[test]
    fn test_elicit_action_serialization() {
        assert_eq!(
            serde_json::to_string(&ElicitAction::Accept).unwrap(),
            "\"accept\""
        );
        assert_eq!(
            serde_json::to_string(&ElicitAction::Decline).unwrap(),
            "\"decline\""
        );
        assert_eq!(
            serde_json::to_string(&ElicitAction::Cancel).unwrap(),
            "\"cancel\""
        );
    }

    #[test]
    fn test_elicitation_capability() {
        let cap = ElicitationCapability {
            form: Some(ElicitationFormCapability {}),
            url: None,
        };

        let json = serde_json::to_value(&cap).unwrap();
        assert!(json["form"].is_object());
        assert!(json.get("url").is_none());
    }

    #[test]
    fn test_client_capabilities_with_elicitation() {
        let caps = ClientCapabilities {
            roots: None,
            sampling: None,
            elicitation: Some(ElicitationCapability {
                form: Some(ElicitationFormCapability {}),
                url: Some(ElicitationUrlCapability {}),
            }),
            tasks: None,
            experimental: None,
            extensions: None,
        };

        let json = serde_json::to_value(&caps).unwrap();
        assert!(json["elicitation"]["form"].is_object());
        assert!(json["elicitation"]["url"].is_object());
    }

    #[test]
    fn test_elicit_url_params() {
        let params = ElicitUrlParams {
            mode: Some(ElicitMode::Url),
            elicitation_id: "abc123".to_string(),
            message: "Please authorize".to_string(),
            url: "https://example.com/auth".to_string(),
            meta: None,
        };

        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json["mode"], "url");
        assert_eq!(json["elicitationId"], "abc123");
        assert_eq!(json["message"], "Please authorize");
        assert_eq!(json["url"], "https://example.com/auth");
    }

    #[test]
    fn test_elicitation_complete_params() {
        let params = ElicitationCompleteParams {
            elicitation_id: "xyz789".to_string(),
            meta: None,
        };

        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json["elicitationId"], "xyz789");
    }

    #[test]
    fn test_root_new() {
        let root = Root::new("file:///home/user/project");
        assert_eq!(root.uri, "file:///home/user/project");
        assert!(root.name.is_none());
    }

    #[test]
    fn test_root_with_name() {
        let root = Root::with_name("file:///home/user/project", "My Project");
        assert_eq!(root.uri, "file:///home/user/project");
        assert_eq!(root.name.as_deref(), Some("My Project"));
    }

    #[test]
    fn test_root_serialization() {
        let root = Root::with_name("file:///workspace", "Workspace");
        let json = serde_json::to_value(&root).unwrap();
        assert_eq!(json["uri"], "file:///workspace");
        assert_eq!(json["name"], "Workspace");
    }

    #[test]
    fn test_root_serialization_without_name() {
        let root = Root::new("file:///workspace");
        let json = serde_json::to_value(&root).unwrap();
        assert_eq!(json["uri"], "file:///workspace");
        assert!(json.get("name").is_none());
    }

    #[test]
    fn test_root_deserialization() {
        let json = serde_json::json!({
            "uri": "file:///home/user",
            "name": "Home"
        });
        let root: Root = serde_json::from_value(json).unwrap();
        assert_eq!(root.uri, "file:///home/user");
        assert_eq!(root.name.as_deref(), Some("Home"));
    }

    #[test]
    fn test_list_roots_result() {
        let result = ListRootsResult {
            roots: vec![
                Root::new("file:///project1"),
                Root::with_name("file:///project2", "Project 2"),
            ],
            meta: None,
        };

        let json = serde_json::to_value(&result).unwrap();
        let roots = json["roots"].as_array().unwrap();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0]["uri"], "file:///project1");
        assert_eq!(roots[1]["name"], "Project 2");
    }

    #[test]
    fn test_roots_capability_serialization() {
        let cap = RootsCapability {
            list_changed: true,
            deprecated: None,
        };
        let json = serde_json::to_value(&cap).unwrap();
        assert_eq!(json["listChanged"], true);
    }

    #[test]
    fn test_client_capabilities_with_roots() {
        let caps = ClientCapabilities {
            roots: Some(RootsCapability {
                list_changed: true,
                deprecated: None,
            }),
            sampling: None,
            elicitation: None,
            tasks: None,
            experimental: None,
            extensions: None,
        };

        let json = serde_json::to_value(&caps).unwrap();
        assert_eq!(json["roots"]["listChanged"], true);
    }

    #[test]
    fn test_roots_list_changed_notification_parsing() {
        let notif = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: notifications::ROOTS_LIST_CHANGED.to_string(),
            params: None,
        };

        let mcp_notif = McpNotification::from_jsonrpc(&notif).unwrap();
        assert!(matches!(mcp_notif, McpNotification::RootsListChanged));
    }

    // =========================================================================
    // Completion Tests
    // =========================================================================

    #[test]
    fn test_prompt_reference() {
        let ref_ = PromptReference::new("my-prompt");
        assert_eq!(ref_.ref_type, "ref/prompt");
        assert_eq!(ref_.name, "my-prompt");

        let json = serde_json::to_value(&ref_).unwrap();
        assert_eq!(json["type"], "ref/prompt");
        assert_eq!(json["name"], "my-prompt");
    }

    #[test]
    fn test_resource_reference() {
        let ref_ = ResourceReference::new("file:///path/to/file");
        assert_eq!(ref_.ref_type, "ref/resource");
        assert_eq!(ref_.uri, "file:///path/to/file");

        let json = serde_json::to_value(&ref_).unwrap();
        assert_eq!(json["type"], "ref/resource");
        assert_eq!(json["uri"], "file:///path/to/file");
    }

    #[test]
    fn test_completion_reference_prompt() {
        let ref_ = CompletionReference::prompt("test-prompt");
        let json = serde_json::to_value(&ref_).unwrap();
        assert_eq!(json["type"], "ref/prompt");
        assert_eq!(json["name"], "test-prompt");
    }

    #[test]
    fn test_completion_reference_resource() {
        let ref_ = CompletionReference::resource("file:///test");
        let json = serde_json::to_value(&ref_).unwrap();
        assert_eq!(json["type"], "ref/resource");
        assert_eq!(json["uri"], "file:///test");
    }

    #[test]
    fn test_completion_argument() {
        let arg = CompletionArgument::new("query", "SELECT * FROM");
        assert_eq!(arg.name, "query");
        assert_eq!(arg.value, "SELECT * FROM");
    }

    #[test]
    fn test_complete_params_serialization() {
        let params = CompleteParams {
            reference: CompletionReference::prompt("sql-prompt"),
            argument: CompletionArgument::new("query", "SEL"),
            context: None,
            meta: None,
        };

        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json["ref"]["type"], "ref/prompt");
        assert_eq!(json["ref"]["name"], "sql-prompt");
        assert_eq!(json["argument"]["name"], "query");
        assert_eq!(json["argument"]["value"], "SEL");
        assert!(json.get("context").is_none()); // omitted when None
    }

    #[test]
    fn test_completion_new() {
        let completion = Completion::new(vec!["SELECT".to_string(), "SET".to_string()]);
        assert_eq!(completion.values.len(), 2);
        assert!(completion.total.is_none());
        assert!(completion.has_more.is_none());
    }

    #[test]
    fn test_completion_with_pagination() {
        let completion =
            Completion::with_pagination(vec!["a".to_string(), "b".to_string()], 100, true);
        assert_eq!(completion.values.len(), 2);
        assert_eq!(completion.total, Some(100));
        assert_eq!(completion.has_more, Some(true));
    }

    #[test]
    fn test_complete_result() {
        let result = CompleteResult::new(vec!["option1".to_string(), "option2".to_string()]);
        let json = serde_json::to_value(&result).unwrap();
        assert!(json["completion"]["values"].is_array());
        assert_eq!(json["completion"]["values"][0], "option1");
    }

    // =========================================================================
    // Sampling Tests
    // =========================================================================

    #[test]
    fn test_model_hint() {
        let hint = ModelHint::new("claude-3-opus");
        assert_eq!(hint.name, Some("claude-3-opus".to_string()));
    }

    #[test]
    fn test_model_preferences_builder() {
        let prefs = ModelPreferences::new()
            .speed(0.8)
            .intelligence(0.9)
            .cost(0.5)
            .hint("gpt-4")
            .hint("claude-3");

        assert_eq!(prefs.speed_priority, Some(0.8));
        assert_eq!(prefs.intelligence_priority, Some(0.9));
        assert_eq!(prefs.cost_priority, Some(0.5));
        assert_eq!(prefs.hints.len(), 2);
    }

    #[test]
    fn test_model_preferences_clamping() {
        let prefs = ModelPreferences::new().speed(1.5).cost(-0.5);

        assert_eq!(prefs.speed_priority, Some(1.0)); // Clamped to max
        assert_eq!(prefs.cost_priority, Some(0.0)); // Clamped to min
    }

    #[test]
    fn test_include_context_serialization() {
        assert_eq!(
            serde_json::to_string(&IncludeContext::AllServers).unwrap(),
            "\"allServers\""
        );
        assert_eq!(
            serde_json::to_string(&IncludeContext::ThisServer).unwrap(),
            "\"thisServer\""
        );
        assert_eq!(
            serde_json::to_string(&IncludeContext::None).unwrap(),
            "\"none\""
        );
    }

    #[test]
    fn test_sampling_message_user() {
        let msg = SamplingMessage::user("Hello, how are you?");
        assert_eq!(msg.role, ContentRole::User);
        assert!(
            matches!(msg.content, SamplingContentOrArray::Single(SamplingContent::Text { ref text, .. }) if text == "Hello, how are you?")
        );
    }

    #[test]
    fn test_sampling_message_assistant() {
        let msg = SamplingMessage::assistant("I'm doing well!");
        assert_eq!(msg.role, ContentRole::Assistant);
    }

    #[test]
    fn test_sampling_content_text_serialization() {
        let content = SamplingContent::Text {
            text: "Hello".to_string(),
            annotations: None,
            meta: None,
        };
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "Hello");
    }

    #[test]
    fn test_sampling_content_image_serialization() {
        let content = SamplingContent::Image {
            data: "base64data".to_string(),
            mime_type: "image/png".to_string(),
            annotations: None,
            meta: None,
        };
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["type"], "image");
        assert_eq!(json["data"], "base64data");
        assert_eq!(json["mimeType"], "image/png");
    }

    #[test]
    fn test_create_message_params() {
        let params = CreateMessageParams::new(
            vec![
                SamplingMessage::user("What is 2+2?"),
                SamplingMessage::assistant("4"),
                SamplingMessage::user("And 3+3?"),
            ],
            100,
        )
        .system_prompt("You are a math tutor")
        .temperature(0.7)
        .stop_sequence("END")
        .include_context(IncludeContext::ThisServer);

        assert_eq!(params.messages.len(), 3);
        assert_eq!(params.max_tokens, 100);
        assert_eq!(
            params.system_prompt.as_deref(),
            Some("You are a math tutor")
        );
        assert_eq!(params.temperature, Some(0.7));
        assert_eq!(params.stop_sequences.len(), 1);
        assert_eq!(params.include_context, Some(IncludeContext::ThisServer));
    }

    #[test]
    fn test_create_message_params_serialization() {
        let params = CreateMessageParams::new(vec![SamplingMessage::user("Hello")], 50);

        let json = serde_json::to_value(&params).unwrap();
        assert!(json["messages"].is_array());
        assert_eq!(json["maxTokens"], 50);
    }

    #[test]
    fn test_create_message_result_deserialization() {
        let json = serde_json::json!({
            "content": {
                "type": "text",
                "text": "The answer is 42"
            },
            "model": "claude-3-opus",
            "role": "assistant",
            "stopReason": "end_turn"
        });

        let result: CreateMessageResult = serde_json::from_value(json).unwrap();
        assert_eq!(result.model, "claude-3-opus");
        assert_eq!(result.role, ContentRole::Assistant);
        assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn test_completions_capability_serialization() {
        let cap = CompletionsCapability {};
        let json = serde_json::to_value(&cap).unwrap();
        assert!(json.is_object());
    }

    #[test]
    fn test_server_capabilities_with_completions() {
        let caps = ServerCapabilities {
            completions: Some(CompletionsCapability {}),
            ..Default::default()
        };

        let json = serde_json::to_value(&caps).unwrap();
        assert!(json["completions"].is_object());
    }

    #[test]
    fn test_content_resource_link_serialization() {
        let content = Content::ResourceLink {
            uri: "file:///test.txt".to_string(),
            name: "test.txt".to_string(),
            title: None,
            description: Some("A test file".to_string()),
            mime_type: Some("text/plain".to_string()),
            size: None,
            icons: None,
            annotations: None,
            meta: None,
        };
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["type"], "resource_link");
        assert_eq!(json["uri"], "file:///test.txt");
        assert_eq!(json["name"], "test.txt");
        assert_eq!(json["description"], "A test file");
        assert_eq!(json["mimeType"], "text/plain");
    }

    #[test]
    fn test_call_tool_result_resource_link() {
        let result = CallToolResult::resource_link("file:///output.json", "output.json");
        assert_eq!(result.content.len(), 1);
        assert!(!result.is_error);
        match &result.content[0] {
            Content::ResourceLink { uri, .. } => assert_eq!(uri, "file:///output.json"),
            _ => panic!("Expected ResourceLink content"),
        }
    }

    #[test]
    fn test_call_tool_result_image() {
        let result = CallToolResult::image("base64data", "image/png");
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            Content::Image {
                data, mime_type, ..
            } => {
                assert_eq!(data, "base64data");
                assert_eq!(mime_type, "image/png");
            }
            _ => panic!("Expected Image content"),
        }
    }

    #[test]
    fn test_call_tool_result_audio() {
        let result = CallToolResult::audio("audiodata", "audio/wav");
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            Content::Audio {
                data, mime_type, ..
            } => {
                assert_eq!(data, "audiodata");
                assert_eq!(mime_type, "audio/wav");
            }
            _ => panic!("Expected Audio content"),
        }
    }

    #[test]
    fn test_sampling_tool_serialization() {
        let tool = SamplingTool {
            name: "get_weather".to_string(),
            title: None,
            description: Some("Get current weather".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": { "type": "string" }
                }
            }),
            output_schema: None,
            icons: None,
            annotations: None,
            execution: None,
        };
        let json = serde_json::to_value(&tool).unwrap();
        assert_eq!(json["name"], "get_weather");
        assert_eq!(json["description"], "Get current weather");
        assert!(json["inputSchema"]["properties"]["location"].is_object());
    }

    #[test]
    fn test_tool_choice_modes() {
        let auto = ToolChoice::auto();
        assert_eq!(auto.mode, "auto");
        assert!(auto.name.is_none());

        let required = ToolChoice::required();
        assert_eq!(required.mode, "required");

        let none = ToolChoice::none();
        assert_eq!(none.mode, "none");

        let tool = ToolChoice::tool("get_weather");
        assert_eq!(tool.mode, "tool");
        assert_eq!(tool.name.as_deref(), Some("get_weather"));

        // Test serialization — mode field should serialize as "mode", not "type"
        let json = serde_json::to_value(&auto).unwrap();
        assert_eq!(json["mode"], "auto");
        assert!(json.get("name").is_none());

        let json = serde_json::to_value(&tool).unwrap();
        assert_eq!(json["mode"], "tool");
        assert_eq!(json["name"], "get_weather");
    }

    #[test]
    fn test_sampling_content_tool_use() {
        let content = SamplingContent::ToolUse {
            id: "tool_123".to_string(),
            name: "get_weather".to_string(),
            input: serde_json::json!({"location": "San Francisco"}),
            meta: None,
        };
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["type"], "tool_use");
        assert_eq!(json["id"], "tool_123");
        assert_eq!(json["name"], "get_weather");
        assert_eq!(json["input"]["location"], "San Francisco");
    }

    #[test]
    fn test_sampling_content_tool_result() {
        let content = SamplingContent::ToolResult {
            tool_use_id: "tool_123".to_string(),
            content: vec![SamplingContent::Text {
                text: "72F, sunny".to_string(),
                annotations: None,
                meta: None,
            }],
            structured_content: None,
            is_error: None,
            meta: None,
        };
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["type"], "tool_result");
        assert_eq!(json["toolUseId"], "tool_123");
        assert_eq!(json["content"][0]["type"], "text");
    }

    #[test]
    fn test_sampling_content_or_array_single() {
        let json = serde_json::json!({
            "type": "text",
            "text": "Hello"
        });
        let content: SamplingContentOrArray = serde_json::from_value(json).unwrap();
        let items = content.items();
        assert_eq!(items.len(), 1);
        match items[0] {
            SamplingContent::Text { text, .. } => assert_eq!(text, "Hello"),
            _ => panic!("Expected text content"),
        }
    }

    #[test]
    fn test_sampling_content_or_array_multiple() {
        let json = serde_json::json!([
            { "type": "text", "text": "Hello" },
            { "type": "text", "text": "World" }
        ]);
        let content: SamplingContentOrArray = serde_json::from_value(json).unwrap();
        let items = content.items();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn test_create_message_params_with_tools() {
        let tool = SamplingTool {
            name: "calculator".to_string(),
            title: None,
            description: Some("Do math".to_string()),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icons: None,
            annotations: None,
            execution: None,
        };
        let params = CreateMessageParams::new(vec![], 100)
            .tools(vec![tool])
            .tool_choice(ToolChoice::auto());

        let json = serde_json::to_value(&params).unwrap();
        assert!(json["tools"].is_array());
        assert_eq!(json["tools"][0]["name"], "calculator");
        assert_eq!(json["toolChoice"]["mode"], "auto");
    }

    #[test]
    fn test_create_message_result_content_items() {
        let result = CreateMessageResult {
            content: SamplingContentOrArray::Array(vec![
                SamplingContent::Text {
                    text: "First".to_string(),
                    annotations: None,
                    meta: None,
                },
                SamplingContent::Text {
                    text: "Second".to_string(),
                    annotations: None,
                    meta: None,
                },
            ]),
            model: "test".to_string(),
            role: ContentRole::Assistant,
            stop_reason: None,
            meta: None,
        };
        let items = result.content_items();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn test_sampling_content_as_text() {
        let text_content = SamplingContent::Text {
            text: "Hello".to_string(),
            annotations: None,
            meta: None,
        };
        assert_eq!(text_content.as_text(), Some("Hello"));

        let image_content = SamplingContent::Image {
            data: "base64data".to_string(),
            mime_type: "image/png".to_string(),
            annotations: None,
            meta: None,
        };
        assert_eq!(image_content.as_text(), None);

        let audio_content = SamplingContent::Audio {
            data: "base64audio".to_string(),
            mime_type: "audio/wav".to_string(),
            annotations: None,
            meta: None,
        };
        assert_eq!(audio_content.as_text(), None);
    }

    #[test]
    fn test_create_message_result_first_text_single() {
        let result = CreateMessageResult {
            content: SamplingContentOrArray::Single(SamplingContent::Text {
                text: "Hello, world!".to_string(),
                annotations: None,
                meta: None,
            }),
            model: "test".to_string(),
            role: ContentRole::Assistant,
            stop_reason: None,
            meta: None,
        };
        assert_eq!(result.first_text(), Some("Hello, world!"));
    }

    #[test]
    fn test_create_message_result_first_text_array() {
        let result = CreateMessageResult {
            content: SamplingContentOrArray::Array(vec![
                SamplingContent::Text {
                    text: "First".to_string(),
                    annotations: None,
                    meta: None,
                },
                SamplingContent::Text {
                    text: "Second".to_string(),
                    annotations: None,
                    meta: None,
                },
            ]),
            model: "test".to_string(),
            role: ContentRole::Assistant,
            stop_reason: None,
            meta: None,
        };
        assert_eq!(result.first_text(), Some("First"));
    }

    #[test]
    fn test_create_message_result_first_text_skips_non_text() {
        let result = CreateMessageResult {
            content: SamplingContentOrArray::Array(vec![
                SamplingContent::Image {
                    data: "base64data".to_string(),
                    mime_type: "image/png".to_string(),
                    annotations: None,
                    meta: None,
                },
                SamplingContent::Text {
                    text: "After image".to_string(),
                    annotations: None,
                    meta: None,
                },
            ]),
            model: "test".to_string(),
            role: ContentRole::Assistant,
            stop_reason: None,
            meta: None,
        };
        assert_eq!(result.first_text(), Some("After image"));
    }

    #[test]
    fn test_create_message_result_first_text_none() {
        let result = CreateMessageResult {
            content: SamplingContentOrArray::Single(SamplingContent::Image {
                data: "base64data".to_string(),
                mime_type: "image/png".to_string(),
                annotations: None,
                meta: None,
            }),
            model: "test".to_string(),
            role: ContentRole::Assistant,
            stop_reason: None,
            meta: None,
        };
        assert_eq!(result.first_text(), None);
    }

    #[test]
    fn test_tool_annotations_accessors() {
        let annotations = ToolAnnotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: false,
            ..Default::default()
        };

        assert!(annotations.is_read_only());
        assert!(!annotations.is_destructive());
        assert!(annotations.is_idempotent());
        assert!(!annotations.is_open_world());
    }

    #[test]
    fn test_tool_annotations_defaults() {
        // Default matches MCP spec defaults: destructive=true, open_world=true
        let annotations = ToolAnnotations::default();

        assert!(!annotations.is_read_only());
        assert!(annotations.is_destructive());
        assert!(!annotations.is_idempotent());
        assert!(annotations.is_open_world());
    }

    #[test]
    fn test_tool_annotations_serde_defaults() {
        // When deserialized from an empty object, serde applies
        // the spec defaults: destructive_hint=true, open_world_hint=true
        let annotations: ToolAnnotations = serde_json::from_str("{}").unwrap();

        assert!(!annotations.is_read_only());
        assert!(annotations.is_destructive());
        assert!(!annotations.is_idempotent());
        assert!(annotations.is_open_world());
    }

    #[test]
    fn test_tool_definition_accessors_with_annotations() {
        let def = ToolDefinition {
            name: "test".to_string(),
            title: None,
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icons: None,
            annotations: Some(ToolAnnotations {
                read_only_hint: true,
                idempotent_hint: true,
                destructive_hint: false,
                open_world_hint: false,
                ..Default::default()
            }),
            execution: None,
            meta: None,
        };

        assert!(def.is_read_only());
        assert!(!def.is_destructive());
        assert!(def.is_idempotent());
        assert!(!def.is_open_world());
    }

    #[test]
    fn test_tool_definition_accessors_without_annotations() {
        let def = ToolDefinition {
            name: "test".to_string(),
            title: None,
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icons: None,
            annotations: None,
            execution: None,
            meta: None,
        };

        // MCP spec defaults when no annotations present
        assert!(!def.is_read_only());
        assert!(def.is_destructive());
        assert!(!def.is_idempotent());
        assert!(def.is_open_world());
    }

    #[test]
    fn test_call_tool_result_from_list() {
        #[derive(serde::Serialize)]
        struct Item {
            name: String,
        }

        let items = vec![
            Item {
                name: "a".to_string(),
            },
            Item {
                name: "b".to_string(),
            },
            Item {
                name: "c".to_string(),
            },
        ];

        let result = CallToolResult::from_list("items", &items).unwrap();
        assert!(!result.is_error);

        let structured = result.structured_content.unwrap();
        assert_eq!(structured["count"], 3);
        assert_eq!(structured["items"].as_array().unwrap().len(), 3);
        assert_eq!(structured["items"][0]["name"], "a");
    }

    #[test]
    fn test_call_tool_result_from_list_empty() {
        let items: Vec<String> = vec![];
        let result = CallToolResult::from_list("results", &items).unwrap();
        assert!(!result.is_error);

        let structured = result.structured_content.unwrap();
        assert_eq!(structured["count"], 0);
        assert_eq!(structured["results"].as_array().unwrap().len(), 0);
    }

    // =========================================================================
    // JSON Helper Tests
    // =========================================================================

    #[test]
    fn test_call_tool_result_as_json() {
        let result = CallToolResult::json(serde_json::json!({"key": "value"}));
        let value = result.as_json().unwrap().unwrap();
        assert_eq!(value["key"], "value");
    }

    #[test]
    fn test_call_tool_result_as_json_from_text() {
        let result = CallToolResult::text(r#"{"key": "value"}"#);
        let value = result.as_json().unwrap().unwrap();
        assert_eq!(value["key"], "value");
    }

    #[test]
    fn test_call_tool_result_as_json_none() {
        let result = CallToolResult::text("not json");
        let parsed = result.as_json().unwrap();
        assert!(parsed.is_err());
    }

    #[test]
    fn test_call_tool_result_deserialize() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct Output {
            key: String,
        }

        let result = CallToolResult::json(serde_json::json!({"key": "value"}));
        let output: Output = result.deserialize().unwrap().unwrap();
        assert_eq!(output.key, "value");
    }

    #[test]
    fn test_call_tool_result_as_json_empty() {
        let result = CallToolResult {
            content: vec![],
            is_error: false,
            structured_content: None,
            meta: None,
        };
        assert!(result.as_json().is_none());
    }

    #[test]
    fn test_call_tool_result_deserialize_from_text() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct Output {
            key: String,
        }

        let result = CallToolResult::text(r#"{"key": "from_text"}"#);
        let output: Output = result.deserialize().unwrap().unwrap();
        assert_eq!(output.key, "from_text");
    }

    #[test]
    fn test_read_resource_result_as_json() {
        let result = ReadResourceResult::json("data://config", &serde_json::json!({"port": 8080}));
        let value = result.as_json().unwrap().unwrap();
        assert_eq!(value["port"], 8080);
    }

    #[test]
    fn test_read_resource_result_deserialize() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct Config {
            port: u16,
        }

        let result = ReadResourceResult::json("data://config", &serde_json::json!({"port": 8080}));
        let config: Config = result.deserialize().unwrap().unwrap();
        assert_eq!(config.port, 8080);
    }

    #[test]
    fn test_get_prompt_result_as_json() {
        let result = GetPromptResult::user_message(r#"{"action": "analyze"}"#);
        let value = result.as_json().unwrap().unwrap();
        assert_eq!(value["action"], "analyze");
    }

    #[test]
    fn test_get_prompt_result_deserialize() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct Params {
            action: String,
        }

        let result = GetPromptResult::user_message(r#"{"action": "analyze"}"#);
        let params: Params = result.deserialize().unwrap().unwrap();
        assert_eq!(params.action, "analyze");
    }

    #[test]
    fn test_get_prompt_result_as_json_empty() {
        let result = GetPromptResult {
            description: None,
            messages: vec![],
            meta: None,
        };
        assert!(result.as_json().is_none());
    }

    #[test]
    fn test_mcp_response_serde_roundtrip() {
        // CallToolResult variant - has distinctive fields
        let response = McpResponse::CallTool(CallToolResult {
            content: vec![Content::text("hello")],
            structured_content: None,
            is_error: false,
            meta: None,
        });
        let json = serde_json::to_string(&response).unwrap();
        let deserialized: McpResponse = serde_json::from_str(&json).unwrap();
        match deserialized {
            McpResponse::CallTool(result) => {
                assert_eq!(result.content[0].as_text(), Some("hello"));
            }
            _ => panic!("expected CallTool variant"),
        }

        // ListToolsResult variant
        let response = McpResponse::ListTools(ListToolsResult {
            tools: vec![],
            next_cursor: Some("cursor123".to_string()),
            ttl_ms: None,
            cache_scope: None,
            meta: None,
        });
        let json = serde_json::to_string(&response).unwrap();
        let deserialized: McpResponse = serde_json::from_str(&json).unwrap();
        match deserialized {
            McpResponse::ListTools(result) => {
                assert_eq!(result.next_cursor.as_deref(), Some("cursor123"));
            }
            _ => panic!("expected ListTools variant"),
        }
    }

    // =========================================================================
    // SEP-2663 tasks extension wire-format tests
    // =========================================================================

    fn task_obj_for_tests() -> TaskObject {
        TaskObject {
            task_id: "task-786512e2".into(),
            status: TaskStatus::Working,
            status_message: None,
            created_at: "2025-11-25T10:30:00Z".into(),
            last_updated_at: "2025-11-25T10:30:00Z".into(),
            ttl: Some(60_000),
            poll_interval: Some(5_000),
            result: None,
            error: None,
            meta: None,
        }
    }

    #[test]
    fn tasks_extension_id_matches_sep_2663() {
        // The reverse-DNS identifier MUST match the value defined in the SEP
        // (used as the key in ClientCapabilities.extensions / ServerCapabilities.extensions).
        assert_eq!(TASKS_EXTENSION_ID, "io.modelcontextprotocol/tasks");
    }

    #[test]
    fn create_task_result_serializes_with_sep_2663_discriminator() {
        // Per SEP-2663:
        //   - resultType MUST be "task"
        //   - the Task fields are inlined at the top of the result object
        let result = CreateTaskResult::new(task_obj_for_tests());
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["resultType"], serde_json::json!("task"));
        // SEP-2663 inlined Task fields
        assert_eq!(json["taskId"], serde_json::json!("task-786512e2"));
        assert_eq!(json["status"], serde_json::json!("working"));
        assert_eq!(json["pollInterval"], serde_json::json!(5_000));
        assert_eq!(json["ttl"], serde_json::json!(60_000));
        assert_eq!(json["createdAt"], serde_json::json!("2025-11-25T10:30:00Z"));
        // Back-compat: the nested `task` mirror is still emitted for
        // 2025-11-25 clients of this crate. New clients should ignore it.
        assert_eq!(json["task"]["taskId"], serde_json::json!("task-786512e2"));
    }

    #[test]
    fn create_task_result_round_trips_via_flat_layout() {
        let original = CreateTaskResult::new(task_obj_for_tests());
        let json = serde_json::to_string(&original).unwrap();
        let parsed: CreateTaskResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.task.task_id, "task-786512e2");
        assert_eq!(parsed.task.status, TaskStatus::Working);
        assert_eq!(parsed.task.ttl, Some(60_000));
    }

    #[test]
    fn create_task_result_accepts_legacy_nested_layout() {
        // 2025-11-25 wire shape (no resultType, task is nested only).
        let legacy = serde_json::json!({
            "task": {
                "taskId": "legacy-id",
                "status": "working",
                "createdAt": "2025-11-25T10:30:00Z",
                "lastUpdatedAt": "2025-11-25T10:30:00Z",
                "ttl": null
            }
        });
        let parsed: CreateTaskResult = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed.task.task_id, "legacy-id");
        assert_eq!(parsed.task.status, TaskStatus::Working);
    }

    #[test]
    fn tasks_update_method_parses_via_from_jsonrpc() {
        // SEP-2663 introduces tasks/update with { taskId, inputResponses }.
        let body = r#"{
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tasks/update",
            "params": {
                "taskId": "abc-123",
                "inputResponses": {
                    "name": {
                        "action": "accept",
                        "content": { "input": "Luca" }
                    }
                }
            }
        }"#;
        let req: JsonRpcRequest = serde_json::from_str(body).unwrap();
        let parsed = McpRequest::from_jsonrpc(&req).unwrap();
        match parsed {
            McpRequest::UpdateTask(p) => {
                assert_eq!(p.task_id, "abc-123");
                let name_resp = p.input_responses.get("name").expect("has 'name' key");
                assert_eq!(name_resp["action"], "accept");
            }
            other => panic!("expected UpdateTask, got {:?}", other.method_name()),
        }
        // Verify reverse mapping for `method_name()`.
        let parsed = McpRequest::from_jsonrpc(&req).unwrap();
        assert_eq!(parsed.method_name(), "tasks/update");
    }

    #[test]
    fn final_tasks_methods_parse() {
        // The SEP-2663 (final) method set: tasks/get, tasks/update,
        // tasks/cancel. tasks/list and tasks/result are removed.
        for (method, params, expected_name) in [
            (
                "tasks/get",
                serde_json::json!({ "taskId": "x" }),
                "tasks/get",
            ),
            (
                "tasks/cancel",
                serde_json::json!({ "taskId": "x" }),
                "tasks/cancel",
            ),
        ] {
            let req = JsonRpcRequest::new(1, method).with_params(params);
            let parsed = McpRequest::from_jsonrpc(&req)
                .unwrap_or_else(|e| panic!("failed to parse {method}: {e:?}"));
            assert_eq!(parsed.method_name(), expected_name);
        }
    }

    #[test]
    fn removed_tasks_methods_fall_through_to_unknown() {
        // Final SEP-2663 removes tasks/list and tasks/result; they must not
        // parse into typed requests. Falling through to `Unknown` makes the
        // router answer MethodNotFound (-32601), which is the spec-required
        // behavior for removed methods.
        for (method, params) in [
            ("tasks/list", serde_json::json!({})),
            ("tasks/result", serde_json::json!({ "taskId": "x" })),
        ] {
            let req = JsonRpcRequest::new(1, method).with_params(params);
            let parsed = McpRequest::from_jsonrpc(&req)
                .unwrap_or_else(|e| panic!("failed to parse {method}: {e:?}"));
            assert!(
                matches!(parsed, McpRequest::Unknown { .. }),
                "{method} must fall through to Unknown, got {}",
                parsed.method_name()
            );
        }
    }

    #[test]
    fn result_type_task_discriminator_constant() {
        // SEP-2322/SEP-2663 reserve the "task" discriminator value.
        assert_eq!(RESULT_TYPE_TASK, "task");
        assert_eq!(CreateTaskResult::RESULT_TYPE, RESULT_TYPE_TASK);
    }

    #[test]
    fn server_capabilities_extensions_keys_match_tasks_extension_id() {
        // Per SEP-2663, support for the tasks extension is declared by
        // inserting the extension identifier into ServerCapabilities.extensions
        // (an empty object value indicates "supported with no settings").
        let mut extensions = HashMap::new();
        extensions.insert(TASKS_EXTENSION_ID.to_string(), serde_json::json!({}));
        let caps = ServerCapabilities {
            extensions: Some(extensions),
            ..Default::default()
        };
        let json = serde_json::to_value(&caps).unwrap();
        assert_eq!(
            json["extensions"]["io.modelcontextprotocol/tasks"],
            serde_json::json!({})
        );
    }
}

#[cfg(test)]
mod draft_2026_07_28_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn result_type_wire_forms_and_backcompat() {
        assert_eq!(
            serde_json::to_value(ResultType::Complete).unwrap(),
            json!("complete")
        );
        assert_eq!(
            serde_json::to_value(ResultType::InputRequired).unwrap(),
            json!("input_required")
        );
        assert_eq!(
            serde_json::to_value(ResultType::Other("task".into())).unwrap(),
            json!("task")
        );
        assert_eq!(
            serde_json::from_value::<ResultType>(json!("input_required")).unwrap(),
            ResultType::InputRequired
        );
        assert_eq!(
            serde_json::from_value::<ResultType>(json!("task")).unwrap(),
            ResultType::Other("task".into())
        );
        // SEP-2322 backward-compat rule: an absent resultType reads as complete.
        assert_eq!(
            ResultType::from_result_value(&json!({"content": []})),
            ResultType::Complete
        );
        assert_eq!(
            ResultType::from_result_value(&json!({"resultType": "input_required"})),
            ResultType::InputRequired
        );
        assert!(ResultType::default().is_complete());
    }

    #[test]
    fn input_required_result_round_trips() {
        let requests: InputRequests =
            serde_json::from_value(json!({ "r1": {"method": "roots/list", "params": {}} }))
                .unwrap();
        let result = InputRequiredResult {
            input_requests: Some(requests),
            request_state: Some("opaque-blob".to_string()),
            ..InputRequiredResult::new()
        };
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["resultType"], json!("input_required"));
        assert_eq!(v["inputRequests"]["r1"]["method"], json!("roots/list"));
        assert_eq!(v["requestState"], json!("opaque-blob"));
        let back: InputRequiredResult = serde_json::from_value(v).unwrap();
        assert_eq!(back.result_type, ResultType::InputRequired);
        assert_eq!(back.request_state.as_deref(), Some("opaque-blob"));
    }

    #[test]
    fn input_required_builders_validate_and_request_outcome_preserves_variant() {
        let requests: InputRequests = [(
            "roots".to_string(),
            InputRequest::ListRoots(ListRootsParams::default()),
        )]
        .into_iter()
        .collect();
        let result =
            InputRequiredResult::with_requests(requests).with_request_state("opaque-state");
        assert!(result.validate().is_ok());

        let outcome: RequestOutcome<CallToolResult> = RequestOutcome::input_required(result);
        assert!(!outcome.is_complete());
        assert_eq!(
            outcome
                .as_input_required()
                .and_then(|result| result.request_state.as_deref()),
            Some("opaque-state")
        );

        assert!(InputRequiredResult::new().validate().is_err());
        let complete: RequestOutcome<CallToolResult> = CallToolResult::text("done").into();
        assert!(complete.is_complete());
        assert!(complete.as_complete().is_some());
    }

    #[test]
    fn input_request_is_adjacently_tagged_and_response_untagged() {
        let ir: InputRequest =
            serde_json::from_value(json!({"method": "roots/list", "params": {}})).unwrap();
        assert_eq!(ir.method_name(), "roots/list");
        assert!(matches!(ir, InputRequest::ListRoots(_)));
        assert_eq!(
            serde_json::to_value(&ir).unwrap(),
            json!({"method": "roots/list", "params": {}})
        );
        // Untagged response value, correlated to its request by map key.
        let resp: InputResponse = serde_json::from_value(json!({"roots": []})).unwrap();
        assert!(matches!(resp, InputResponse::ListRoots(_)));
    }

    #[test]
    fn request_meta_carries_sep2575_keys_and_validates_by_version() {
        let meta = RequestMeta {
            protocol_version: Some(PROTOCOL_VERSION_2026_07_28.to_string()),
            client_capabilities: Some(ClientCapabilities::default()),
            ..Default::default()
        };
        let v = serde_json::to_value(&meta).unwrap();
        assert_eq!(
            v["io.modelcontextprotocol/protocolVersion"],
            json!(PROTOCOL_VERSION_2026_07_28)
        );
        assert!(
            v.get("io.modelcontextprotocol/clientCapabilities")
                .is_some()
        );
        // Version-aware required-key validation (SEP-2575).
        assert!(
            meta.validate_for_version(PROTOCOL_VERSION_2026_07_28)
                .is_ok()
        );
        assert!(
            RequestMeta::default()
                .validate_for_version(PROTOCOL_VERSION_2026_07_28)
                .is_err()
        );
        assert!(
            RequestMeta::default()
                .validate_for_version("2025-11-25")
                .is_ok()
        );
    }

    #[test]
    fn subscription_types_round_trip() {
        let filter = SubscriptionFilter {
            tools_list_changed: Some(true),
            resource_subscriptions: Some(vec!["file:///a".to_string()]),
            ..Default::default()
        };
        let v = serde_json::to_value(&filter).unwrap();
        assert_eq!(v["toolsListChanged"], json!(true));
        assert_eq!(v["resourceSubscriptions"], json!(["file:///a"]));
        assert!(v.get("promptsListChanged").is_none()); // None is omitted

        let ack = SubscriptionsAcknowledgedParams {
            meta: None,
            notifications: filter,
        };
        let back: SubscriptionsAcknowledgedParams =
            serde_json::from_value(serde_json::to_value(&ack).unwrap()).unwrap();
        assert_eq!(back.notifications.tools_list_changed, Some(true));

        let result = SubscriptionsListenResult {
            result_type: ResultType::Complete,
            meta: SubscriptionsListenResultMeta {
                subscription_id: RequestId::Number(7),
                server_info: Some(Implementation {
                    name: "test-server".to_string(),
                    version: "1.0.0".to_string(),
                    title: None,
                    description: None,
                    icons: None,
                    website_url: None,
                    meta: None,
                }),
            },
        };
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["resultType"], json!("complete"));
        assert_eq!(
            value["_meta"]["io.modelcontextprotocol/subscriptionId"],
            json!(7)
        );
        assert_eq!(
            value["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            json!("test-server")
        );
        let back: SubscriptionsListenResult = serde_json::from_value(value).unwrap();
        assert!(back.result_type.is_complete());
        assert_eq!(back.meta.subscription_id, RequestId::Number(7));
        assert_eq!(
            back.meta.server_info.map(|info| info.name),
            Some("test-server".to_string())
        );
        assert!(
            serde_json::from_value::<SubscriptionsListenResult>(serde_json::json!({
                "resultType": "complete"
            }))
            .is_err(),
            "final subscription results require _meta and a subscription ID"
        );
    }

    #[test]
    fn additions_keep_2025_11_25_output_byte_identical() {
        // A tools/call with no MRTR/task/meta serializes exactly as before:
        // only name + arguments, no new keys.
        let params = CallToolParams {
            name: "echo".to_string(),
            arguments: json!({"message": "hi"}),
            input_responses: None,
            request_state: None,
            meta: None,
            task: None,
        };
        assert_eq!(
            serde_json::to_value(&params).unwrap(),
            json!({"name": "echo", "arguments": {"message": "hi"}})
        );
        // RequestMeta with only a progress token adds no SEP-2575 keys.
        let meta = RequestMeta {
            progress_token: Some(ProgressToken::String("p".into())),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&meta).unwrap(),
            json!({"progressToken": "p"})
        );
    }

    #[test]
    fn meta_survives_on_a_request_with_no_other_params() {
        // Guard against the rmcp _meta-drop bug (rust-sdk#993): a params struct
        // whose only populated field is `_meta` must round-trip `_meta` intact.
        let params = ListToolsParams {
            cursor: None,
            meta: Some(RequestMeta {
                protocol_version: Some(PROTOCOL_VERSION_2026_07_28.to_string()),
                ..Default::default()
            }),
        };
        let v = serde_json::to_value(&params).unwrap();
        assert_eq!(
            v["_meta"]["io.modelcontextprotocol/protocolVersion"],
            json!(PROTOCOL_VERSION_2026_07_28)
        );
        let back: ListToolsParams = serde_json::from_value(v).unwrap();
        assert_eq!(
            back.meta.unwrap().protocol_version.as_deref(),
            Some(PROTOCOL_VERSION_2026_07_28)
        );
    }

    #[test]
    fn result_type_stamp_is_gated_on_the_negotiated_version() {
        let result = CallToolResult::text("hi");
        let baseline = serde_json::to_value(&result).unwrap();

        // 2025-11-25 output is untouched: no resultType, byte-identical.
        let mut v = baseline.clone();
        assert!(!ResultType::Complete.stamp_into(&mut v, "2025-11-25"));
        assert_eq!(v, baseline);

        // 2026-07-28 carries the discriminator.
        let mut v = baseline.clone();
        assert!(ResultType::Complete.stamp_into(&mut v, PROTOCOL_VERSION_2026_07_28));
        assert_eq!(v["resultType"], json!("complete"));
        assert_eq!(v["content"], baseline["content"]);

        // Unknown future versions do not silently inherit 2026 semantics.
        let mut v = baseline.clone();
        assert!(!ResultType::Complete.stamp_into(&mut v, "2027-01-01"));
        assert_eq!(v, baseline);

        assert!(!version_carries_result_type("2025-11-25"));
        assert!(version_carries_result_type(PROTOCOL_VERSION_2026_07_28));
        assert!(!version_carries_result_type("2027-01-01"));
    }

    #[test]
    fn result_type_stamp_leaves_owned_discriminators_alone() {
        // InputRequiredResult and CreateTaskResult write their own resultType;
        // stamping must not overwrite it with "complete".
        let mut v = serde_json::to_value(InputRequiredResult::new()).unwrap();
        assert!(!ResultType::Complete.stamp_into(&mut v, PROTOCOL_VERSION_2026_07_28));
        assert_eq!(v["resultType"], json!("input_required"));

        let mut v = json!({"resultType": RESULT_TYPE_TASK, "taskId": "t1"});
        assert!(!ResultType::Complete.stamp_into(&mut v, PROTOCOL_VERSION_2026_07_28));
        assert_eq!(v["resultType"], json!("task"));

        // A non-object result (the empty result some methods return) is a no-op.
        let mut v = json!(null);
        assert!(!ResultType::Complete.stamp_into(&mut v, PROTOCOL_VERSION_2026_07_28));
        assert_eq!(v, json!(null));
    }

    #[test]
    fn stamped_result_round_trips_and_reads_back_as_complete() {
        // Draft-schema shape of a complete tools/call result: the discriminator
        // rides alongside the 2025-11-25 body and is ignored by readers that
        // deserialize into the concrete result type.
        let example = json!({
            "resultType": "complete",
            "content": [{"type": "text", "text": "42"}],
            "isError": false
        });
        assert_eq!(
            ResultType::from_result_value(&example),
            ResultType::Complete
        );
        let parsed: CallToolResult = serde_json::from_value(example).unwrap();
        let mut round_tripped = serde_json::to_value(&parsed).unwrap();
        assert!(ResultType::Complete.stamp_into(&mut round_tripped, PROTOCOL_VERSION_2026_07_28));
        assert_eq!(round_tripped["resultType"], json!("complete"));
        assert_eq!(round_tripped["content"][0]["text"], json!("42"));

        // An input_required result reads back through the same entry point.
        let example = json!({
            "resultType": "input_required",
            "inputRequests": {"r1": {"method": "elicitation/create", "params": {
                "message": "which file?",
                "requestedSchema": {"type": "object", "properties": {}}
            }}}
        });
        assert_eq!(
            ResultType::from_result_value(&example),
            ResultType::InputRequired
        );
        let parsed: InputRequiredResult = serde_json::from_value(example).unwrap();
        assert_eq!(parsed.result_type, ResultType::InputRequired);
        assert!(parsed.input_requests.is_some_and(|r| r.contains_key("r1")));
    }
}

#[cfg(test)]
mod elicitation_field_order_tests {
    use super::*;

    /// Raw wire bytes, not a `json!` value: the macro builds a map that may
    /// itself reorder keys, which would pre-sort the input and make these
    /// tests pass for the wrong reason.
    const DECLARED: &str = r#"{"type":"object","properties":{"firstName":{"type":"string"},"lastName":{"type":"string"},"email":{"type":"string"},"age":{"type":"integer"}},"required":["firstName"]}"#;

    fn parse_declared() -> ElicitFormSchema {
        serde_json::from_str(DECLARED).expect("declared schema parses")
    }

    /// #1199: elicitation schemas are the one place in MCP where a JSON
    /// object's key order carries presentation meaning, since it is the order
    /// a client renders the form in. A hash map made that order arbitrary and,
    /// because `HashMap` seeds per process, unstable between runs of the same
    /// server.
    #[test]
    fn declared_field_order_survives_a_round_trip() {
        let parsed = parse_declared();

        let order: Vec<&str> = parsed.properties.keys().map(String::as_str).collect();
        assert_eq!(order, ["firstName", "lastName", "email", "age"]);

        // Direct serialization, and the `Value` hop the JSON-RPC layer takes
        // on every response. The latter needs serde_json's `preserve_order`;
        // without it the ordered map is re-sorted on the way out.
        let direct: ElicitFormSchema =
            serde_json::from_str(&serde_json::to_string(&parsed).expect("serialize"))
                .expect("reparse");
        assert_eq!(
            direct
                .properties
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["firstName", "lastName", "email", "age"],
        );

        let round = serde_json::to_value(&parsed).unwrap();
        let wire: Vec<&str> = round["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            wire,
            ["firstName", "lastName", "email", "age"],
            "the order a server declared must reach the wire unchanged"
        );
    }

    /// The order must also be stable across repeated decodes, which a
    /// per-process-seeded hash map could not guarantee.
    #[test]
    fn field_order_is_deterministic_across_decodes() {
        let first: Vec<String> = parse_declared().properties.keys().cloned().collect();
        for _ in 0..16 {
            let again: Vec<String> = parse_declared().properties.keys().cloned().collect();
            assert_eq!(first, again);
        }
    }

    /// The builder's declaration order is the order that ships.
    #[test]
    fn builder_insertion_order_is_preserved() {
        let schema = ElicitFormSchema::new()
            .string_field("zulu", None, true)
            .string_field("alpha", None, false)
            .string_field("mike", None, false);

        let order: Vec<&str> = schema.properties.keys().map(String::as_str).collect();
        assert_eq!(
            order,
            ["zulu", "alpha", "mike"],
            "builder order must not be re-sorted"
        );
    }

    /// Field types still dispatch correctly through the ordered map (#1189).
    #[test]
    fn ordering_does_not_disturb_variant_dispatch() {
        let parsed = parse_declared();
        assert!(matches!(
            parsed.properties.get("age"),
            Some(PrimitiveSchemaDefinition::Integer(_))
        ));
        assert!(matches!(
            parsed.properties.get("firstName"),
            Some(PrimitiveSchemaDefinition::String(_))
        ));
    }
}

#[cfg(test)]
mod primitive_schema_dispatch_tests {
    use super::*;

    fn variant(json: serde_json::Value) -> &'static str {
        match serde_json::from_value::<PrimitiveSchemaDefinition>(json).unwrap() {
            PrimitiveSchemaDefinition::String(_) => "String",
            PrimitiveSchemaDefinition::Integer(_) => "Integer",
            PrimitiveSchemaDefinition::Number(_) => "Number",
            PrimitiveSchemaDefinition::Boolean(_) => "Boolean",
            PrimitiveSchemaDefinition::SingleSelectEnum(_) => "SingleSelectEnum",
            PrimitiveSchemaDefinition::MultiSelectEnum(_) => "MultiSelectEnum",
            PrimitiveSchemaDefinition::Raw(_) => "Raw",
        }
    }

    /// #1189: the untagged union matched `String` for every field, so a
    /// client that matched on the variant coerced every elicitation field to
    /// text.
    #[test]
    fn each_declared_type_parses_to_its_own_variant() {
        assert_eq!(variant(serde_json::json!({"type": "string"})), "String");
        assert_eq!(variant(serde_json::json!({"type": "integer"})), "Integer");
        assert_eq!(variant(serde_json::json!({"type": "number"})), "Number");
        assert_eq!(variant(serde_json::json!({"type": "boolean"})), "Boolean");
        assert_eq!(
            variant(serde_json::json!({"type": "string", "enum": ["a", "b"]})),
            "SingleSelectEnum"
        );
        assert_eq!(
            variant(serde_json::json!({
                "type": "array",
                "items": {"type": "string", "enum": ["a", "b"]}
            })),
            "MultiSelectEnum"
        );
    }

    /// The values were not merely mislabelled: `StringSchema` has nowhere to
    /// keep them, so they were destroyed before a client ever saw them.
    #[test]
    fn single_select_choices_survive_a_round_trip() {
        let json = serde_json::json!({"type": "string", "enum": ["a", "b"]});
        let parsed: PrimitiveSchemaDefinition = serde_json::from_value(json.clone()).unwrap();
        match &parsed {
            PrimitiveSchemaDefinition::SingleSelectEnum(schema) => {
                assert_eq!(schema.enum_values, vec!["a".to_string(), "b".to_string()]);
            }
            other => panic!("expected SingleSelectEnum, got {other:?}"),
        }
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
    }

    #[test]
    fn multi_select_choices_survive_a_round_trip() {
        let json = serde_json::json!({
            "type": "array",
            "items": {"type": "string", "enum": ["x", "y"]}
        });
        let parsed: PrimitiveSchemaDefinition = serde_json::from_value(json.clone()).unwrap();
        match &parsed {
            PrimitiveSchemaDefinition::MultiSelectEnum(schema) => {
                assert_eq!(
                    schema.items.enum_values,
                    vec!["x".to_string(), "y".to_string()]
                );
            }
            other => panic!("expected MultiSelectEnum, got {other:?}"),
        }
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
    }

    /// Field-level detail still lands on the right struct.
    #[test]
    fn variant_fields_are_preserved() {
        let json = serde_json::json!({
            "type": "integer",
            "title": "Count",
            "minimum": 1,
            "maximum": 10
        });
        let parsed: PrimitiveSchemaDefinition = serde_json::from_value(json.clone()).unwrap();
        match &parsed {
            PrimitiveSchemaDefinition::Integer(schema) => {
                assert_eq!(schema.title.as_deref(), Some("Count"));
                assert_eq!(schema.minimum, Some(1));
                assert_eq!(schema.maximum, Some(10));
            }
            other => panic!("expected Integer, got {other:?}"),
        }
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
    }

    /// An unmodelled shape is preserved verbatim rather than rejected or
    /// coerced, so a server ahead of this crate still round-trips.
    #[test]
    fn unknown_shapes_fall_back_to_raw() {
        assert_eq!(variant(serde_json::json!({"type": "null"})), "Raw");
        assert_eq!(variant(serde_json::json!({"anyOf": []})), "Raw");
        let json = serde_json::json!({"type": "null", "title": "Nothing"});
        let parsed: PrimitiveSchemaDefinition = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
    }
}
