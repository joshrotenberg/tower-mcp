//! MCP Proxy example -- aggregate multiple backend servers
//!
//! Demonstrates using McpProxy to combine tools from multiple backend
//! MCP servers behind a single endpoint, with:
//! - Namespace prefixing (tools become `basic_*` and `sampling_*`)
//! - Per-backend connection, initialization, and discovery deadlines
//! - Per-backend middleware (timeout on the sampling backend)
//! - Notification forwarding (backend list changes propagate to clients)
//! - Health checks (ping all backends)
//!
//! This example spawns two backend servers as subprocesses:
//! - `getting_started` server (provides echo and add tools)
//! - `sampling_server` (provides summarize, confirm_delete, slow_task tools)
//!
//! From the workspace root, pre-build the two servers with the default
//! features used by their subprocess commands, then start the proxy. This
//! keeps first-time compilation outside the handshake deadlines.
//!
//! ```sh
//! cargo build -p tower-mcp-examples --example getting_started --example sampling_server
//! cargo run -p tower-mcp-examples --example proxy --features proxy
//! ```
//!
//! Then interact via JSON-RPC on stdin/stdout, or connect with any MCP client.

use std::pin::pin;
use std::time::Duration;

use futures::StreamExt;
use tower::timeout::TimeoutLayer;
use tower_mcp::client::StdioClientTransport;
use tower_mcp::proxy::{BackendSetupOptions, McpProxy};
use tower_mcp::{GenericStdioTransport, notification_channel};

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter("tower_mcp=info,proxy=debug")
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("Starting MCP proxy...");

    // Populate these durations from each backend's configuration in mcp-proxy.
    let sampling_setup = BackendSetupOptions {
        connect_timeout: Duration::from_secs(5),
        initialize_timeout: Duration::from_secs(30),
        discovery_timeout: Duration::from_secs(10),
    };

    // Create notification channel for forwarding backend list-changed
    // events to downstream clients (e.g., when a backend adds/removes tools)
    let (notif_tx, notif_rx) = notification_channel(32);

    // Build the proxy -- backends initialize concurrently
    let result = McpProxy::builder("mcp-proxy-example", "1.0.0")
        .notification_sender(notif_tx)
        .backend_with_options(
            "basic",
            StdioClientTransport::spawn(
                "cargo",
                &[
                    "run",
                    "-p",
                    "tower-mcp-examples",
                    "--example",
                    "getting_started",
                ],
            ),
            BackendSetupOptions::default(),
        )
        .await
        .backend_with_options(
            "sampling",
            StdioClientTransport::spawn(
                "cargo",
                &[
                    "run",
                    "-p",
                    "tower-mcp-examples",
                    "--example",
                    "sampling_server",
                ],
            ),
            sampling_setup,
        )
        .await
        // This layer applies to routed RPCs.
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

    // Process each backend's health result as soon as it completes.
    let mut health = pin!(proxy.health_checks(Duration::from_secs(2)).await);
    while let Some(h) = health.next().await {
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
