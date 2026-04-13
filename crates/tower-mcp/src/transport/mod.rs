//! MCP transport implementations
//!
//! Provides different transport layers for MCP communication:
//! - `stdio` - Standard input/output for CLI usage
//! - `http` - Streamable HTTP transport (requires `http` feature)
//! - `websocket` - WebSocket transport for full-duplex communication (requires `websocket` feature)
//! - `childproc` - Child process transport for subprocess MCP servers (requires `childproc` feature)

pub mod stdio;

#[cfg(feature = "http")]
pub mod http;

#[cfg(feature = "websocket")]
pub mod websocket;

#[cfg(all(unix, feature = "unix"))]
pub mod unix;

#[cfg(feature = "childproc")]
pub mod childproc;

pub mod service;

pub use service::{CatchError, InjectAnnotations};
pub use stdio::{
    BidirectionalStdioTransport, GenericStdioTransport, StdioTransport, SyncStdioTransport,
};

#[cfg(feature = "http")]
pub use http::{HttpTransport, SessionHandle, SessionInfo};

#[cfg(feature = "websocket")]
pub use websocket::WebSocketTransport;

#[cfg(all(unix, feature = "unix"))]
pub use unix::UnixSocketTransport;

#[cfg(feature = "childproc")]
pub use childproc::{ChildProcessConnection, ChildProcessTransport};

#[cfg(any(feature = "http", feature = "websocket", feature = "unix"))]
pub use service::McpBoxService;
