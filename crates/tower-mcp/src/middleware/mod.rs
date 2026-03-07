//! MCP-specific tower middleware layers.
//!
//! This module provides middleware layers designed for MCP request processing.
//! All layers implement [`tower::Layer`] and work with
//! [`RouterRequest`](crate::router::RouterRequest) /
//! [`RouterResponse`](crate::router::RouterResponse) types.
//!
//! # Available Layers
//!
//! | Layer | Purpose |
//! |-------|---------|
//! | [`McpTracingLayer`] | Structured tracing for all MCP requests |
//! | [`ToolCallLoggingLayer`] | Focused audit logging for tool calls |
//! | [`AuditLayer`] | Comprehensive audit events for all MCP requests |
//!
//! # Usage
//!
//! Apply at the transport level for all requests:
//!
//! ```rust,ignore
//! use tower::ServiceBuilder;
//! use tower_mcp::middleware::{AuditLayer, McpTracingLayer, ToolCallLoggingLayer};
//!
//! let transport = StdioTransport::new(router)
//!     .layer(
//!         ServiceBuilder::new()
//!             .layer(McpTracingLayer::new())
//!             .layer(AuditLayer::new())
//!             .into_inner(),
//!     );
//! ```

mod audit;
mod tool_call_logging;
mod tracing;

pub use audit::{AuditLayer, AuditService};
pub use tool_call_logging::{ToolCallLoggingLayer, ToolCallLoggingService};
pub use tracing::{McpTracingLayer, McpTracingService};
