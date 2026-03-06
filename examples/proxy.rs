//! MCP Proxy example -- aggregate multiple backend servers
//!
//! Demonstrates using McpProxy to combine tools from multiple backend
//! MCP servers behind a single endpoint, with:
//! - Namespace prefixing (tools become `basic_*` and `sampling_*`)
//! - Per-backend middleware (timeout on the sampling backend)
//! - Notification forwarding (backend list changes propagate to clients)
//! - Health checks (ping all backends)
//!
//! This example spawns two backend servers as subprocesses:
//! - `basic` server (provides echo and add tools)
//! - `sampling_server` (provides summarize, confirm_delete, slow_task tools)
//!
//! Run with: cargo run --example proxy --features proxy
//!
//! Then interact via JSON-RPC on stdin/stdout, or connect with any MCP client.

use std::time::Duration;

use tower::timeout::TimeoutLayer;
use tower_mcp::GenericStdioTransport;
use tower_mcp::client::StdioClientTransport;
use tower_mcp::context::notification_channel;
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

    // Create notification channel for forwarding backend list-changed
    // events to downstream clients (e.g., when a backend adds/removes tools)
    let (notif_tx, notif_rx) = notification_channel(32);

    // Build the proxy -- backends initialize concurrently
    let result = McpProxy::builder("mcp-proxy-example", "1.0.0")
        .notification_sender(notif_tx)
        .backend("basic", basic_transport)
        .await
        // Per-backend middleware: 30s timeout on sampling backend
        .backend("sampling", sampling_transport)
        .await
        .backend_layer(TimeoutLayer::new(Duration::from_secs(30)))
        .build()
        .await?;

    // Check for skipped backends
    if !result.skipped.is_empty() {
        for s in &result.skipped {
            tracing::warn!("Skipped backend: {s}");
        }
    }
    let proxy = result.proxy;

    // Health check all backends
    let health = proxy.health_check().await;
    for h in &health {
        tracing::info!(
            namespace = %h.namespace,
            healthy = h.healthy,
            "Backend health"
        );
    }

    tracing::info!("Proxy ready, serving on stdio");

    // Serve the proxy over stdio with notification forwarding.
    // When a backend emits tools/list_changed, resources/list_changed,
    // or prompts/list_changed, the proxy refreshes its cache and forwards
    // the notification to connected clients via this transport.
    let mut transport = GenericStdioTransport::with_notifications(proxy, notif_rx);
    transport.run().await?;

    Ok(())
}
