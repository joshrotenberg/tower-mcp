//! Lightweight MCP protocol and error types.
//!
//! This crate provides the core type definitions for the
//! [Model Context Protocol](https://modelcontextprotocol.io) (MCP) without
//! pulling in runtime dependencies like Tower, Tokio, or axum.
//!
//! Use this crate directly when you need MCP types for serialization,
//! code generation, or editor integrations. For a full MCP server/client
//! implementation, see [`tower-mcp`](https://docs.rs/tower-mcp).

pub mod error;
pub mod protocol;

// Convenience re-exports
pub use error::{
    BoxError, Error, ErrorCode, JsonRpcError, McpErrorCode, Result, ResultExt, ToolError,
};
pub use protocol::{CallToolResult, Content, GetPromptResult, ReadResourceResult};
