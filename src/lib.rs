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

pub mod error;
pub mod prompt;
pub mod protocol;
pub mod resource;
pub mod router;
pub mod session;
pub mod tool;
pub mod transport;

// Re-exports
pub use error::{Error, Result, ToolError};
pub use prompt::{Prompt, PromptBuilder, PromptHandler};
pub use protocol::{
    CallToolResult, GetPromptResult, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse,
    JsonRpcResponseMessage, McpRequest, McpResponse, PromptMessage, PromptRole, ReadResourceResult,
    ResourceContent,
};
pub use resource::{Resource, ResourceBuilder, ResourceHandler};
pub use router::{JsonRpcService, McpRouter};
pub use session::{SessionPhase, SessionState};
pub use tool::{Tool, ToolBuilder, ToolHandler};
pub use transport::{StdioTransport, SyncStdioTransport};

#[cfg(feature = "http")]
pub use transport::HttpTransport;
