//! Stdio server example with tower middleware
//!
//! Demonstrates applying standard tower middleware to MCP request processing
//! using `GenericStdioTransport` with `ServiceBuilder`. This is the purest
//! tower composition pattern -- no HTTP or axum involved.
//!
//! Run with: cargo run --example stdio_server_with_middleware
//!
//! Test with:
//! ```bash
//! echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}' \
//!   | cargo run --example stdio_server_with_middleware
//! ```

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tower::ServiceBuilder;
use tower::timeout::TimeoutLayer;
use tower_mcp::{CallToolResult, CatchError, GenericStdioTransport, McpRouter, ToolBuilder};

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    /// First number
    a: i64,
    /// Second number
    b: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SlowInput {
    /// How many seconds to sleep before responding
    seconds: u64,
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    // Initialize tracing (to stderr so it doesn't interfere with JSON-RPC on stdout)
    tracing_subscriber::fmt()
        .with_env_filter("tower_mcp=debug")
        .with_writer(std::io::stderr)
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
        .server_info("middleware-stdio-example", "1.0.0")
        .instructions("A stdio MCP server demonstrating tower middleware layers.")
        .tool(add)
        .tool(slow);

    // Compose tower middleware with ServiceBuilder, then wrap in CatchError
    // to convert middleware errors (like timeouts) into JSON-RPC error responses.
    //
    // Layer ordering in ServiceBuilder:
    //   .layer(OuterLayer)    // runs first on request, last on response
    //   .layer(InnerLayer)    // runs last on request, first on response
    //   .service(router)      // the inner service
    //
    // CatchError preserves the Error = Infallible contract that
    // GenericStdioTransport requires.
    let service = CatchError::new(
        ServiceBuilder::new()
            // Timeout: fail MCP requests that take longer than 5 seconds
            .layer(TimeoutLayer::new(Duration::from_secs(5)))
            // Concurrency: process at most 10 MCP requests at a time
            .concurrency_limit(10)
            .service(router),
    );

    let mut transport = GenericStdioTransport::new(service);

    tracing::info!("Starting stdio server with middleware");
    transport.run().await?;

    Ok(())
}
