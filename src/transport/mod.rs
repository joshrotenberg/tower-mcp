//! MCP transport implementations
//!
//! Provides different transport layers for MCP communication:
//! - `stdio` - Standard input/output for CLI usage
//! - `http` - Streamable HTTP transport (requires `http` feature)
//! - `websocket` - WebSocket transport for full-duplex communication (requires `websocket` feature)
//! - `childproc` - Child process transport for subprocess MCP servers (requires `childproc` feature)
//!
//! ## Synchronization and Thread Safety
//!
//! All transports are designed to be safe for concurrent use:
//!
//! - **Session storage**: Uses `RwLock` for safe concurrent access
//! - **SSE notifications**: Uses `broadcast` channels which deliver to all receivers
//! - **Request IDs**: Uses `AtomicI64` for lock-free incrementing
//!
//! We deliberately avoid `tokio::sync::Notify` patterns that can miss signals
//! if `notify_one()` is called before a task starts waiting. This prevents
//! race conditions in serverless/Lambda cold-start scenarios.
//!
//! ## Serverless Considerations
//!
//! For serverless environments (AWS Lambda, Cloudflare Workers, etc.):
//!
//! - **HTTP transport**: Each request is handled independently; session state
//!   is stored in memory and will be lost on cold starts. Consider using
//!   external session storage for persistence across invocations.
//!
//! - **Stateless mode**: For truly stateless operation, see issue #51 which
//!   tracks SEP-1442 (Make MCP Stateless) once the spec is finalized.
//!
//! - **Cold starts**: Our synchronization primitives don't have timing-dependent
//!   race conditions, so cold starts should be reliable.

pub mod stdio;

#[cfg(feature = "http")]
pub mod http;

#[cfg(feature = "websocket")]
pub mod websocket;

#[cfg(feature = "childproc")]
pub mod childproc;

pub use stdio::{StdioTransport, SyncStdioTransport};

#[cfg(feature = "http")]
pub use http::HttpTransport;

#[cfg(feature = "websocket")]
pub use websocket::WebSocketTransport;

#[cfg(feature = "childproc")]
pub use childproc::{ChildProcessConnection, ChildProcessTransport};
