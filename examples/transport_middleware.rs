//! Transport-level middleware example
//!
//! Demonstrates applying tower middleware at the MCP protocol level on both
//! HTTP and stdio transports. Middleware errors (like timeouts) are automatically
//! converted into JSON-RPC error responses.
//!
//! The two key patterns:
//! - HTTP: `HttpTransport::new(router).layer(middleware)` handles error
//!   conversion internally
//! - Stdio: `CatchError::new(middleware.service(router))` + `GenericStdioTransport`
//!   requires explicit `CatchError` wrapping to preserve `Error = Infallible`
//!
//! Run with:
//!   HTTP:  cargo run --example transport_middleware --features http -- --transport http
//!   Stdio: cargo run --example transport_middleware -- --transport stdio
//!
//! Test the HTTP timeout:
//! ```bash
//! # Initialize session
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -H "Accept: application/json, text/event-stream" \
//!   -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"1.0"}}}'
//!
//! # This will timeout (sleeps 5s, timeout is 2s)
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -H "MCP-Session-Id: <session-id>" \
//!   -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"slow","arguments":{"seconds":5}}}'
//! ```

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tower::ServiceBuilder;
use tower::timeout::TimeoutLayer;
use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

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

fn build_router() -> McpRouter {
    let add = ToolBuilder::new("add")
        .description("Add two numbers together")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    let slow = ToolBuilder::new("slow")
        .description("Sleep for a specified number of seconds, then respond")
        .handler(|input: SlowInput| async move {
            tokio::time::sleep(Duration::from_secs(input.seconds)).await;
            Ok(CallToolResult::text(format!(
                "Slept for {} seconds",
                input.seconds
            )))
        })
        .build();

    McpRouter::new()
        .server_info("middleware-example", "1.0.0")
        .instructions("An MCP server demonstrating tower middleware layers.")
        .tool(add)
        .tool(slow)
}

/// Serve over HTTP with `.layer()` on the transport.
///
/// HttpTransport handles error conversion internally -- middleware errors
/// (like timeouts) become JSON-RPC error responses automatically.
#[cfg(feature = "http")]
async fn serve_http(router: McpRouter) -> Result<(), tower_mcp::BoxError> {
    use tower_mcp::HttpTransport;

    let transport = HttpTransport::new(router)
        .disable_origin_validation()
        .layer(
            ServiceBuilder::new()
                .layer(TimeoutLayer::new(Duration::from_secs(2)))
                .concurrency_limit(10)
                .into_inner(),
        );

    tracing::info!("Starting HTTP MCP server with middleware on http://127.0.0.1:3000");
    tracing::info!("Try the 'slow' tool with seconds > 2 to see the timeout");
    transport.serve("127.0.0.1:3000").await?;

    Ok(())
}

/// Serve over stdio with explicit CatchError wrapping.
///
/// GenericStdioTransport requires `Error = Infallible`, but middleware like
/// TimeoutLayer introduces its own error type. CatchError converts these
/// middleware errors into JSON-RPC error responses to satisfy the contract.
///
/// Layer ordering in ServiceBuilder:
///   .layer(OuterLayer)  -- runs first on request, last on response
///   .layer(InnerLayer)  -- runs last on request, first on response
///   .service(router)    -- the inner service
async fn serve_stdio(router: McpRouter) -> Result<(), tower_mcp::BoxError> {
    use tower_mcp::{CatchError, GenericStdioTransport};

    let service = CatchError::new(
        ServiceBuilder::new()
            .layer(TimeoutLayer::new(Duration::from_secs(5)))
            .concurrency_limit(10)
            .service(router),
    );

    let mut transport = GenericStdioTransport::new(service);

    tracing::info!("Starting stdio server with middleware");
    transport.run().await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    let transport = std::env::args()
        .nth(1)
        .or_else(|| {
            std::env::args().find_map(|a| {
                a.strip_prefix("--transport=")
                    .or_else(|| a.strip_prefix("--transport "))
                    .map(String::from)
            })
        })
        .or_else(|| {
            let mut args = std::env::args();
            while let Some(arg) = args.next() {
                if arg == "--transport" {
                    return args.next();
                }
            }
            None
        })
        .unwrap_or_else(|| "stdio".to_string());

    match transport.as_str() {
        "stdio" => {
            tracing_subscriber::fmt()
                .with_env_filter("tower_mcp=debug")
                .with_writer(std::io::stderr)
                .init();
            serve_stdio(build_router()).await
        }
        #[cfg(feature = "http")]
        "http" => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::from_default_env()
                        .add_directive("tower_mcp=debug".parse()?),
                )
                .init();
            serve_http(build_router()).await
        }
        #[cfg(not(feature = "http"))]
        "http" => {
            eprintln!(
                "HTTP transport requires the 'http' feature: cargo run --example transport_middleware --features http -- --transport http"
            );
            std::process::exit(1);
        }
        other => {
            eprintln!("Unknown transport: {other}. Use 'stdio' or 'http'.");
            std::process::exit(1);
        }
    }
}
