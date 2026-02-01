//! markdownlint-mcp - MCP server for markdown linting
//!
//! This server provides markdown linting capabilities via MCP, powered by mdbook-lint.
//!
//! ## Tools
//! - `lint_content` - Lint markdown content directly
//! - `lint_file` - Lint a local markdown file
//! - `lint_url` - Fetch and lint markdown from a URL
//! - `list_rules` - List all available lint rules
//! - `explain_rule` - Get detailed explanation of a rule
//! - `fix_content` - Apply automatic fixes to markdown content
//!
//! ## Resources
//! - `rules://{rule_id}` - Browse individual rule details
//!
//! ## Prompts
//! - `fix_suggestions` - Generate fix suggestions for lint violations

mod engine;
mod resources;
mod tools;

use clap::Parser;
use tower_mcp::context::notification_channel;
use tower_mcp::{HttpTransport, McpRouter, StdioTransport};

#[derive(Parser)]
#[command(name = "markdownlint-mcp")]
#[command(about = "MCP server for markdown linting")]
struct Args {
    /// Transport to use: stdio or http
    #[arg(long, default_value = "stdio")]
    transport: String,

    /// Port for HTTP transport
    #[arg(long, default_value = "3002")]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tower_mcp=debug".parse()?)
                .add_directive("markdownlint_mcp=debug".parse()?),
        )
        .init();

    let args = Args::parse();

    let (tx, _rx) = notification_channel(256);

    let mut router = McpRouter::new()
        .server_info("markdownlint-mcp", env!("CARGO_PKG_VERSION"))
        .instructions(
            "Markdown linting server powered by mdbook-lint. \
             Use lint_content to check markdown text, lint_file for local files, \
             or lint_url to fetch and lint from URLs. Use list_rules to see available \
             rules and explain_rule for details. fix_content can auto-fix some issues.",
        )
        .with_notification_sender(tx);

    // Register tools
    for tool in tools::build_tools() {
        router = router.tool(tool);
    }

    // Register resources
    for resource in resources::build_resources() {
        router = router.resource(resource);
    }
    for template in resources::build_resource_templates() {
        router = router.resource_template(template);
    }

    // Register prompts
    router = router.prompt(tools::build_fix_suggestions_prompt());

    match args.transport.as_str() {
        "stdio" => {
            tracing::info!("Starting markdownlint-mcp server on stdio");
            StdioTransport::new(router).run().await?;
        }
        "http" => {
            let transport = HttpTransport::new(router);
            let app = transport.into_router_at("/mcp");
            let addr = format!("127.0.0.1:{}", args.port);
            tracing::info!("Starting markdownlint-mcp server on http://{}/mcp", addr);
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            axum::serve(listener, app).await?;
        }
        other => {
            return Err(format!("Unknown transport: {}", other).into());
        }
    }

    Ok(())
}
