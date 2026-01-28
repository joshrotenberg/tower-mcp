//! HTTP server example with tower middleware layers
//!
//! Demonstrates applying tower middleware to MCP request processing using
//! `.layer()`. This operates at the MCP protocol level, not the HTTP level.
//!
//! For HTTP-level middleware (auth, CORS, etc.), see http_server_with_auth.rs.
//!
//! Run with: cargo run --example http_server_with_middleware --features http
//!
//! Test with curl:
//! ```bash
//! # Initialize session
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -H "Accept: application/json, text/event-stream" \
//!   -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"curl","version":"1.0"}}}'
//!
//! # Call the slow tool (will timeout after 2 seconds)
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -H "MCP-Session-Id: <session-id>" \
//!   -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"slow","arguments":{"seconds":5}}}'
//!
//! # Call the fast tool (succeeds normally)
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -H "MCP-Session-Id: <session-id>" \
//!   -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"add","arguments":{"a":10,"b":32}}}'
//! ```

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tower::ServiceBuilder;
use tower::timeout::TimeoutLayer;
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SlowInput {
    /// How many seconds to sleep before responding
    seconds: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tower_mcp=debug".parse()?)
                .add_directive("http_server_with_middleware=debug".parse()?),
        )
        .init();

    let add = ToolBuilder::new("add")
        .description("Add two numbers together")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build()?;

    let slow = ToolBuilder::new("slow")
        .description("Sleep for a specified number of seconds, then respond")
        .handler(|input: SlowInput| async move {
            tokio::time::sleep(Duration::from_secs(input.seconds)).await;
            Ok(CallToolResult::text(format!(
                "Slept for {} seconds",
                input.seconds
            )))
        })
        .build()?;

    let router = McpRouter::new()
        .server_info("middleware-example", "1.0.0")
        .instructions("An MCP server demonstrating tower middleware layers.")
        .tool(add)
        .tool(slow);

    // Apply tower middleware at the MCP request level via .layer().
    //
    // This is distinct from axum middleware on into_router():
    //   - .layer() wraps the McpRouter Service<RouterRequest> pipeline
    //   - axum middleware wraps the HTTP request/response pipeline
    //
    // Middleware errors (like timeouts) are automatically converted into
    // JSON-RPC error responses.
    let transport = HttpTransport::new(router)
        .disable_origin_validation()
        .layer(
            ServiceBuilder::new()
                // Timeout: fail MCP requests that take longer than 2 seconds
                .layer(TimeoutLayer::new(Duration::from_secs(2)))
                // Concurrency: process at most 10 MCP requests at a time
                .concurrency_limit(10)
                .into_inner(),
        );

    tracing::info!("Starting HTTP MCP server with middleware on http://127.0.0.1:3000");
    tracing::info!("Try the 'slow' tool with seconds > 2 to see the timeout in action");
    transport.serve("127.0.0.1:3000").await?;

    Ok(())
}
