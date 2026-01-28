//! MCP protocol types based on JSON-RPC 2.0
//!
//! These types follow the MCP specification (2025-03-26):
//! https://modelcontextprotocol.io/specification/2025-03-26

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::JsonRpcError;

/// The JSON-RPC version. MUST be "2.0".
pub const JSONRPC_VERSION: &str = "2.0";

/// The latest supported MCP protocol version.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-03-26";

/// All supported MCP protocol versions (newest first).
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-03-26"];

/// JSON-RPC 2.0 request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    pub fn new(id: impl Into<RequestId>, method: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.into(),
            method: method.into(),
            params: None,
        }
    }

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

/// JSON-RPC 2.0 response (success)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResultResponse {
    pub jsonrpc: String,
    pub id: RequestId,
    pub result: Value,
}

/// JSON-RPC 2.0 response (error)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcErrorResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<RequestId>,
    pub error: JsonRpcError,
}

/// JSON-RPC 2.0 response (either success or error)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcResponse {
    Result(JsonRpcResultResponse),
    Error(JsonRpcErrorResponse),
}

impl JsonRpcResponse {
    pub fn result(id: RequestId, result: Value) -> Self {
        Self::Result(JsonRpcResultResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result,
        })
    }

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
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcNotification {
    pub fn new(method: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params: None,
        }
    }

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
    pub const TASK_STATUS_CHANGED: &str = "notifications/tasks/status_changed";
    /// Elicitation completed (for URL-based elicitation)
    pub const ELICITATION_COMPLETE: &str = "notifications/elicitation/complete";
}

/// Log severity levels following RFC 5424 (syslog)
///
/// Levels are ordered from most severe (emergency) to least severe (debug).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
    /// Optional structured data
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl LoggingMessageParams {
    /// Create a new logging message with the given level
    pub fn new(level: LogLevel) -> Self {
        Self {
            level,
            logger: None,
            data: None,
        }
    }

    /// Set the logger name
    pub fn with_logger(mut self, logger: impl Into<String>) -> Self {
        self.logger = Some(logger.into());
        self
    }

    /// Set the structured data
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// Parameters for setting log level
#[derive(Debug, Clone, Deserialize)]
pub struct SetLogLevelParams {
    /// Minimum log level to receive
    pub level: LogLevel,
}

/// Request ID - can be string or number per JSON-RPC spec
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    String(String),
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
#[derive(Debug, Clone)]
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
    /// Enqueue an async task
    EnqueueTask(EnqueueTaskParams),
    /// List tasks
    ListTasks(ListTasksParams),
    /// Get task info
    GetTaskInfo(GetTaskInfoParams),
    /// Get task result
    GetTaskResult(GetTaskResultParams),
    /// Cancel a task
    CancelTask(CancelTaskParams),
    /// Ping (keepalive)
    Ping,
    /// Set logging level
    SetLoggingLevel(SetLogLevelParams),
    /// Unknown method
    Unknown {
        method: String,
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
            McpRequest::EnqueueTask(_) => "tasks/enqueue",
            McpRequest::ListTasks(_) => "tasks/list",
            McpRequest::GetTaskInfo(_) => "tasks/get",
            McpRequest::GetTaskResult(_) => "tasks/result",
            McpRequest::CancelTask(_) => "tasks/cancel",
            McpRequest::Ping => "ping",
            McpRequest::SetLoggingLevel(_) => "logging/setLevel",
            McpRequest::Unknown { method, .. } => method,
        }
    }
}

/// High-level MCP notification (parsed from JSON-RPC)
#[derive(Debug, Clone)]
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
        method: String,
        params: Option<Value>,
    },
}

/// Parameters for cancellation notification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelledParams {
    /// The ID of the request to cancel
    pub request_id: RequestId,
    /// Optional reason for cancellation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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
}

/// Progress token - can be string or number
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProgressToken {
    String(String),
    Number(i64),
}

/// Request metadata that can include progress token
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestMeta {
    /// Progress token for receiving progress notifications
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_token: Option<ProgressToken>,
}

/// High-level MCP response
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum McpResponse {
    Initialize(InitializeResult),
    ListTools(ListToolsResult),
    CallTool(CallToolResult),
    ListResources(ListResourcesResult),
    ListResourceTemplates(ListResourceTemplatesResult),
    ReadResource(ReadResourceResult),
    SubscribeResource(EmptyResult),
    UnsubscribeResource(EmptyResult),
    ListPrompts(ListPromptsResult),
    GetPrompt(GetPromptResult),
    EnqueueTask(EnqueueTaskResult),
    ListTasks(ListTasksResult),
    GetTaskInfo(TaskInfo),
    GetTaskResult(GetTaskResultResult),
    CancelTask(CancelTaskResult),
    SetLoggingLevel(EmptyResult),
    Pong(EmptyResult),
    Empty(EmptyResult),
}

// =============================================================================
// Initialize
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: String,
    pub capabilities: ClientCapabilities,
    pub client_info: Implementation,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ClientCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<RootsCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<SamplingCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elicitation: Option<ElicitationCapability>,
}

/// Client capability for elicitation (requesting user input)
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ElicitationCapability {
    /// Support for form-based elicitation
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form: Option<ElicitationFormCapability>,
    /// Support for URL-based elicitation
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<ElicitationUrlCapability>,
}

/// Marker for form-based elicitation support
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ElicitationFormCapability {}

/// Marker for URL-based elicitation support
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ElicitationUrlCapability {}

/// Client capability for roots (filesystem access)
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RootsCapability {
    /// Whether the client supports roots list changed notifications
    #[serde(default)]
    pub list_changed: bool,
}

/// Represents a root directory or file that the server can operate on
///
/// Roots allow clients to expose filesystem roots to servers, enabling:
/// - Scoped file access
/// - Workspace awareness
/// - Security boundaries
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Root {
    /// The URI identifying the root. Must start with `file://` for now.
    pub uri: String,
    /// Optional human-readable name for the root
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Root {
    /// Create a new root with just a URI
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            name: None,
        }
    }

    /// Create a new root with a URI and name
    pub fn with_name(uri: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            name: Some(name.into()),
        }
    }
}

/// Parameters for roots/list request (server to client)
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ListRootsParams {
    /// Optional metadata
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<RequestMeta>,
}

/// Result of roots/list request
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListRootsResult {
    /// The list of roots available to the server
    pub roots: Vec<Root>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SamplingCapability {}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Implementation {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    pub server_info: Implementation,
    /// Optional instructions describing how to use this server.
    /// These hints help LLMs understand the server's features.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourcesCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<PromptsCapability>,
    /// Logging capability - servers that emit log notifications declare this
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<LoggingCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tasks: Option<TasksCapability>,
}

/// Logging capability declaration
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoggingCapability {}

/// Capability for async task management
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TasksCapability {
    /// Default poll interval suggestion in seconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_poll_interval: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCapability {
    #[serde(default)]
    pub list_changed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesCapability {
    #[serde(default)]
    pub subscribe: bool,
    #[serde(default)]
    pub list_changed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptsCapability {
    #[serde(default)]
    pub list_changed: bool,
}

// =============================================================================
// Tools
// =============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListToolsParams {
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListToolsResult {
    pub tools: Vec<ToolDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Tool definition as returned by tools/list
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
    /// Optional annotations describing tool behavior.
    /// Note: Clients MUST consider these untrusted unless from a trusted server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
}

/// Annotations describing tool behavior for trust and safety.
/// Clients MUST consider these untrusted unless the server is trusted.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

fn default_true() -> bool {
    true
}

fn is_true(v: &bool) -> bool {
    *v
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
    /// Request metadata including progress token
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<RequestMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
    pub content: Vec<Content>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
}

impl CallToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Text {
                text: text.into(),
                annotations: None,
            }],
            is_error: false,
            structured_content: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Text {
                text: message.into(),
                annotations: None,
            }],
            is_error: true,
            structured_content: None,
        }
    }

    pub fn json(value: Value) -> Self {
        let text = serde_json::to_string_pretty(&value).unwrap_or_default();
        Self {
            content: vec![Content::Text {
                text,
                annotations: None,
            }],
            is_error: false,
            structured_content: Some(value),
        }
    }
}

/// Content types for tool results
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
    },
    Audio {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
    },
    Resource {
        resource: ResourceContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
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
}

/// Role indicating who content is intended for
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceContent {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

// =============================================================================
// Resources
// =============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListResourcesParams {
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListResourcesResult {
    pub resources: Vec<ResourceDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceDefinition {
    pub uri: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResourceParams {
    pub uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResourceResult {
    pub contents: Vec<ResourceContent>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubscribeResourceParams {
    pub uri: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UnsubscribeResourceParams {
    pub uri: String,
}

/// Parameters for listing resource templates
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListResourceTemplatesParams {
    /// Pagination cursor from previous response
    #[serde(default)]
    pub cursor: Option<String>,
}

/// Result of listing resource templates
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListResourceTemplatesResult {
    /// Available resource templates
    pub resource_templates: Vec<ResourceTemplateDefinition>,
    /// Cursor for next page (if more templates available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
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
    /// Description of what resources this template provides
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// MIME type hint for resources from this template
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

// =============================================================================
// Prompts
// =============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListPromptsParams {
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListPromptsResult {
    pub prompts: Vec<PromptDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<PromptArgument>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptArgument {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetPromptParams {
    pub name: String,
    #[serde(default)]
    pub arguments: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetPromptResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub messages: Vec<PromptMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptMessage {
    pub role: PromptRole,
    pub content: Content,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptRole {
    User,
    Assistant,
}

// =============================================================================
// Tasks (async operations)
// =============================================================================

/// Status of an async task
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// Information about a task
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskInfo {
    /// Unique task identifier
    pub task_id: String,
    /// Current task status
    pub status: TaskStatus,
    /// ISO 8601 timestamp when the task was created
    pub created_at: String,
    /// Time-to-live in seconds (how long to keep the result after completion)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<u64>,
    /// Suggested polling interval in seconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_interval: Option<u64>,
    /// Progress percentage (0-100) if available
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    /// Human-readable status message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Parameters for enqueuing a task
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnqueueTaskParams {
    /// Tool name to execute
    pub tool_name: String,
    /// Arguments to pass to the tool
    #[serde(default)]
    pub arguments: Value,
    /// Optional time-to-live for the task result in seconds
    #[serde(default)]
    pub ttl: Option<u64>,
}

/// Result of enqueuing a task
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnqueueTaskResult {
    /// The task ID for tracking
    pub task_id: String,
    /// Initial status (should be Working)
    pub status: TaskStatus,
    /// Suggested polling interval in seconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_interval: Option<u64>,
}

/// Parameters for listing tasks
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListTasksParams {
    /// Filter by status (optional)
    #[serde(default)]
    pub status: Option<TaskStatus>,
    /// Pagination cursor
    #[serde(default)]
    pub cursor: Option<String>,
}

/// Result of listing tasks
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListTasksResult {
    /// List of tasks
    pub tasks: Vec<TaskInfo>,
    /// Next cursor for pagination
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Parameters for getting task info
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTaskInfoParams {
    /// Task ID to query
    pub task_id: String,
}

/// Result of getting task info
pub type GetTaskInfoResult = TaskInfo;

/// Parameters for getting task result
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTaskResultParams {
    /// Task ID to get result for
    pub task_id: String,
}

/// Result of getting task result
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTaskResultResult {
    /// Task ID
    pub task_id: String,
    /// Task status
    pub status: TaskStatus,
    /// The tool call result (if completed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<CallToolResult>,
    /// Error message (if failed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Parameters for cancelling a task
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelTaskParams {
    /// Task ID to cancel
    pub task_id: String,
    /// Optional reason for cancellation
    #[serde(default)]
    pub reason: Option<String>,
}

/// Result of cancelling a task
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelTaskResult {
    /// Whether the cancellation was successful
    pub cancelled: bool,
    /// Updated task status
    pub status: TaskStatus,
}

/// Notification params when task status changes
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusChangedParams {
    /// Task ID
    pub task_id: String,
    /// New status
    pub status: TaskStatus,
    /// Human-readable message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// =============================================================================
// Elicitation (server-to-client user input requests)
// =============================================================================

/// Parameters for form-based elicitation request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitFormParams {
    /// The elicitation mode
    pub mode: ElicitMode,
    /// Message to present to the user explaining what information is needed
    pub message: String,
    /// Schema for the form fields (restricted subset of JSON Schema)
    pub requested_schema: ElicitFormSchema,
    /// Request metadata including progress token
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<RequestMeta>,
}

/// Parameters for URL-based elicitation request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitUrlParams {
    /// The elicitation mode
    pub mode: ElicitMode,
    /// Unique ID for this elicitation (opaque to client)
    pub elicitation_id: String,
    /// Message explaining why the user needs to navigate to the URL
    pub message: String,
    /// The URL the user should navigate to
    pub url: String,
    /// Request metadata including progress token
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<RequestMeta>,
}

/// Elicitation request parameters (union of form and URL modes)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ElicitRequestParams {
    Form(ElicitFormParams),
    Url(ElicitUrlParams),
}

/// Elicitation mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ElicitMode {
    /// Form-based elicitation with structured input
    Form,
    /// URL-based elicitation (out-of-band)
    Url,
}

/// Restricted JSON Schema for elicitation forms
///
/// Only allows top-level properties with primitive types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitFormSchema {
    /// Must be "object"
    #[serde(rename = "type")]
    pub schema_type: String,
    /// Map of property names to their schema definitions
    pub properties: std::collections::HashMap<String, PrimitiveSchemaDefinition>,
    /// List of required property names
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
}

impl ElicitFormSchema {
    /// Create a new form schema
    pub fn new() -> Self {
        Self {
            schema_type: "object".to_string(),
            properties: std::collections::HashMap::new(),
            required: Vec::new(),
        }
    }

    /// Add a string field
    pub fn string_field(mut self, name: &str, description: Option<&str>, required: bool) -> Self {
        self.properties.insert(
            name.to_string(),
            PrimitiveSchemaDefinition::String(StringSchema {
                schema_type: "string".to_string(),
                description: description.map(|s| s.to_string()),
                format: None,
                min_length: None,
                max_length: None,
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
                description: description.map(|s| s.to_string()),
                minimum: None,
                maximum: None,
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
                description: description.map(|s| s.to_string()),
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
                description: description.map(|s| s.to_string()),
                enum_values: options,
            }),
        );
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PrimitiveSchemaDefinition {
    String(StringSchema),
    Number(NumberSchema),
    Boolean(BooleanSchema),
    SingleSelectEnum(SingleSelectEnumSchema),
    MultiSelectEnum(MultiSelectEnumSchema),
}

/// String field schema
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StringSchema {
    #[serde(rename = "type")]
    pub schema_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u64>,
}

/// Number field schema
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NumberSchema {
    #[serde(rename = "type")]
    pub schema_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
}

/// Boolean field schema
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BooleanSchema {
    #[serde(rename = "type")]
    pub schema_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Single-select enum schema
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SingleSelectEnumSchema {
    #[serde(rename = "type")]
    pub schema_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "enum")]
    pub enum_values: Vec<String>,
}

/// Multi-select enum schema
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiSelectEnumSchema {
    #[serde(rename = "type")]
    pub schema_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub items: MultiSelectEnumItems,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unique_items: Option<bool>,
}

/// Items definition for multi-select enum
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiSelectEnumItems {
    #[serde(rename = "type")]
    pub schema_type: String,
    #[serde(rename = "enum")]
    pub enum_values: Vec<String>,
}

/// User action in response to elicitation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
}

impl ElicitResult {
    /// Create an accept result with content
    pub fn accept(content: std::collections::HashMap<String, ElicitFieldValue>) -> Self {
        Self {
            action: ElicitAction::Accept,
            content: Some(content),
        }
    }

    /// Create a decline result
    pub fn decline() -> Self {
        Self {
            action: ElicitAction::Decline,
            content: None,
        }
    }

    /// Create a cancel result
    pub fn cancel() -> Self {
        Self {
            action: ElicitAction::Cancel,
            content: None,
        }
    }
}

/// Value from an elicitation form field
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ElicitFieldValue {
    String(String),
    Number(f64),
    Integer(i64),
    Boolean(bool),
    StringArray(Vec<String>),
}

/// Parameters for elicitation complete notification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationCompleteParams {
    /// The ID of the elicitation that completed
    pub elicitation_id: String,
}

// =============================================================================
// Common
// =============================================================================

#[derive(Debug, Clone, Default, Serialize)]
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
            "tasks/enqueue" => {
                let p: EnqueueTaskParams = serde_json::from_value(params)?;
                Ok(McpRequest::EnqueueTask(p))
            }
            "tasks/list" => {
                let p: ListTasksParams = serde_json::from_value(params).unwrap_or_default();
                Ok(McpRequest::ListTasks(p))
            }
            "tasks/get" => {
                let p: GetTaskInfoParams = serde_json::from_value(params)?;
                Ok(McpRequest::GetTaskInfo(p))
            }
            "tasks/result" => {
                let p: GetTaskResultParams = serde_json::from_value(params)?;
                Ok(McpRequest::GetTaskResult(p))
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

#[cfg(test)]
mod tests {
    use super::*;

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
        };

        let json = serde_json::to_value(&caps).unwrap();
        assert!(json["elicitation"]["form"].is_object());
        assert!(json["elicitation"]["url"].is_object());
    }

    #[test]
    fn test_elicit_url_params() {
        let params = ElicitUrlParams {
            mode: ElicitMode::Url,
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
        };

        let json = serde_json::to_value(&result).unwrap();
        let roots = json["roots"].as_array().unwrap();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0]["uri"], "file:///project1");
        assert_eq!(roots[1]["name"], "Project 2");
    }

    #[test]
    fn test_roots_capability_serialization() {
        let cap = RootsCapability { list_changed: true };
        let json = serde_json::to_value(&cap).unwrap();
        assert_eq!(json["listChanged"], true);
    }

    #[test]
    fn test_client_capabilities_with_roots() {
        let caps = ClientCapabilities {
            roots: Some(RootsCapability { list_changed: true }),
            sampling: None,
            elicitation: None,
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
}
