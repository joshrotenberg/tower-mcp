//! Rate limiting example
//!
//! Demonstrates rate limiting and concurrency control using Tower middleware.
//! This is a key tower-mcp differentiator: any Tower layer works without custom abstractions.
//!
//! This example shows:
//! - Per-tool rate limiting: control requests per second/minute for each tool
//! - Per-tool concurrency limits: control how many concurrent calls each tool allows
//! - Combining rate limits with timeouts and concurrency limits
//!
//! Uses `tower-resilience`'s `RateLimiterLayer` which implements `Clone` (via `Arc`),
//! making it compatible with tower-mcp's per-tool `.layer()`. Tower's built-in
//! `RateLimitLayer` does not implement `Clone`, so it only works at the transport level.
//!
//! Run with: cargo run --example rate_limiting
//!
//! Test interactively or with an MCP client connected via stdio.

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tower::limit::ConcurrencyLimitLayer;
use tower::timeout::TimeoutLayer;
use tower_mcp::{BoxError, CallToolResult, McpRouter, StdioTransport, ToolBuilder};
use tower_resilience::ratelimiter::RateLimiterLayer;

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

    // Tool 1: API search with rate limit
    // Max 10 requests per second -- protects an external API quota
    let search_api = ToolBuilder::new("search_api")
        .description("Search an external API. Rate limited to 10 requests/second.")
        .handler(|input: QueryInput| async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(CallToolResult::text(format!(
                "Search results for: {}",
                input.query
            )))
        })
        .layer(RateLimiterLayer::per_second(10).build())
        .build();

    // Tool 2: Email sending with strict rate limit
    // Max 2 per minute -- prevents spam
    let send_email = ToolBuilder::new("send_email")
        .description("Send an email. Strictly rate limited to 2 per minute.")
        .handler(|input: SendInput| async move {
            Ok(CallToolResult::text(format!(
                "Email sent to {}: {}",
                input.to,
                input.message.chars().take(50).collect::<String>()
            )))
        })
        .layer(RateLimiterLayer::per_minute(2).build())
        .build();

    // Tool 3: Database query with combined middleware
    // Rate limit + concurrency limit + timeout -- full production stack
    let db_query = ToolBuilder::new("db_query")
        .description(
            "Execute a database query. Rate limited (5/sec), \
             max 3 concurrent, 10s timeout.",
        )
        .handler(|input: QueryInput| async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(CallToolResult::text(format!(
                "Query result for: {}",
                input.query
            )))
        })
        // Layers wrap outside-in: request hits rate limit -> concurrency -> timeout -> handler
        .layer(RateLimiterLayer::per_second(5).build())
        .layer(ConcurrencyLimitLayer::new(3))
        .layer(TimeoutLayer::new(Duration::from_secs(10)))
        .build();

    // Tool 4: Burst-tolerant endpoint
    // 20 requests/sec sustained with burst capacity of 30 additional
    let burst_api = ToolBuilder::new("burst_api")
        .description("API with burst tolerance: 20/sec sustained + 30 burst capacity.")
        .handler(|input: QueryInput| async move {
            Ok(CallToolResult::text(format!(
                "Burst API result for: {}",
                input.query
            )))
        })
        .layer(RateLimiterLayer::burst(20, 30).build())
        .build();

    // Tool 5: Unrestricted tool for comparison
    let echo = ToolBuilder::new("echo")
        .description("Echo back the input. No rate limiting.")
        .handler(|input: QueryInput| async move {
            Ok(CallToolResult::text(format!("Echo: {}", input.query)))
        })
        .build();

    let router = McpRouter::new()
        .server_info("rate-limiting-example", "1.0.0")
        .instructions(
            "This server demonstrates rate limiting with Tower middleware. Try:\n\
             - search_api: 10 req/sec rate limit\n\
             - send_email: 2 per minute (strict)\n\
             - db_query: 5/sec rate limit + max 3 concurrent + 10s timeout\n\
             - burst_api: 20/sec sustained + 30 burst capacity\n\
             - echo: no limits (for comparison)\n\n\
             Exceeding a rate limit returns an error to the caller.",
        )
        .tool(search_api)
        .tool(send_email)
        .tool(db_query)
        .tool(burst_api)
        .tool(echo);

    eprintln!("Server ready. Connect with an MCP client via stdio.");
    StdioTransport::new(router).run().await?;

    Ok(())
}
