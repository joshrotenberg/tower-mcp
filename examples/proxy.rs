//! MCP Proxy example -- aggregate multiple backend servers
//!
//! Demonstrates using McpProxy to combine tools from multiple backend
//! MCP servers behind a single endpoint.
//!
//! This example spawns two backend servers as subprocesses:
//! - `basic` server (provides echo and add tools)
//! - `sampling_server` (provides summarize, confirm_delete, slow_task tools)
//!
//! The proxy namespaces them as `basic_*` and `sampling_*` and exposes
//! all tools through a single stdio interface.
//!
//! Run with: cargo run --example proxy
//!
//! Then interact via JSON-RPC on stdin/stdout, or connect with any MCP client.

use tower_mcp::GenericStdioTransport;
use tower_mcp::client::StdioClientTransport;
use tower_mcp::proxy::McpProxy;

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter("tower_mcp=info,proxy=debug")
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("Starting MCP proxy...");

    // Spawn backend servers as subprocesses
    let basic_transport =
        StdioClientTransport::spawn("cargo", &["run", "--example", "basic"]).await?;
    let sampling_transport =
        StdioClientTransport::spawn("cargo", &["run", "--example", "sampling_server"]).await?;

    // Build the proxy -- backends initialize concurrently
    let proxy = McpProxy::builder("mcp-proxy-example", "1.0.0")
        .backend("basic", basic_transport)
        .await
        .backend("sampling", sampling_transport)
        .await
        .build()
        .await?;

    tracing::info!("Proxy ready, serving on stdio");

    // Serve the proxy over stdio
    // McpProxy implements Service<RouterRequest>, so it works with any transport
    let mut transport = GenericStdioTransport::new(proxy);
    transport.run().await?;

    Ok(())
}
