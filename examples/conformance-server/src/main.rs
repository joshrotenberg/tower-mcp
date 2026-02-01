//! MCP Conformance Test Server
//!
//! This server implements all MCP protocol features for conformance testing.
//! It passes all 39 official MCP conformance tests.
//!
//! ## Running
//!
//! Stdio transport (for Claude Desktop, agents):
//! ```bash
//! cargo run -p conformance-server -- --transport stdio
//! ```
//!
//! HTTP transport (for conformance test suite):
//! ```bash
//! cargo run -p conformance-server -- --transport http --port 3001
//! ```
//!
//! ## Features Demonstrated
//!
//! - Tools with various input types and state
//! - Resources and resource templates
//! - Prompts with arguments
//! - Progress notifications
//! - Logging notifications
//! - Sampling (LLM requests back to client)
//! - Elicitation (user input requests)

mod prompts;
mod resources;
mod tools;

use clap::Parser;
use tower_mcp::context::notification_channel;
use tower_mcp::{HttpTransport, McpRouter, StdioTransport};

#[derive(Parser)]
#[command(name = "conformance-server")]
#[command(about = "MCP conformance test server implementing all spec features")]
struct Args {
    /// Transport to use: stdio or http
    #[arg(long, default_value = "http")]
    transport: String,

    /// Port for HTTP transport
    #[arg(long, default_value = "3001")]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tower_mcp=debug".parse()?),
        )
        .init();

    let args = Args::parse();

    let (tx, _rx) = notification_channel(256);

    let mut router = McpRouter::new()
        .server_info("conformance-server", "0.1.0")
        .instructions(
            "MCP conformance test server implementing all spec features. \
             This server passes all 39 official MCP conformance tests and \
             demonstrates tools, resources, prompts, progress notifications, \
             logging, sampling, and elicitation.",
        )
        .with_notification_sender(tx);

    for tool in tools::build_tools() {
        router = router.tool(tool);
    }
    for resource in resources::build_resources() {
        router = router.resource(resource);
    }
    for template in resources::build_resource_templates() {
        router = router.resource_template(template);
    }
    for prompt in prompts::build_prompts() {
        router = router.prompt(prompt);
    }

    match args.transport.as_str() {
        "stdio" => {
            tracing::info!("Starting conformance server on stdio");
            StdioTransport::new(router).run().await?;
        }
        "http" => {
            let transport = HttpTransport::new(router).with_sampling();
            let app = transport.into_router_at("/mcp");
            let addr = format!("127.0.0.1:{}", args.port);
            tracing::info!("Starting conformance server on http://{}/mcp", addr);
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            axum::serve(listener, app).await?;
        }
        other => {
            return Err(format!("Unknown transport: {}", other).into());
        }
    }

    Ok(())
}
