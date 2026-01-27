//! MCP transport implementations
//!
//! Provides different transport layers for MCP communication:
//! - `stdio` - Standard input/output for CLI usage
//! - `http` - Streamable HTTP transport (requires `http` feature)

pub mod stdio;

#[cfg(feature = "http")]
pub mod http;

pub use stdio::{StdioTransport, SyncStdioTransport};

#[cfg(feature = "http")]
pub use http::HttpTransport;
