//! # tower-mcp
//!
//! Tower-native Model Context Protocol (MCP) implementation for Rust.
//!
//! This crate provides a composable, middleware-friendly approach to building
//! MCP servers and clients using the [Tower](https://docs.rs/tower) service abstraction.
//!
//! ## Philosophy
//!
//! Unlike framework-style MCP implementations, tower-mcp treats MCP as just another
//! protocol that can be served through Tower's `Service` trait. This means:
//!
//! - Standard tower middleware (tracing, metrics, rate limiting, auth) just works
//! - Same service can be exposed over multiple transports (stdio, HTTP, WebSocket)
//! - Easy integration with existing tower-based applications (axum, tonic, etc.)
//!
//! ## Familiar to axum Users
//!
//! If you've used [axum](https://docs.rs/axum), tower-mcp's API will feel familiar.
//! We've adopted axum's patterns for a consistent Rust web ecosystem experience:
//!
//! - **Extractor pattern**: Tool handlers use extractors like [`extract::State<T>`],
//!   [`extract::Json<T>`], and [`extract::Context`] - just like axum's request extractors
//! - **Router composition**: [`McpRouter::merge()`] and [`McpRouter::nest()`] work like
//!   axum's router methods for combining routers
//! - **Per-route middleware**: Apply Tower layers to individual tools, resources, or
//!   prompts via `.layer()` on builders
//! - **Builder pattern**: Fluent builders for tools, resources, and prompts
//!
//! ```rust
//! use std::sync::Arc;
//! use tower_mcp::{ToolBuilder, CallToolResult};
//! use tower_mcp::extract::{State, Json, Context};
//! use schemars::JsonSchema;
//! use serde::Deserialize;
//!
//! #[derive(Clone)]
//! struct AppState { db_url: String }
//!
//! #[derive(Deserialize, JsonSchema)]
//! struct SearchInput { query: String }
//!
//! // Looks just like an axum handler!
//! let tool = ToolBuilder::new("search")
//!     .description("Search the database")
//!     .extractor_handler(
//!         Arc::new(AppState { db_url: "postgres://...".into() }),
//!         |State(app): State<Arc<AppState>>,
//!          ctx: Context,
//!          Json(input): Json<SearchInput>| async move {
//!             ctx.report_progress(0.5, Some(1.0), Some("Searching...")).await;
//!             Ok(CallToolResult::text(format!("Found results for: {}", input.query)))
//!         },
//!     )
//!     .build();
//! ```
//!
//! ## Quick Start: Server
//!
//! Build an MCP server with tools, resources, and prompts:
//!
//! ```rust,no_run
//! use tower_mcp::{BoxError, McpRouter, ToolBuilder, CallToolResult, StdioTransport};
//! use schemars::JsonSchema;
//! use serde::Deserialize;
//!
//! #[derive(Debug, Deserialize, JsonSchema)]
//! struct GreetInput {
//!     name: String,
//! }
//!
//! #[tokio::main]
//! async fn main() -> Result<(), BoxError> {
//!     // Define a tool
//!     let greet = ToolBuilder::new("greet")
//!         .description("Greet someone by name")
//!         .handler(|input: GreetInput| async move {
//!             Ok(CallToolResult::text(format!("Hello, {}!", input.name)))
//!         })
//!         .build();
//!
//!     // Create router and run over stdio
//!     let router = McpRouter::new()
//!         .server_info("my-server", "1.0.0")
//!         .tool(greet);
//!
//!     StdioTransport::new(router).run().await?;
//!     Ok(())
//! }
//! ```
//!
//! ## Quick Start: Client
//!
//! Connect to an MCP server and call tools:
//!
//! ```rust,no_run
//! use tower_mcp::BoxError;
//! use tower_mcp::client::{McpClient, StdioClientTransport};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), BoxError> {
//!     // Connect to server
//!     let transport = StdioClientTransport::spawn("my-mcp-server", &[]).await?;
//!     let mut client = McpClient::new(transport);
//!
//!     // Initialize and list tools
//!     client.initialize("my-client", "1.0.0").await?;
//!     let tools = client.list_tools().await?;
//!
//!     // Call a tool
//!     let result = client.call_tool("greet", serde_json::json!({"name": "World"})).await?;
//!     println!("{:?}", result);
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Key Types
//!
//! ### Server
//! - [`McpRouter`] - Routes MCP requests to tools, resources, and prompts
//! - [`ToolBuilder`] - Builder for defining tools with type-safe handlers
//! - [`ResourceBuilder`] - Builder for defining resources
//! - [`PromptBuilder`] - Builder for defining prompts
//! - [`StdioTransport`] - Stdio transport for CLI servers
//!
//! ### Client
//! - [`McpClient`] - Client for connecting to MCP servers
//! - [`StdioClientTransport`] - Spawn and connect to server subprocesses
//!
//! ### Protocol
//! - [`CallToolResult`] - Tool execution result with content
//! - [`ReadResourceResult`] - Resource read result
//! - [`GetPromptResult`] - Prompt expansion result
//! - [`Content`] - Text, image, audio, or resource content
//!
//! ## Feature Flags
//!
//! - `full` - Enable all optional features
//! - `http` - HTTP/SSE transport for web servers
//! - `websocket` - WebSocket transport for bidirectional communication
//! - `childproc` - Child process transport for subprocess management
//! - `oauth` - OAuth 2.1 resource server support (token validation, metadata endpoint)
//! - `testing` - Test utilities (`TestClient`) for ergonomic MCP server testing
//! - `dynamic-tools` - Runtime tool (de)registration via `DynamicToolRegistry`
//!
//! ## Middleware Placement Guide
//!
//! tower-mcp supports Tower middleware at multiple levels. Choose based on scope:
//!
//! | Level | Method | Scope | Use Cases |
//! |-------|--------|-------|-----------|
//! | **Transport** | `HttpTransport::layer()` | All MCP requests | Global timeout, rate limit, metrics |
//! | **axum** | `.into_router().layer()` | HTTP layer only | CORS, compression, request logging |
//! | **Per-tool** | `ToolBuilder::...layer()` | Single tool | Tool-specific timeout, concurrency |
//! | **Per-resource** | `ResourceBuilder::...layer()` | Single resource | Caching, read timeout |
//! | **Per-prompt** | `PromptBuilder::...layer()` | Single prompt | Generation timeout |
//!
//! ### Decision Tree
//!
//! ```text
//! Where should my middleware go?
//! │
//! ├─ Affects ALL MCP requests?
//! │  └─ Yes → Transport: HttpTransport::layer() or WebSocketTransport::layer()
//! │
//! ├─ HTTP-specific (CORS, compression, headers)?
//! │  └─ Yes → axum: transport.into_router().layer(...)
//! │
//! ├─ Only one specific tool?
//! │  └─ Yes → Per-tool: ToolBuilder::...handler(...).layer(...)
//! │
//! ├─ Only one specific resource?
//! │  └─ Yes → Per-resource: ResourceBuilder::...handler(...).layer(...)
//! │
//! └─ Only one specific prompt?
//!    └─ Yes → Per-prompt: PromptBuilder::...handler(...).layer(...)
//! ```
//!
//! ### Example: Layered Timeouts
//!
//! ```rust,ignore
//! use std::time::Duration;
//! use tower::timeout::TimeoutLayer;
//! use tower_mcp::{McpRouter, ToolBuilder, CallToolResult, HttpTransport};
//! use schemars::JsonSchema;
//! use serde::Deserialize;
//!
//! #[derive(Debug, Deserialize, JsonSchema)]
//! struct SearchInput { query: String }
//!
//! // This tool gets a longer timeout than the global default
//! let slow_search = ToolBuilder::new("slow_search")
//!     .description("Thorough search (may take a while)")
//!     .handler(|input: SearchInput| async move {
//!         // ... slow operation ...
//!         Ok(CallToolResult::text("results"))
//!     })
//!     .layer(TimeoutLayer::new(Duration::from_secs(60)))  // 60s for this tool
//!     .build();
//!
//! let router = McpRouter::new()
//!     .server_info("example", "1.0.0")
//!     .tool(slow_search);
//!
//! // Global 30s timeout for all OTHER requests
//! let transport = HttpTransport::new(router)
//!     .layer(TimeoutLayer::new(Duration::from_secs(30)));
//! ```
//!
//! In this example:
//! - `slow_search` tool has a 60-second timeout (per-tool layer)
//! - All other MCP requests have a 30-second timeout (transport layer)
//! - The per-tool layer is **inner** to the transport layer
//!
//! ### Layer Ordering
//!
//! Layers wrap from outside in. The first layer added is the outermost:
//!
//! ```text
//! Request → [Transport Layer] → [Per-tool Layer] → Handler → Response
//! ```
//!
//! For per-tool/resource/prompt, chained `.layer()` calls also wrap outside-in:
//!
//! ```rust,ignore
//! ToolBuilder::new("api")
//!     .handler(...)
//!     .layer(TimeoutLayer::new(...))      // Outer: timeout checked first
//!     .layer(ConcurrencyLimitLayer::new(5)) // Inner: concurrency after timeout
//!     .build()
//! ```
//!
//! ### Full Example
//!
//! See [`examples/tool_middleware.rs`](https://github.com/joshrotenberg/tower-mcp/blob/main/examples/tool_middleware.rs)
//! for a complete example demonstrating:
//! - Different timeouts per tool
//! - Concurrency limiting for expensive operations
//! - Multiple layers combined on a single tool
//!
//! ## Advanced Features
//!
//! ### Sampling (LLM Requests)
//!
//! Tools can request LLM completions from the client via [`RequestContext::sample()`].
//! This enables AI-assisted tools like "suggest a query" or "analyze results":
//!
//! ```rust,ignore
//! use tower_mcp::{ToolBuilder, CallToolResult, CreateMessageParams, SamplingMessage};
//! use tower_mcp::extract::Context;
//!
//! let tool = ToolBuilder::new("suggest")
//!     .description("Get AI suggestions")
//!     .extractor_handler(|ctx: Context| async move {
//!         if !ctx.can_sample() {
//!             return Ok(CallToolResult::error("Sampling not available"));
//!         }
//!
//!         let params = CreateMessageParams::new()
//!             .message(SamplingMessage::user("Suggest 3 search queries for: rust async"))
//!             .max_tokens(200);
//!
//!         let result = ctx.sample(params).await?;
//!         let text = result.first_text().unwrap_or("No response");
//!         Ok(CallToolResult::text(text))
//!     })
//!     .build();
//! ```
//!
//! ### Elicitation (User Input)
//!
//! Tools can request user input via forms using [`RequestContext::elicit_form()`]
//! or the convenience method [`RequestContext::confirm()`]:
//!
//! ```rust,ignore
//! use tower_mcp::{ToolBuilder, CallToolResult};
//! use tower_mcp::extract::Context;
//!
//! // Simple confirmation dialog
//! let delete_tool = ToolBuilder::new("delete")
//!     .description("Delete a file")
//!     .extractor_handler(|ctx: Context| async move {
//!         if !ctx.confirm("Are you sure you want to delete this file?").await? {
//!             return Ok(CallToolResult::text("Cancelled"));
//!         }
//!         // ... perform deletion ...
//!         Ok(CallToolResult::text("Deleted"))
//!     })
//!     .build();
//! ```
//!
//! For complex forms, use [`ElicitFormSchema`] to define multiple fields.
//!
//! ### Progress Notifications
//!
//! Long-running tools can report progress via [`RequestContext::report_progress()`]:
//!
//! ```rust,ignore
//! use tower_mcp::{ToolBuilder, CallToolResult};
//! use tower_mcp::extract::Context;
//!
//! let process_tool = ToolBuilder::new("process")
//!     .description("Process items")
//!     .extractor_handler(|ctx: Context| async move {
//!         let items = vec!["a", "b", "c", "d", "e"];
//!         let total = items.len() as f64;
//!
//!         for (i, item) in items.iter().enumerate() {
//!             ctx.report_progress(i as f64, Some(total), Some(&format!("Processing {}", item))).await;
//!             // ... process item ...
//!         }
//!
//!         Ok(CallToolResult::text("Done"))
//!     })
//!     .build();
//! ```
//!
//! ### Router Composition
//!
//! Combine multiple routers using [`McpRouter::merge()`] or [`McpRouter::nest()`]:
//!
//! ```rust,ignore
//! use tower_mcp::McpRouter;
//!
//! // Create domain-specific routers
//! let db_router = McpRouter::new()
//!     .tool(query_tool)
//!     .tool(insert_tool);
//!
//! let api_router = McpRouter::new()
//!     .tool(fetch_tool);
//!
//! // Nest with prefixes: tools become "db.query", "db.insert", "api.fetch"
//! let combined = McpRouter::new()
//!     .server_info("combined", "1.0")
//!     .nest("db", db_router)
//!     .nest("api", api_router);
//!
//! // Or merge without prefixes
//! let merged = McpRouter::new()
//!     .merge(db_router)
//!     .merge(api_router);
//! ```
//!
//! ## MCP Specification
//!
//! This crate implements the MCP specification (2025-11-25):
//! <https://modelcontextprotocol.io/specification/2025-11-25>

pub mod async_task;
pub mod auth;
pub mod client;
pub mod context;
pub mod error;
pub mod extract;
pub mod filter;
pub mod jsonrpc;
#[cfg(feature = "oauth")]
pub mod oauth;
pub mod prompt;
pub mod protocol;
#[cfg(feature = "dynamic-tools")]
pub mod registry;
pub mod resource;
pub mod router;
pub mod session;
#[cfg(feature = "testing")]
pub mod testing;
pub mod tool;
pub mod tracing_layer;
pub mod transport;

// Re-exports
pub use async_task::{Task, TaskStore};
pub use client::{ClientTransport, McpClient, StdioClientTransport};
pub use context::{
    ChannelClientRequester, ClientRequester, ClientRequesterHandle, Extensions,
    NotificationReceiver, NotificationSender, OutgoingRequest, OutgoingRequestReceiver,
    OutgoingRequestSender, RequestContext, RequestContextBuilder, ServerNotification,
    outgoing_request_channel,
};
pub use error::{BoxError, Error, Result, ResultExt, ToolError};
pub use filter::{
    CapabilityFilter, DenialBehavior, Filterable, PromptFilter, ResourceFilter, ToolFilter,
};
pub use jsonrpc::{JsonRpcLayer, JsonRpcService};
pub use prompt::{BoxPromptService, Prompt, PromptBuilder, PromptHandler, PromptRequest};
#[allow(deprecated)]
pub use protocol::{
    BooleanSchema, CallToolParams, CallToolResult, CancelTaskParams, CancelledParams,
    ClientCapabilities, ClientTasksCancelCapability, ClientTasksCapability,
    ClientTasksElicitationCapability, ClientTasksElicitationCreateCapability,
    ClientTasksListCapability, ClientTasksRequestsCapability, ClientTasksSamplingCapability,
    ClientTasksSamplingCreateMessageCapability, CompleteParams, CompleteResult, Completion,
    CompletionArgument, CompletionContext, CompletionReference, CompletionsCapability, Content,
    ContentAnnotations, ContentRole, CreateMessageParams, CreateMessageResult, CreateTaskResult,
    ElicitAction, ElicitFieldValue, ElicitFormParams, ElicitFormSchema, ElicitMode,
    ElicitRequestParams, ElicitResult, ElicitUrlParams, ElicitationCapability,
    ElicitationCompleteParams, ElicitationFormCapability, ElicitationUrlCapability, EmptyResult,
    GetPromptParams, GetPromptResult, GetPromptResultBuilder, GetTaskInfoParams,
    GetTaskResultParams, IconTheme, Implementation, IncludeContext, InitializeParams,
    InitializeResult, IntegerSchema, JsonRpcErrorResponse, JsonRpcMessage, JsonRpcNotification,
    JsonRpcRequest, JsonRpcResponse, JsonRpcResponseMessage, JsonRpcResultResponse,
    ListPromptsParams, ListPromptsResult, ListResourceTemplatesParams, ListResourceTemplatesResult,
    ListResourcesParams, ListResourcesResult, ListRootsParams, ListRootsResult, ListTasksParams,
    ListTasksResult, ListToolsParams, ListToolsResult, LogLevel, LoggingCapability,
    LoggingMessageParams, McpNotification, McpRequest, McpResponse, ModelHint, ModelPreferences,
    MultiSelectEnumItems, MultiSelectEnumSchema, NumberSchema, PrimitiveSchemaDefinition,
    ProgressParams, ProgressToken, PromptArgument, PromptDefinition, PromptMessage,
    PromptReference, PromptRole, PromptsCapability, ReadResourceParams, ReadResourceResult,
    RequestId, RequestMeta, ResourceContent, ResourceDefinition, ResourceReference,
    ResourceTemplateDefinition, ResourcesCapability, Root, RootsCapability, SamplingCapability,
    SamplingContent, SamplingContentOrArray, SamplingContextCapability, SamplingMessage,
    SamplingTool, SamplingToolsCapability, ServerCapabilities, SetLogLevelParams,
    SingleSelectEnumSchema, StringSchema, SubscribeResourceParams, TaskInfo, TaskObject,
    TaskRequestParams, TaskStatus, TaskStatusChangedParams, TaskStatusParams, TaskSupportMode,
    TasksCancelCapability, TasksCapability, TasksListCapability, TasksRequestsCapability,
    TasksToolsCallCapability, TasksToolsRequestsCapability, ToolAnnotations, ToolChoice,
    ToolDefinition, ToolExecution, ToolIcon, ToolsCapability, UnsubscribeResourceParams,
};
#[cfg(feature = "dynamic-tools")]
pub use registry::DynamicToolRegistry;
pub use resource::{
    BoxResourceService, Resource, ResourceBuilder, ResourceHandler, ResourceRequest,
    ResourceTemplate, ResourceTemplateBuilder, ResourceTemplateHandler,
};
pub use router::{McpRouter, RouterRequest, RouterResponse};
pub use session::{SessionPhase, SessionState};
pub use tool::{BoxToolService, GuardLayer, NoParams, Tool, ToolBuilder, ToolHandler, ToolRequest};
pub use tracing_layer::{McpTracingLayer, McpTracingService};
pub use transport::{
    BidirectionalStdioTransport, CatchError, GenericStdioTransport, StdioTransport,
    SyncStdioTransport,
};

#[cfg(feature = "http")]
pub use transport::HttpTransport;

#[cfg(feature = "websocket")]
pub use transport::WebSocketTransport;

#[cfg(any(feature = "http", feature = "websocket"))]
pub use transport::McpBoxService;

#[cfg(feature = "childproc")]
pub use transport::{ChildProcessConnection, ChildProcessTransport};

#[cfg(feature = "oauth")]
pub use oauth::{ScopeEnforcementLayer, ScopeEnforcementService};

#[cfg(feature = "jwks")]
pub use oauth::{JwksError, JwksValidator, JwksValidatorBuilder};

#[cfg(feature = "testing")]
pub use testing::TestClient;
