//! Validate captured MCP wire frames against the published JSON schemas.
//!
//! `conformance-server` writes every JSON-RPC frame it reads or writes to a
//! JSONL file when `TOWER_MCP_FRAME_LOG` is set. This crate replays that log:
//! each frame is mapped to a schema definition by its method and direction and
//! validated against the schema vendored for the frame's protocol version.

pub mod baseline;
pub mod check;
pub mod methods;
pub mod schemas;
