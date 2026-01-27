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
//! use tower_mcp::{McpRouter, Tool, ToolBuilder};
//! use tower::ServiceBuilder;
//!
//! // Define a tool using the builder
//! let evaluate = ToolBuilder::new("evaluate")
//!     .description("Evaluate a JMESPath expression")
//!     .input::<EvaluateInput>()
//!     .handler(|input: EvaluateInput| async move {
//!         // implementation
//!         Ok(result)
//!     })
//!     .build();
//!
//! // Create router with tools
//! let router = McpRouter::new()
//!     .tool(evaluate)
//!     .tool(validate);
//!
//! // Add middleware
//! let service = ServiceBuilder::new()
//!     .layer(TracingLayer::new())
//!     .layer(MetricsLayer::new())
//!     .service(router);
//!
//! // Serve over any transport
//! StdioTransport::serve(service).await;
//! ```

pub mod error;
pub mod protocol;
pub mod router;
pub mod tool;

// Re-exports
pub use error::{Error, Result};
pub use protocol::{JsonRpcRequest, JsonRpcResponse, McpRequest, McpResponse};
pub use router::McpRouter;
pub use tool::{Tool, ToolBuilder, ToolHandler};
