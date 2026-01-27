//! MCP transport implementations
//!
//! Provides different transport layers for MCP communication:
//! - `stdio` - Standard input/output for CLI usage

pub mod stdio;

pub use stdio::{StdioTransport, SyncStdioTransport};
