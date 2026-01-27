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
//! ```rust,ignore
//! use tower_mcp::{McpRouter, ToolBuilder, CallToolResult};
//! use tower::ServiceBuilder;
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
//!         // implementation
//!         Ok(CallToolResult::text("result"))
//!     })
//!     .build();
//!
//! // Create router with tools
//! let router = McpRouter::new()
//!     .server_info("my-server", "1.0.0")
//!     .tool(evaluate);
//!
//! // Add middleware via tower's ServiceBuilder
//! let service = ServiceBuilder::new()
//!     .service(router);
//! ```

pub mod error;
pub mod protocol;
pub mod router;
pub mod session;
pub mod tool;

// Re-exports
pub use error::{Error, Result};
pub use protocol::{
    CallToolResult, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, JsonRpcResponseMessage,
    McpRequest, McpResponse,
};
pub use router::{JsonRpcService, McpRouter};
pub use session::{SessionPhase, SessionState};
pub use tool::{Tool, ToolBuilder, ToolHandler};
