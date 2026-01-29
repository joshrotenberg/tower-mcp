//! Crates.io MCP Server
//!
//! A comprehensive example MCP server demonstrating tower-mcp features.

mod prompts;
mod resources;
mod state;
mod tools;

use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use tower::ServiceBuilder;
use tower::timeout::TimeoutLayer;
use tower_mcp::{HttpTransport, McpRouter, StdioTransport};
use tower_resilience_bulkhead::BulkheadLayer;
use tower_resilience_ratelimiter::RateLimiterLayer;

use crate::state::AppState;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Transport {
    Stdio,
    Http,
}

#[derive(Parser, Debug)]
#[command(name = "crates-mcp")]
#[command(about = "MCP server for querying crates.io", long_about = None)]
struct Args {
    /// Transport to use
    #[arg(short, long, default_value = "stdio")]
    transport: Transport,

    /// Maximum concurrent requests (concurrency limit)
    #[arg(long, default_value = "10")]
    max_concurrent: usize,

    /// Rate limit interval between crates.io API calls (in milliseconds)
    #[arg(long, default_value = "1000")]
    rate_limit_ms: u64,

    /// Log level
    #[arg(short, long, default_value = "info")]
    log_level: String,

    /// HTTP host to bind to (use 0.0.0.0 for public access)
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// HTTP port to bind to
    #[arg(short, long, default_value = "3000")]
    port: u16,

    /// Request timeout in seconds (for HTTP transport)
    #[arg(long, default_value = "30")]
    request_timeout_secs: u64,
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    let args = Args::parse();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(format!("crates_mcp={}", args.log_level).parse()?)
                .add_directive(format!("tower_mcp={}", args.log_level).parse()?),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::info!(
        transport = ?args.transport,
        max_concurrent = args.max_concurrent,
        rate_limit_ms = args.rate_limit_ms,
        "Starting crates-mcp server"
    );

    // Create shared state with rate limiting for crates.io API
    let rate_limit = Duration::from_millis(args.rate_limit_ms);
    let state =
        Arc::new(AppState::new(rate_limit).map_err(|e| format!("Failed to create state: {}", e))?);

    // Build all tools
    let search_tool = tools::search::build(state.clone());
    let info_tool = tools::info::build(state.clone());
    let versions_tool = tools::versions::build(state.clone());
    let deps_tool = tools::dependencies::build(state.clone());
    let reverse_deps_tool = tools::reverse_deps::build(state.clone());
    let downloads_tool = tools::downloads::build(state.clone());
    let owners_tool = tools::owners::build(state.clone());

    // Build resources
    let recent_searches = resources::recent_searches::build(state.clone());

    // Build prompts
    let analyze_prompt = prompts::analyze::build();
    let compare_prompt = prompts::compare::build();

    // Create router with all capabilities
    let router = McpRouter::new()
        .server_info("crates-mcp", env!("CARGO_PKG_VERSION"))
        .instructions(
            "MCP server for querying crates.io - the Rust package registry.\n\n\
             Available tools:\n\
             - search_crates: Find crates by name/keywords\n\
             - get_crate_info: Get detailed crate information\n\
             - get_crate_versions: Get version history\n\
             - get_dependencies: Get dependencies for a version\n\
             - get_reverse_dependencies: Find crates that depend on this crate\n\
             - get_downloads: Get download statistics\n\
             - get_owners: Get crate owners/maintainers\n\n\
             Use the prompts for guided analysis:\n\
             - analyze_crate: Comprehensive crate analysis\n\
             - compare_crates: Compare multiple crates",
        )
        .tool(search_tool)
        .tool(info_tool)
        .tool(versions_tool)
        .tool(deps_tool)
        .tool(reverse_deps_tool)
        .tool(downloads_tool)
        .tool(owners_tool)
        .resource(recent_searches)
        .prompt(analyze_prompt)
        .prompt(compare_prompt);

    match args.transport {
        Transport::Stdio => {
            // For stdio, we serve directly without middleware since error handling
            // is more complex (would need error type conversion).
            tracing::info!("Serving over stdio");
            StdioTransport::new(router).run().await?;
        }
        Transport::Http => {
            let addr = format!("{}:{}", args.host, args.port);
            tracing::info!(%addr, "Serving over HTTP");

            // Build tower middleware stack for request protection:
            //
            // 1. TimeoutLayer - Request timeout protection
            // 2. RateLimiterLayer - Limits requests per second (token bucket)
            // 3. BulkheadLayer - Limits concurrent in-flight requests
            //
            // These layers compose naturally with tower-mcp's Service implementation.
            // The HTTP transport's CatchError wrapper converts middleware errors
            // to JSON-RPC error responses.
            //
            // tower-resilience layers use composite error types that wrap both
            // the layer's own errors and the inner service error, making them
            // compatible with tower-mcp's Infallible error type.
            let rate_limiter = RateLimiterLayer::builder()
                .limit_for_period(5) // 5 requests per period
                .refresh_period(Duration::from_secs(1))
                .timeout_duration(Duration::from_millis(500))
                .build();

            let bulkhead = BulkheadLayer::builder()
                .max_concurrent_calls(args.max_concurrent)
                .max_wait_duration(Duration::from_millis(500))
                .build();

            let transport = HttpTransport::new(router)
                .disable_origin_validation() // Allow any origin for public demo
                .layer(
                    ServiceBuilder::new()
                        .layer(TimeoutLayer::new(Duration::from_secs(
                            args.request_timeout_secs,
                        )))
                        .layer(rate_limiter)
                        .layer(bulkhead)
                        .into_inner(),
                );

            transport.serve(&addr).await?;
        }
    }

    Ok(())
}
