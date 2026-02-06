//! MCP protocol types based on JSON-RPC 2.0
//!
//! These types follow the MCP specification (2025-11-25):
//! <https://modelcontextprotocol.io/specification/2025-11-25>

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::JsonRpcError;

/// The JSON-RPC version. MUST be "2.0".
pub const JSONRPC_VERSION: &str = "2.0";

/// The latest supported MCP protocol version.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

/// All supported MCP protocol versions (newest first).
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-03-26"];

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
    /// Request identifier (may be null for parse errors).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<RequestId>,
    /// The error details.
    pub error: JsonRpcError,
}

/// JSON-RPC 2.0 response (either success or error).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
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
    /// Request completion suggestions
    Complete(CompleteParams),
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
            McpRequest::Complete(_) => "completion/complete",
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
    Complete(CompleteResult),
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SamplingCapability {}

/// Server capability for providing completions
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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
}

impl CompleteResult {
    /// Create a new completion result
    pub fn new(values: Vec<String>) -> Self {
        Self {
            completion: Completion::new(values),
        }
    }

    /// Create a completion result with pagination info
    pub fn with_pagination(values: Vec<String>, total: u32, has_more: bool) -> Self {
        Self {
            completion: Completion::with_pagination(values, total, has_more),
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
    pub name: String,
}

impl ModelHint {
    /// Create a new model hint
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
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
    /// The content of the message
    pub content: SamplingContent,
}

impl SamplingMessage {
    /// Create a user message with text content
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: ContentRole::User,
            content: SamplingContent::Text { text: text.into() },
        }
    }

    /// Create an assistant message with text content
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: ContentRole::Assistant,
            content: SamplingContent::Text { text: text.into() },
        }
    }
}

/// Tool definition for use in sampling requests (SEP-1577)
///
/// Describes a tool that can be used during a sampling request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SamplingTool {
    /// The name of the tool
    pub name: String,
    /// Description of what the tool does
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema describing the tool's input parameters
    pub input_schema: Value,
}

/// Tool choice mode for sampling requests (SEP-1577)
///
/// Controls how the LLM should use the available tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolChoice {
    /// The tool choice mode:
    /// - "auto": Model decides whether to use tools
    /// - "required": Model must use a tool
    /// - "none": Model should not use tools
    #[serde(rename = "type")]
    pub mode: String,
}

impl ToolChoice {
    /// Model decides whether to use tools
    pub fn auto() -> Self {
        Self {
            mode: "auto".to_string(),
        }
    }

    /// Model must use a tool
    pub fn required() -> Self {
        Self {
            mode: "required".to_string(),
        }
    }

    /// Model should not use tools
    pub fn none() -> Self {
        Self {
            mode: "none".to_string(),
        }
    }
}

/// Content types for sampling messages
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SamplingContent {
    /// Text content
    Text {
        /// The text content
        text: String,
    },
    /// Image content
    Image {
        /// Base64-encoded image data
        data: String,
        /// MIME type of the image
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// Audio content (if supported)
    Audio {
        /// Base64-encoded audio data
        data: String,
        /// MIME type of the audio
        #[serde(rename = "mimeType")]
        mime_type: String,
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
    },
    /// Result of a tool invocation (SEP-1577)
    #[serde(rename = "tool_result")]
    ToolResult {
        /// ID of the tool use this result corresponds to
        tool_use_id: String,
        /// The tool result content
        content: Vec<SamplingContent>,
        /// Whether the tool execution resulted in an error
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
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
    /// use tower_mcp::protocol::SamplingContent;
    ///
    /// let text_content = SamplingContent::Text { text: "Hello".into() };
    /// assert_eq!(text_content.as_text(), Some("Hello"));
    ///
    /// let image_content = SamplingContent::Image {
    ///     data: "base64...".into(),
    ///     mime_type: "image/png".into(),
    /// };
    /// assert_eq!(image_content.as_text(), None);
    /// ```
    pub fn as_text(&self) -> Option<&str> {
        match self {
            SamplingContent::Text { text } => Some(text),
            _ => None,
        }
    }
}

/// Content that can be either a single item or an array (for CreateMessageResult)
///
/// The MCP spec allows CreateMessageResult.content to be either a single
/// SamplingContent or an array of SamplingContent items.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
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
    /// use tower_mcp::protocol::{CreateMessageResult, SamplingContent, SamplingContentOrArray, ContentRole};
    ///
    /// let result = CreateMessageResult {
    ///     content: SamplingContentOrArray::Single(SamplingContent::Text {
    ///         text: "Hello, world!".into(),
    ///     }),
    ///     model: "claude-3".into(),
    ///     role: ContentRole::Assistant,
    ///     stop_reason: None,
    /// };
    /// assert_eq!(result.first_text(), Some("Hello, world!"));
    /// ```
    pub fn first_text(&self) -> Option<&str> {
        self.content.items().iter().find_map(|c| c.as_text())
    }
}

/// Information about a client or server implementation
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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
    /// Completion capability - server provides autocomplete suggestions
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completions: Option<CompletionsCapability>,
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
    /// Human-readable title for display purposes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
    /// Request metadata including progress token
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<RequestMeta>,
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
/// use tower_mcp::CallToolResult;
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
            }],
            is_error: false,
            structured_content: None,
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
            }],
            is_error: true,
            structured_content: None,
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
            }],
            is_error: false,
            structured_content: Some(value),
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
    /// use tower_mcp::CallToolResult;
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
    /// use tower_mcp::CallToolResult;
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

    /// Create a result with an image
    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Image {
                data: data.into(),
                mime_type: mime_type.into(),
                annotations: None,
            }],
            is_error: false,
            structured_content: None,
        }
    }

    /// Create a result with audio
    pub fn audio(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Audio {
                data: data.into(),
                mime_type: mime_type.into(),
                annotations: None,
            }],
            is_error: false,
            structured_content: None,
        }
    }

    /// Create a result with a resource link
    pub fn resource_link(uri: impl Into<String>) -> Self {
        Self {
            content: vec![Content::ResourceLink {
                uri: uri.into(),
                name: None,
                description: None,
                mime_type: None,
                annotations: None,
            }],
            is_error: false,
            structured_content: None,
        }
    }

    /// Create a result with a resource link including metadata
    pub fn resource_link_with_meta(
        uri: impl Into<String>,
        name: Option<String>,
        description: Option<String>,
        mime_type: Option<String>,
    ) -> Self {
        Self {
            content: vec![Content::ResourceLink {
                uri: uri.into(),
                name,
                description,
                mime_type,
                annotations: None,
            }],
            is_error: false,
            structured_content: None,
        }
    }

    /// Create a result with an embedded resource
    pub fn resource(resource: ResourceContent) -> Self {
        Self {
            content: vec![Content::Resource {
                resource,
                annotations: None,
            }],
            is_error: false,
            structured_content: None,
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
    /// use tower_mcp::CallToolResult;
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
    /// use tower_mcp::CallToolResult;
    ///
    /// let result = CallToolResult::text("hello");
    /// assert_eq!(result.first_text(), Some("hello"));
    /// ```
    pub fn first_text(&self) -> Option<&str> {
        self.content.iter().find_map(|c| c.as_text())
    }
}

/// Content types for tool results, resources, and prompts.
///
/// Content can be text, images, audio, or embedded resources. Each variant
/// supports optional annotations for audience targeting and priority hints.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    /// Plain text content.
    Text {
        /// The text content.
        text: String,
        /// Optional annotations for this content.
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
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
    },
    /// Embedded resource content.
    Resource {
        /// The embedded resource.
        resource: ResourceContent,
        /// Optional annotations for this content.
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<ContentAnnotations>,
    },
    /// Link to a resource (without embedding the content)
    ResourceLink {
        /// URI of the resource
        uri: String,
        /// Human-readable name
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Description of the resource
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// MIME type of the resource
        #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
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

impl Content {
    /// Extract the text from a [`Content::Text`] variant.
    ///
    /// Returns `None` for non-text content variants.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::Content;
    ///
    /// let content = Content::Text { text: "hello".into(), annotations: None };
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
    /// Human-readable title for display purposes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Optional icons for display in user interfaces
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icons: Option<Vec<ToolIcon>>,
    /// Size of the resource in bytes (if known)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResourceParams {
    pub uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResourceResult {
    pub contents: Vec<ResourceContent>,
}

impl ReadResourceResult {
    /// Create a result with text content.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::ReadResourceResult;
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
            }],
        }
    }

    /// Create a result with text content and a specific MIME type.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::ReadResourceResult;
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
            }],
        }
    }

    /// Create a result with JSON content.
    ///
    /// The value is serialized to a JSON string automatically.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::ReadResourceResult;
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
            }],
        }
    }

    /// Create a result with binary content (base64 encoded).
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::ReadResourceResult;
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
            }],
        }
    }

    /// Create a result with binary content and a specific MIME type.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::ReadResourceResult;
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
            }],
        }
    }

    /// Get the text from the first content item.
    ///
    /// Returns `None` if there are no contents or the first item has no text.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::ReadResourceResult;
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
    /// use tower_mcp::ReadResourceResult;
    ///
    /// let result = ReadResourceResult::text("file://readme.md", "# Hello");
    /// assert_eq!(result.first_uri(), Some("file://readme.md"));
    /// ```
    pub fn first_uri(&self) -> Option<&str> {
        self.contents.first().map(|c| c.uri.as_str())
    }
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
    /// Human-readable title for display purposes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional icons for display in user interfaces
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icons: Option<Vec<ToolIcon>>,
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

impl GetPromptResult {
    /// Create a result with a single user message.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::GetPromptResult;
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
                },
            }],
        }
    }

    /// Create a result with a single user message and description.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::GetPromptResult;
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
                },
            }],
        }
    }

    /// Create a result with a single assistant message.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::GetPromptResult;
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
                },
            }],
        }
    }

    /// Create a builder for constructing prompts with multiple messages.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::GetPromptResult;
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
    /// use tower_mcp::GetPromptResult;
    ///
    /// let result = GetPromptResult::user_message("Analyze this code.");
    /// assert_eq!(result.first_message_text(), Some("Analyze this code."));
    /// ```
    pub fn first_message_text(&self) -> Option<&str> {
        self.messages.first().and_then(|m| m.content.as_text())
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
            },
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
            },
        });
        self
    }

    /// Build the final result.
    pub fn build(self) -> GetPromptResult {
        GetPromptResult {
            description: self.description,
            messages: self.messages,
        }
    }
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
                description: description.map(|s| s.to_string()),
                format: None,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
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
    /// Default value for this field
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// Integer field schema
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegerSchema {
    #[serde(rename = "type")]
    pub schema_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum: Option<i64>,
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
    #[serde(rename = "type")]
    pub schema_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
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
    #[serde(rename = "type")]
    pub schema_type: String,
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
    #[serde(rename = "type")]
    pub schema_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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
            "completion/complete" => {
                let p: CompleteParams = serde_json::from_value(params)?;
                Ok(McpRequest::Complete(p))
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
        };

        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json["ref"]["type"], "ref/prompt");
        assert_eq!(json["ref"]["name"], "sql-prompt");
        assert_eq!(json["argument"]["name"], "query");
        assert_eq!(json["argument"]["value"], "SEL");
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
        assert_eq!(hint.name, "claude-3-opus");
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
            matches!(msg.content, SamplingContent::Text { text } if text == "Hello, how are you?")
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
            name: Some("test.txt".to_string()),
            description: Some("A test file".to_string()),
            mime_type: Some("text/plain".to_string()),
            annotations: None,
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
        let result = CallToolResult::resource_link("file:///output.json");
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
            description: Some("Get current weather".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": { "type": "string" }
                }
            }),
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

        let required = ToolChoice::required();
        assert_eq!(required.mode, "required");

        let none = ToolChoice::none();
        assert_eq!(none.mode, "none");

        // Test serialization
        let json = serde_json::to_value(&auto).unwrap();
        assert_eq!(json["type"], "auto");
    }

    #[test]
    fn test_sampling_content_tool_use() {
        let content = SamplingContent::ToolUse {
            id: "tool_123".to_string(),
            name: "get_weather".to_string(),
            input: serde_json::json!({"location": "San Francisco"}),
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
            }],
            is_error: None,
        };
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["type"], "tool_result");
        assert_eq!(json["tool_use_id"], "tool_123");
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
            SamplingContent::Text { text } => assert_eq!(text, "Hello"),
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
            description: Some("Do math".to_string()),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let params = CreateMessageParams::new(vec![], 100)
            .tools(vec![tool])
            .tool_choice(ToolChoice::auto());

        let json = serde_json::to_value(&params).unwrap();
        assert!(json["tools"].is_array());
        assert_eq!(json["tools"][0]["name"], "calculator");
        assert_eq!(json["toolChoice"]["type"], "auto");
    }

    #[test]
    fn test_create_message_result_content_items() {
        let result = CreateMessageResult {
            content: SamplingContentOrArray::Array(vec![
                SamplingContent::Text {
                    text: "First".to_string(),
                },
                SamplingContent::Text {
                    text: "Second".to_string(),
                },
            ]),
            model: "test".to_string(),
            role: ContentRole::Assistant,
            stop_reason: None,
        };
        let items = result.content_items();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn test_sampling_content_as_text() {
        let text_content = SamplingContent::Text {
            text: "Hello".to_string(),
        };
        assert_eq!(text_content.as_text(), Some("Hello"));

        let image_content = SamplingContent::Image {
            data: "base64data".to_string(),
            mime_type: "image/png".to_string(),
        };
        assert_eq!(image_content.as_text(), None);

        let audio_content = SamplingContent::Audio {
            data: "base64audio".to_string(),
            mime_type: "audio/wav".to_string(),
        };
        assert_eq!(audio_content.as_text(), None);
    }

    #[test]
    fn test_create_message_result_first_text_single() {
        let result = CreateMessageResult {
            content: SamplingContentOrArray::Single(SamplingContent::Text {
                text: "Hello, world!".to_string(),
            }),
            model: "test".to_string(),
            role: ContentRole::Assistant,
            stop_reason: None,
        };
        assert_eq!(result.first_text(), Some("Hello, world!"));
    }

    #[test]
    fn test_create_message_result_first_text_array() {
        let result = CreateMessageResult {
            content: SamplingContentOrArray::Array(vec![
                SamplingContent::Text {
                    text: "First".to_string(),
                },
                SamplingContent::Text {
                    text: "Second".to_string(),
                },
            ]),
            model: "test".to_string(),
            role: ContentRole::Assistant,
            stop_reason: None,
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
                },
                SamplingContent::Text {
                    text: "After image".to_string(),
                },
            ]),
            model: "test".to_string(),
            role: ContentRole::Assistant,
            stop_reason: None,
        };
        assert_eq!(result.first_text(), Some("After image"));
    }

    #[test]
    fn test_create_message_result_first_text_none() {
        let result = CreateMessageResult {
            content: SamplingContentOrArray::Single(SamplingContent::Image {
                data: "base64data".to_string(),
                mime_type: "image/png".to_string(),
            }),
            model: "test".to_string(),
            role: ContentRole::Assistant,
            stop_reason: None,
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
        // Default::default() uses Rust defaults (false for bool).
        // The serde defaults (destructive_hint=true, open_world_hint=true)
        // only apply during deserialization.
        let annotations = ToolAnnotations::default();

        assert!(!annotations.is_read_only());
        assert!(!annotations.is_destructive());
        assert!(!annotations.is_idempotent());
        assert!(!annotations.is_open_world());
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
}
