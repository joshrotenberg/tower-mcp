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
//!         .build()?;
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
//! - `testing` - Test utilities ([`TestClient`]) for ergonomic MCP server testing
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
pub mod filter;
pub mod jsonrpc;
#[cfg(feature = "oauth")]
pub mod oauth;
pub mod prompt;
pub mod protocol;
pub mod resource;
pub mod router;
pub mod session;
#[cfg(feature = "testing")]
pub mod testing;
pub mod tool;
pub mod transport;

// Re-exports
pub use async_task::{Task, TaskStore};
pub use client::{ClientTransport, McpClient, StdioClientTransport};
pub use context::{
    ChannelClientRequester, ClientRequester, ClientRequesterHandle, NotificationReceiver,
    NotificationSender, OutgoingRequest, OutgoingRequestReceiver, OutgoingRequestSender,
    RequestContext, RequestContextBuilder, ServerNotification, outgoing_request_channel,
};
pub use error::{BoxError, Error, Result, ToolError};
pub use filter::{
    CapabilityFilter, DenialBehavior, Filterable, PromptFilter, ResourceFilter, ToolFilter,
};
pub use jsonrpc::{JsonRpcLayer, JsonRpcService};
pub use prompt::{Prompt, PromptBuilder, PromptHandler};
pub use protocol::{
    CallToolResult, CompleteParams, CompleteResult, Completion, CompletionArgument,
    CompletionReference, CompletionsCapability, Content, ContentRole, CreateMessageParams,
    CreateMessageResult, ElicitAction, ElicitFieldValue, ElicitFormParams, ElicitFormSchema,
    ElicitMode, ElicitRequestParams, ElicitResult, ElicitUrlParams, ElicitationCapability,
    ElicitationCompleteParams, GetPromptResult, GetPromptResultBuilder, IncludeContext,
    JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, JsonRpcResponseMessage, ListRootsParams,
    ListRootsResult, McpRequest, McpResponse, ModelHint, ModelPreferences,
    PrimitiveSchemaDefinition, PromptMessage, PromptReference, PromptRole, ReadResourceResult,
    ResourceContent, ResourceReference, Root, RootsCapability, SamplingCapability, SamplingContent,
    SamplingContentOrArray, SamplingMessage, SamplingTool, ToolChoice,
};
pub use resource::{
    Resource, ResourceBuilder, ResourceHandler, ResourceTemplate, ResourceTemplateBuilder,
    ResourceTemplateHandler,
};
pub use router::{Extensions, McpRouter, RouterRequest, RouterResponse};
pub use session::{SessionPhase, SessionState};
pub use tool::{BoxToolService, NoParams, Tool, ToolBuilder, ToolHandler, ToolRequest};
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
