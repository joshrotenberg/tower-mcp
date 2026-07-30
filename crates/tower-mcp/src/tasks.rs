//! Final Tasks extension wire types (re-exported from [`tower-mcp-types`](https://docs.rs/tower-mcp-types)).
//!
//! These types follow the `io.modelcontextprotocol/tasks` extension defined by
//! [SEP-2663](https://modelcontextprotocol.io/seps/2663-tasks-extension) for the
//! 2026-07-28 protocol version. They are distinct from the legacy 2025-11-25
//! task surface in [`crate::async_task`], which is not wire-compatible.

pub use tower_mcp_types::tasks::*;
