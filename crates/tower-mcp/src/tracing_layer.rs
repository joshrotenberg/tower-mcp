//! MCP request tracing middleware.
//!
//! This module re-exports from [`crate::middleware`] for backward
//! compatibility. The canonical location is [`crate::middleware`].

#[doc(inline)]
pub use crate::middleware::{McpTracingLayer, McpTracingService};
