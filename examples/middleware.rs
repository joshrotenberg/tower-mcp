//! Middleware patterns for tower-mcp
//!
//! Tower middleware can be applied at three levels:
//!
//! 1. **Transport-level** -- wraps ALL requests (tools, resources, prompts, ping)
//! 2. **Per-tool** -- wraps a specific tool's handler
//! 3. **Per-resource / per-prompt** -- wraps individual resource or prompt handlers
//!
//! Additionally, **guards** provide lightweight per-tool validation without
//! implementing a full tower layer.
//!
//! This example demonstrates all four patterns in one server.
//!
//! Run with: cargo run --example middleware

use std::collections::HashMap;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tower::ServiceBuilder;
use tower::limit::ConcurrencyLimitLayer;
use tower::timeout::TimeoutLayer;
use tower_mcp::{
    BoxError, CallToolResult, GetPromptResult, McpRouter, PromptBuilder, ReadResourceResult,
    ResourceBuilder, ResourceContent, StdioTransport, ToolBuilder, ToolRequest,
};

// =============================================================================
// Input types
// =============================================================================

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchInput {
    /// The search query
    query: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SlowInput {
    /// Simulated delay in milliseconds
    delay_ms: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WriteInput {
    /// Data to write
    data: String,
}

// =============================================================================
// Main
// =============================================================================

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter("tower_mcp=debug")
        .with_writer(std::io::stderr)
        .init();

    // =========================================================================
    // Pattern 1: Per-tool middleware via .layer()
    // =========================================================================

    // Fast search with short timeout -- times out if handler takes > 2s
    let quick_search = ToolBuilder::new("quick_search")
        .description("Fast search with 2-second timeout")
        .handler(|input: SearchInput| async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(CallToolResult::text(format!(
                "Results for: {}",
                input.query
            )))
        })
        .layer(TimeoutLayer::new(Duration::from_secs(2)))
        .build();

    // Slow operation with concurrency limit -- max 3 concurrent calls
    let slow_operation = ToolBuilder::new("slow_operation")
        .description("Slow operation limited to 3 concurrent calls")
        .handler(|input: SlowInput| async move {
            tokio::time::sleep(Duration::from_millis(input.delay_ms)).await;
            Ok(CallToolResult::text(format!(
                "Completed after {}ms",
                input.delay_ms
            )))
        })
        .layer(ConcurrencyLimitLayer::new(3))
        .layer(TimeoutLayer::new(Duration::from_secs(30)))
        .build();

    // =========================================================================
    // Pattern 2: Per-resource middleware via .layer()
    // =========================================================================

    // Resource with a timeout -- useful for resources that fetch from external APIs
    let slow_resource = ResourceBuilder::new("data://slow-report")
        .name("Slow Report")
        .description("Report that takes time to generate (5-second timeout)")
        .handler(|| async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri: "data://slow-report".to_string(),
                    mime_type: Some("application/json".to_string()),
                    text: Some(r#"{"status": "generated", "rows": 1000}"#.to_string()),
                    blob: None,
                    meta: None,
                }],
                meta: None,
            })
        })
        .layer(TimeoutLayer::new(Duration::from_secs(5)))
        .build();

    // =========================================================================
    // Pattern 3: Per-prompt middleware via .layer()
    // =========================================================================

    // Prompt with a timeout -- useful for prompts that call external services
    let slow_prompt = PromptBuilder::new("analyze")
        .description("Code analysis prompt (3-second timeout)")
        .required_arg("code", "The code to analyze")
        .handler(|args: HashMap<String, String>| async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let code = args.get("code").map(|s| s.as_str()).unwrap_or("");
            Ok(GetPromptResult::user_message(format!(
                "Analyze this code for issues:\n\n```\n{code}\n```"
            )))
        })
        .layer(TimeoutLayer::new(Duration::from_secs(3)));

    // =========================================================================
    // Pattern 4: Guards -- lightweight per-tool validation
    // =========================================================================

    // Guard that blocks write operations (defined once, reused across tools)
    let read_only = true;
    let write_guard = move |_req: &ToolRequest| -> Result<(), String> {
        if read_only {
            Err("Server is in read-only mode".to_string())
        } else {
            Ok(())
        }
    };

    let write_tool = ToolBuilder::new("write_data")
        .description("Write data (blocked in read-only mode)")
        .handler(|input: WriteInput| async move {
            Ok(CallToolResult::text(format!("Wrote: {}", input.data)))
        })
        .guard(write_guard)
        .build();

    // =========================================================================
    // Router + transport-level middleware
    // =========================================================================

    let router = McpRouter::new()
        .server_info("middleware-example", "1.0.0")
        .instructions(
            "Demonstrates middleware at every level:\n\
             - quick_search: per-tool 2s timeout\n\
             - slow_operation: per-tool concurrency limit (3) + 30s timeout\n\
             - data://slow-report: per-resource 5s timeout\n\
             - analyze: per-prompt 3s timeout\n\
             - write_data: guard blocks writes in read-only mode\n\
             - Transport: global 60s timeout + 10 max concurrent requests",
        )
        .tool(quick_search)
        .tool(slow_operation)
        .tool(write_tool)
        .resource(slow_resource)
        .prompt(slow_prompt);

    // Transport-level middleware wraps ALL requests
    eprintln!("Server ready. Connect with an MCP client via stdio.");
    StdioTransport::new(router)
        .layer(
            ServiceBuilder::new()
                .layer(TimeoutLayer::new(Duration::from_secs(60)))
                .concurrency_limit(10)
                .into_inner(),
        )
        .run()
        .await?;

    Ok(())
}
