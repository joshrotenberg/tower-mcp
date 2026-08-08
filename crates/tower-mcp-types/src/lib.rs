//! Standalone MCP protocol and error types -- zero runtime dependencies.
//!
//! `tower-mcp-types` ships [Model Context Protocol](https://modelcontextprotocol.io)
//! types and JSON-RPC 2.0 primitives as a self-contained crate. It has **no
//! dependency on Tower, Tokio, axum, or any async
//! runtime** -- only `serde`, `serde_json`, `thiserror`, and `base64`.
//!
//! This makes it suitable for:
//!
//! - WASM targets (browser clients, edge runtimes, editor extensions)
//! - Code generation tools that need to inspect or emit MCP messages
//! - Thin clients and proxies that only need to parse/serialize MCP wire
//!   format without running a full server
//! - Testing libraries that want MCP types without pulling in a server stack
//!
//! The [`inspection`] module provides strict, transport-free classification of
//! JSON-RPC requests, notifications, success responses, error responses, and
//! batches for intermediaries that observe both sides of a connection. Its
//! [`inspection::McpInspector`] layers exact-revision MCP method, direction,
//! batching, and typed parameter checks on top without selecting a revision
//! implicitly or changing an application's runtime allowlist.
//!
//! # Stateless protocol support (2026-07-28)
//!
//! This crate also represents the released `2026-07-28` protocol, which adds
//! stateless operation, per-request `_meta`, and the `server/discover` RPC.
//! Key items:
//!
//! - [`protocol::PROTOCOL_VERSION_2026_07_28`] -- the canonical
//!   `"2026-07-28"` wire-version constant. Runtime enablement is a separate
//!   concern in the parent `tower-mcp` crate.
//! - [`protocol::KNOWN_PROTOCOL_VERSIONS`] -- every version whose wire format
//!   is represented here, independent of runtime compile-time features.
//! - [`protocol::DiscoverResult`] / [`protocol::DiscoverParams`] -- types for
//!   the `server/discover` RPC (SEP-2575), which returns the server's supported
//!   protocol versions and capabilities without requiring an `initialize`
//!   handshake.
//! - [`error::UnsupportedProtocolVersionData`] -- the structured error data
//!   attached to a `-32022` (UnsupportedProtocolVersion) response. Contains
//!   the `supported` versions list and the `requested` version string.
//! - [`error::McpErrorCode::HeaderMismatch`] (`-32020`) and
//!   [`error::McpErrorCode::UnsupportedProtocolVersion`] (`-32022`) --
//!   spec-assigned error codes (SEP-2243 and SEP-2575, renumbered by the
//!   upstream error-code allocation in spec PR modelcontextprotocol#2907).
//!   See the [`error`] module for the full error code table.
//!
//! # The only standalone Rust MCP types crate
//!
//! Most Rust MCP libraries bundle their protocol types with their runtime.
//! `tower-mcp-types` ships independently so that tools with narrower
//! requirements -- WASM, codegen, linters, CLI utilities -- can take a
//! dependency without pulling in an async server stack. The
//! [Zed editor team raised exactly this concern](https://github.com/modelcontextprotocol/rust-sdk/issues/449)
//! for the official `rmcp` crate, which has not yet split types from runtime.
//!
//! # Feature flags
//!
//! | Feature | What it adds |
//! |---------|-------------|
//! | *(default: none)* | Core protocol types + serde serialization |
//! | `testing` | Wire-format assertion helpers for JSON-RPC test cases |
//!
//! # Usage
//!
//! Add to `Cargo.toml`:
//!
//! ```toml
//! [dependencies]
//! tower-mcp-types = "0.21"
//! ```
//!
//! Parse an incoming JSON-RPC request:
//!
//! ```rust
//! use tower_mcp_types::protocol::JsonRpcRequest;
//!
//! let json = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":null}"#;
//! let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
//! assert_eq!(req.method, "tools/list");
//! ```
//!
//! Construct a JSON-RPC error response:
//!
//! ```rust
//! use tower_mcp_types::error::{ErrorCode, JsonRpcError};
//!
//! let err = JsonRpcError::new(ErrorCode::MethodNotFound, "tools/call not found");
//! assert_eq!(err.code, -32601);
//! let serialized = serde_json::to_string(&err).unwrap();
//! assert!(serialized.contains("-32601"));
//! ```
//!
//! # Compatibility with `tower-mcp`
//!
//! [`tower-mcp`](https://docs.rs/tower-mcp) re-exports everything from this
//! crate, so migrating from `tower-mcp-types` to the full server library
//! requires only a dependency change -- no import paths need to change:
//!
//! ```toml
//! tower-mcp = { version = "0.21", features = ["http"] }
//! ```
//!
//! # WASM support
//!
//! `tower-mcp-types` compiles for `wasm32-unknown-unknown` out of the box.
//! This is verified by a CI job on every commit.
//!
//! The parent [`tower-mcp`](https://docs.rs/tower-mcp) crate is not
//! WASM-compatible today; it pulls in Tokio networking features that do not
//! build for `wasm32`. See
//! [tower-mcp issue #777](https://github.com/joshrotenberg/tower-mcp/issues/777)
//! for progress on client-side WASM support in the full crate.

pub mod error;
pub mod inspection;
pub mod protocol;
pub mod tasks;

#[cfg(feature = "testing")]
pub mod testing;

// Convenience re-exports
pub use error::{
    BoxError, Error, ErrorCode, JsonRpcError, McpErrorCode, MissingRequiredClientCapabilityData,
    Result, ResultExt, ToolError,
};
pub use protocol::{CallToolResult, Content, GetPromptResult, ReadResourceResult};
