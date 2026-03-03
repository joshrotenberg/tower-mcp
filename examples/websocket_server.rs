//! WebSocket server example for tower-mcp
//!
//! Demonstrates the WebSocket transport for full-duplex MCP communication.
//! WebSocket connections support session management, tower middleware layers,
//! and optional server-to-client sampling.
//!
//! Run with: cargo run --example websocket_server --features websocket
//!
//! Connect with any WebSocket client (e.g., websocat):
//! ```bash
//! websocat ws://127.0.0.1:3000/
//! ```
//!
//! Then send JSON-RPC messages:
//! ```json
//! {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"websocat","version":"1.0"}}}
//! {"jsonrpc":"2.0","id":2,"method":"tools/list"}
//! {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"message":"hello"}}}
//! ```

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tower::timeout::TimeoutLayer;
use tower_mcp::transport::WebSocketTransport;
use tower_mcp::{CallToolResult, McpRouter, ResourceBuilder, ToolBuilder};

#[derive(Debug, Deserialize, JsonSchema)]
struct EchoInput {
    /// Message to echo back
    message: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tower_mcp=debug".parse()?)
                .add_directive("websocket_server=debug".parse()?),
        )
        .init();

    let echo = ToolBuilder::new("echo")
        .description("Echo a message back")
        .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.message)) })
        .build();

    let add = ToolBuilder::new("add")
        .description("Add two numbers together")
        .read_only()
        .idempotent()
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    let status = ResourceBuilder::new("app:///status")
        .name("Server Status")
        .description("Current server status")
        .text("Running (WebSocket transport)");

    let router = McpRouter::new()
        .server_info("websocket-example", "1.0.0")
        .instructions("A WebSocket MCP server. Connect via ws://127.0.0.1:3000/")
        .tool(echo)
        .tool(add)
        .resource(status);

    let transport =
        WebSocketTransport::new(router).layer(TimeoutLayer::new(Duration::from_secs(30)));

    tracing::info!("Starting WebSocket MCP server on ws://127.0.0.1:3000");
    transport.serve("127.0.0.1:3000").await?;

    Ok(())
}
