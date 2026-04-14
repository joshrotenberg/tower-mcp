//! Lightweight MCP protocol and error types.
//!
//! This crate provides the core type definitions for the
//! [Model Context Protocol](https://modelcontextprotocol.io) (MCP) without
//! pulling in runtime dependencies like Tower, Tokio, or axum.
//!
//! Use this crate directly when you need MCP types for serialization,
//! code generation, or editor integrations. For a full MCP server/client
//! implementation, see [`tower-mcp`](https://docs.rs/tower-mcp).
//!
//! # Target Support
//!
//! `tower-mcp-types` compiles for `wasm32-unknown-unknown` out of the box.
//! This is covered by a CI job, so downstream WASM tools (browser clients,
//! edge runtimes, editor extensions) can depend on this crate directly.
//!
//! The parent [`tower-mcp`](https://docs.rs/tower-mcp) crate is not
//! WASM-compatible today; it pulls in Tokio networking features that don't
//! build for `wasm32`. See
//! [tower-mcp issue #777](https://github.com/joshrotenberg/tower-mcp/issues/777)
//! for progress on client-side WASM support in the full crate.

pub mod error;
pub mod protocol;

// Convenience re-exports
pub use error::{
    BoxError, Error, ErrorCode, JsonRpcError, McpErrorCode, Result, ResultExt, ToolError,
};
pub use protocol::{CallToolResult, Content, GetPromptResult, ReadResourceResult};
