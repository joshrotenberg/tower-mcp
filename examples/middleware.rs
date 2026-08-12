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
//! It also shows the thing composition examples usually leave out: a layer
//! that resolves a value and a handler that reads it, passed through the
//! request extensions and pulled out with the `Extension<T>` extractor.
//!
//! This example demonstrates all four patterns in one server.
//!
//! Run with: cargo run --example middleware

use std::collections::HashMap;
use std::task::{Context, Poll};
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tower::ServiceBuilder;
use tower::limit::ConcurrencyLimitLayer;
use tower::timeout::TimeoutLayer;
use tower_mcp::extract::Extension;
use tower_mcp::{
    BoxError, CallToolResult, GetPromptResult, McpRouter, PromptBuilder, ReadResourceResult,
    ResourceBuilder, ResourceContent, RouterRequest, StdioTransport, ToolBuilder, ToolRequest,
};

// =============================================================================
// Passing data from a layer to a handler
// =============================================================================
//
// Timeouts and rate limits are the easy half of middleware: they wrap a
// request and never speak to it. The half people actually ask about is a
// layer that resolves something once (a tenant, a trace id, an authenticated
// principal) and a handler that reads it.
//
// The channel is `RouterRequest::extensions`. A layer inserts a value, and a
// handler pulls it out with the `Extension<T>` extractor. The type is the
// key, so nothing is stringly-typed and a handler asking for a value no layer
// inserted is rejected rather than silently given a default.

/// What the layer below resolves for each request.
#[derive(Debug, Clone)]
struct Tenant {
    id: String,
}

/// Inserts a [`Tenant`] into every request passing through it.
///
/// A real one would read a header or a token bridged in by the transport
/// (`HttpTransport::bridge_extension`) rather than inventing the value.
#[derive(Clone)]
struct TenantLayer;

impl<S> tower::Layer<S> for TenantLayer {
    type Service = TenantService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TenantService { inner }
    }
}

#[derive(Clone)]
struct TenantService<S> {
    inner: S,
}

impl<S> tower::Service<RouterRequest> for TenantService<S>
where
    S: tower::Service<RouterRequest>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: RouterRequest) -> Self::Future {
        request.extensions.insert(Tenant {
            id: "acme-corp".to_string(),
        });
        self.inner.call(request)
    }
}

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

    // A handler reading what the layer resolved. `Extension<Tenant>` is
    // filled from the request extensions `TenantLayer` wrote, so the handler
    // never knows or cares where the value came from.
    let whoami = ToolBuilder::new("whoami")
        .description("Reports the tenant the transport layer resolved")
        .extractor_handler((), |Extension(tenant): Extension<Tenant>| async move {
            Ok(CallToolResult::text(format!("tenant: {}", tenant.id)))
        })
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
                ..Default::default()
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
             - whoami: reads a value the transport layer resolved\n\
             - Transport: global 60s timeout + 10 max concurrent requests",
        )
        .tool(quick_search)
        .tool(slow_operation)
        .tool(write_tool)
        .tool(whoami)
        .resource(slow_resource)
        .prompt(slow_prompt);

    // Transport-level middleware wraps ALL requests
    eprintln!("Server ready. Connect with an MCP client via stdio.");
    StdioTransport::new(router)
        .layer(
            ServiceBuilder::new()
                .layer(TimeoutLayer::new(Duration::from_secs(60)))
                .concurrency_limit(10)
                // Outermost-first, so this runs before the router dispatches
                // and `whoami` finds its value already in place.
                .layer(TenantLayer)
                .into_inner(),
        )
        .run()
        .await?;

    Ok(())
}
