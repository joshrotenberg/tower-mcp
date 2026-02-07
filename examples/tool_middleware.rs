//! Per-tool middleware example
//!
//! Demonstrates applying tower middleware to individual tools using `.layer()`.
//! This is distinct from transport-level middleware: per-tool layers only apply
//! to that specific tool, allowing fine-grained control over behavior.
//!
//! This example shows:
//! - Timeout: slow_search has a 30-second timeout, quick_search has 2 seconds
//! - Concurrency: expensive_operation limited to 5 concurrent calls
//! - Multiple layers: combined timeout + concurrency limits
//!
//! Run with: cargo run --example tool_middleware
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
struct SearchInput {
    /// The search query
    query: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SlowSearchInput {
    /// The search query
    query: String,
    /// Simulated delay in milliseconds
    delay_ms: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ExpensiveInput {
    /// Task identifier
    task_id: u32,
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    eprintln!("Starting per-tool middleware example...");

    // Tool 1: Quick search with short (2-second) timeout
    let quick_search = ToolBuilder::new("quick_search")
        .description("Fast search with 2-second timeout. Fails if search takes too long.")
        .handler(|input: SearchInput| async move {
            // Simulate a fast search
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(CallToolResult::text(format!(
                "Quick results for: {}",
                input.query
            )))
        })
        .layer(TimeoutLayer::new(Duration::from_secs(2)))
        .build();

    // Tool 2: Slow search with long (30-second) timeout
    let slow_search = ToolBuilder::new("slow_search")
        .description("Thorough search with 30-second timeout. Use delay_ms to simulate work.")
        .handler(|input: SlowSearchInput| async move {
            // Simulate a slow, thorough search
            tokio::time::sleep(Duration::from_millis(input.delay_ms)).await;
            Ok(CallToolResult::text(format!(
                "Thorough results for: {} (took {}ms)",
                input.query, input.delay_ms
            )))
        })
        .layer(TimeoutLayer::new(Duration::from_secs(30)))
        .build();

    // Tool 3: Expensive operation with concurrency limit
    // Only 5 concurrent calls allowed; additional calls will wait
    let active_count = Arc::new(AtomicU32::new(0));
    let peak_count = Arc::new(AtomicU32::new(0));
    let active = active_count.clone();
    let peak = peak_count.clone();

    let expensive_operation = ToolBuilder::new("expensive_operation")
        .description("Resource-intensive operation limited to 5 concurrent calls.")
        .handler(move |input: ExpensiveInput| {
            let active = active.clone();
            let peak = peak.clone();
            async move {
                // Track concurrency
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(current, Ordering::SeqCst);

                // Simulate expensive work
                tokio::time::sleep(Duration::from_millis(500)).await;

                let result = format!(
                    "Task {} completed (concurrent: {}, peak: {})",
                    input.task_id,
                    current,
                    peak.load(Ordering::SeqCst)
                );

                active.fetch_sub(1, Ordering::SeqCst);
                Ok(CallToolResult::text(result))
            }
        })
        .layer(ConcurrencyLimitLayer::new(5))
        .build();

    // Tool 4: Combined layers - both timeout AND concurrency limit
    let rate_limited_api = ToolBuilder::new("rate_limited_api")
        .description("API call with both 10-second timeout and 3 concurrent call limit.")
        .handler(|input: SearchInput| async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(CallToolResult::text(format!(
                "API response for: {}",
                input.query
            )))
        })
        // Multiple layers can be chained; outer layers wrap inner layers
        .layer(TimeoutLayer::new(Duration::from_secs(10)))
        .layer(ConcurrencyLimitLayer::new(3))
        .build();

    // Tool 5: No middleware - direct handler for comparison
    let simple_tool = ToolBuilder::new("simple_tool")
        .description("Simple tool with no middleware layers.")
        .handler(|input: SearchInput| async move {
            Ok(CallToolResult::text(format!(
                "Simple result: {}",
                input.query
            )))
        })
        .build();

    let router = McpRouter::new()
        .server_info("tool-middleware-example", "1.0.0")
        .instructions(
            "This server demonstrates per-tool middleware. Each tool has different \
             timeout and concurrency limits. Try:\n\
             - quick_search: 2s timeout\n\
             - slow_search: 30s timeout, use delay_ms to test\n\
             - expensive_operation: max 5 concurrent calls\n\
             - rate_limited_api: 10s timeout + 3 concurrent calls\n\
             - simple_tool: no middleware",
        )
        .tool(quick_search)
        .tool(slow_search)
        .tool(expensive_operation)
        .tool(rate_limited_api)
        .tool(simple_tool);

    eprintln!("Server ready. Connect with an MCP client via stdio.");
    StdioTransport::new(router).run().await?;

    Ok(())
}
