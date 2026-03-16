//! Rate limiting example
//!
//! Demonstrates rate limiting and concurrency control using standard Tower middleware.
//! This is a key tower-mcp differentiator: any Tower layer works without custom abstractions.
//!
//! This example shows:
//! - Per-tool concurrency limits: control how many concurrent calls each tool allows
//! - Per-tool timeouts: prevent slow tools from blocking
//! - Transport-level rate limiting: global limit across all MCP requests
//! - Combining multiple layers on a single tool
//!
//! Note: `RateLimitLayer` works at the transport level. For per-tool limits,
//! use `ConcurrencyLimitLayer` (which controls how many calls run simultaneously)
//! combined with `TimeoutLayer` (which bounds how long each call can take).
//!
//! Run with: cargo run --example rate_limiting
//!
//! Test interactively or with an MCP client connected via stdio.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tower::limit::ConcurrencyLimitLayer;
use tower::timeout::TimeoutLayer;
use tower_mcp::{BoxError, CallToolResult, McpRouter, StdioTransport, ToolBuilder};

#[derive(Debug, Deserialize, JsonSchema)]
struct QueryInput {
    /// The query to execute
    query: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SendInput {
    /// Recipient
    to: String,
    /// Message body
    message: String,
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    eprintln!("Starting rate limiting example...");

    // Tool 1: API search with concurrency limit
    // Only 10 concurrent calls allowed -- protects an external API
    let search_api = ToolBuilder::new("search_api")
        .description("Search an external API. Limited to 10 concurrent calls.")
        .handler(|input: QueryInput| async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(CallToolResult::text(format!(
                "Search results for: {}",
                input.query
            )))
        })
        .layer(ConcurrencyLimitLayer::new(10))
        .build();

    // Tool 2: Email sending with strict concurrency limit
    // Only 1 at a time -- serializes sends to prevent spam
    let send_email = ToolBuilder::new("send_email")
        .description("Send an email. Serialized: only 1 send at a time.")
        .handler(|input: SendInput| async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            Ok(CallToolResult::text(format!(
                "Email sent to {}: {}",
                input.to,
                input.message.chars().take(50).collect::<String>()
            )))
        })
        .layer(ConcurrencyLimitLayer::new(1))
        .build();

    // Tool 3: Database query with combined middleware
    // Concurrency limit + timeout -- full production stack
    let call_count = Arc::new(AtomicU32::new(0));
    let counter = call_count.clone();

    let db_query = ToolBuilder::new("db_query")
        .description(
            "Execute a database query. Max 3 concurrent, 10s timeout. \
             Returns call count for observability.",
        )
        .handler(move |input: QueryInput| {
            let counter = counter.clone();
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok(CallToolResult::text(format!(
                    "Query #{} result for: {}",
                    n, input.query
                )))
            }
        })
        // Layers wrap outside-in: request hits concurrency limit -> timeout -> handler
        .layer(ConcurrencyLimitLayer::new(3))
        .layer(TimeoutLayer::new(Duration::from_secs(10)))
        .build();

    // Tool 4: Unrestricted tool for comparison
    let echo = ToolBuilder::new("echo")
        .description("Echo back the input. No middleware layers.")
        .handler(|input: QueryInput| async move {
            Ok(CallToolResult::text(format!("Echo: {}", input.query)))
        })
        .build();

    let router = McpRouter::new()
        .server_info("rate-limiting-example", "1.0.0")
        .instructions(
            "This server demonstrates rate limiting with Tower middleware. Try:\n\
             - search_api: max 10 concurrent calls\n\
             - send_email: serialized (1 at a time)\n\
             - db_query: max 3 concurrent + 10s timeout\n\
             - echo: no limits (for comparison)\n\n\
             Exceeding a concurrency limit causes the request to wait.",
        )
        .tool(search_api)
        .tool(send_email)
        .tool(db_query)
        .tool(echo);

    eprintln!("Server ready. Connect with an MCP client via stdio.");
    StdioTransport::new(router).run().await?;

    Ok(())
}
