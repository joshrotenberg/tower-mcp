//! # tower-mcp
//!
//! Tower-native Model Context Protocol (MCP) implementation.
//!
//! This crate provides a composable, middleware-friendly approach to building
//! MCP servers and clients using the Tower service abstraction.
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
//! ## Example
//!
//! ```rust
//! use tower_mcp::{McpRouter, ToolBuilder, CallToolResult};
//! use schemars::JsonSchema;
//! use serde::Deserialize;
//!
//! #[derive(Debug, Deserialize, JsonSchema)]
//! struct EvaluateInput {
//!     expression: String,
//!     data: serde_json::Value,
//! }
//!
//! // Define a tool using the builder - type is inferred from handler
//! let evaluate = ToolBuilder::new("evaluate")
//!     .description("Evaluate a JMESPath expression")
//!     .handler(|input: EvaluateInput| async move {
//!         Ok(CallToolResult::text(format!("Evaluated: {}", input.expression)))
//!     })
//!     .build()
//!     .expect("valid tool name");
//!
//! // Create router with tools
//! let router = McpRouter::new()
//!     .server_info("my-server", "1.0.0")
//!     .tool(evaluate);
//! ```

pub mod async_task;
pub mod auth;
pub mod client;
pub mod context;
pub mod error;
pub mod jsonrpc;
pub mod prompt;
pub mod protocol;
pub mod resource;
pub mod router;
pub mod session;
pub mod tool;
pub mod transport;

// Re-exports
pub use async_task::{Task, TaskStore};
pub use client::{ClientTransport, McpClient, StdioClientTransport};
pub use context::{
    NotificationReceiver, NotificationSender, RequestContext, RequestContextBuilder,
    ServerNotification,
};
pub use error::{Error, Result, ToolError};
pub use jsonrpc::JsonRpcService;
pub use prompt::{Prompt, PromptBuilder, PromptHandler};
pub use protocol::{
    CallToolResult, CompleteParams, CompleteResult, Completion, CompletionArgument,
    CompletionReference, CompletionsCapability, ContentRole, CreateMessageParams,
    CreateMessageResult, ElicitAction, ElicitFieldValue, ElicitFormParams, ElicitFormSchema,
    ElicitMode, ElicitRequestParams, ElicitResult, ElicitUrlParams, ElicitationCapability,
    ElicitationCompleteParams, GetPromptResult, IncludeContext, JsonRpcMessage, JsonRpcRequest,
    JsonRpcResponse, JsonRpcResponseMessage, ListRootsParams, ListRootsResult, McpRequest,
    McpResponse, ModelHint, ModelPreferences, PrimitiveSchemaDefinition, PromptMessage,
    PromptReference, PromptRole, ReadResourceResult, ResourceContent, ResourceReference, Root,
    RootsCapability, SamplingCapability, SamplingContent, SamplingMessage,
};
pub use resource::{
    Resource, ResourceBuilder, ResourceHandler, ResourceTemplate, ResourceTemplateBuilder,
    ResourceTemplateHandler,
};
pub use router::McpRouter;
pub use session::{SessionPhase, SessionState};
pub use tool::{Tool, ToolBuilder, ToolHandler};
pub use transport::{StdioTransport, SyncStdioTransport};

#[cfg(feature = "http")]
pub use transport::HttpTransport;

#[cfg(feature = "websocket")]
pub use transport::WebSocketTransport;

#[cfg(feature = "childproc")]
pub use transport::{ChildProcessConnection, ChildProcessTransport};
