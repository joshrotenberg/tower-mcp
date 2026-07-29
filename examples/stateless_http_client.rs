//! Stateless HTTP client example for the 2026-07-28 MCP protocol.
//!
//! # What this example demonstrates
//!
//! The 2026-07-28 protocol removes the session-based handshake of the 2025-11-25
//! protocol. There is no `initialize` RPC, no `MCP-Session-Id`, and no per-session
//! state on the server. Instead:
//!
//! - Every request includes `MCP-Protocol-Version: 2026-07-28`.
//! - Every request includes `Mcp-Method: <method>` (SEP-2243).
//! - `tools/call` requests include `Mcp-Name: <tool-name>` (SEP-2243).
//! - `server/discover` replaces `initialize` for capability discovery.
//! - `subscriptions/listen` replaces per-session SSE for server-push notifications.
//!
//! This example spins up an in-process HTTP server on a random port, then makes
//! the four stateless requests in sequence and prints the results.
//!
//! # Running
//!
//! ```bash
//! cargo run --example stateless_http_client \
//!   --features "http,http-client,protocol-2026-07-28"
//! ```
//!
//! No separate server process is needed -- the server is started in-process.
//! The `protocol-2026-07-28` feature is required for the server and client to
//! compile this experimental final-protocol implementation.

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, HttpTransport, McpRouter, ProtocolSupport, SubscriptionFilter, ToolBuilder,
    client::{HttpClientTransport, McpClient},
};

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EchoInput {
    message: String,
}

fn build_router() -> McpRouter {
    let add = ToolBuilder::new("add")
        .description("Add two numbers together")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    let echo = ToolBuilder::new("echo")
        .description("Echo a message back")
        .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.message)) })
        .build();

    McpRouter::new()
        .server_info("stateless-client-example", "1.0.0")
        .tool(add)
        .tool(echo)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("stateless_http_client=debug")
        .init();

    // Start an in-process HTTP server on a random port.
    let router = build_router();
    let transport = HttpTransport::new(router).disable_origin_validation();
    let app = transport.into_router();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let base_url = format!("http://127.0.0.1:{}", addr.port());

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Give the server a moment to start.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"])?)
        .connect_simple(HttpClientTransport::new(&base_url))
        .await?;

    // -----------------------------------------------------------------------
    // Step 1: server/discover
    //
    // Replaces `initialize`. No session is created; no MCP-Session-Id is
    // returned. The client learns what protocol versions and capabilities
    // the server supports.
    // -----------------------------------------------------------------------
    println!("=== Step 1: server/discover ===");
    let discovery = client.discover("stateless-example-client", "1.0.0").await?;
    if let Some(server_info) = discovery.meta.and_then(|meta| meta.server_info) {
        println!("Server: {}", server_info.name);
    }
    println!(
        "Supported protocol versions: {}",
        discovery.supported_versions.join(", ")
    );
    println!("Selected protocol: 2026-07-28 (no session ID)");
    println!();

    // -----------------------------------------------------------------------
    // Step 2: tools/list
    //
    // No session ID. The Mcp-Method header is required by SEP-2243 when
    // using the 2026-07-28 protocol version.
    // -----------------------------------------------------------------------
    println!("=== Step 2: tools/list ===");

    let tools = client.list_tools().await?;
    println!("Available tools ({}):", tools.tools.len());
    for tool in tools.tools {
        println!(
            "  - {} : {}",
            tool.name,
            tool.description.unwrap_or_default()
        );
    }
    println!();

    // -----------------------------------------------------------------------
    // Step 3: tools/call
    //
    // Calls the "add" tool. The Mcp-Name header is required by SEP-2243 for
    // tools/call when using the 2026-07-28 protocol version. No session ID.
    // -----------------------------------------------------------------------
    println!("=== Step 3: tools/call (add, a=10, b=32) ===");

    let result = client
        .call_tool("add", serde_json::json!({ "a": 10, "b": 32 }))
        .await?;
    println!("Result: {}", result.first_text().unwrap_or("(empty)"));
    println!();

    // -----------------------------------------------------------------------
    // Step 4: subscriptions/listen
    //
    // Opens a server-push SSE stream. In the 2026-07-28 protocol this is a
    // POST (not a GET) with Accept: text/event-stream. The client validates
    // the server's mandatory first-message acknowledgment, keeps the stream
    // open for a short window, and cancels it by closing this response stream.
    // -----------------------------------------------------------------------
    println!("=== Step 4: subscriptions/listen (SSE stream, 1-second window) ===");

    let mut subscription = client
        .listen_subscriptions(SubscriptionFilter {
            tools_list_changed: Some(true),
            ..Default::default()
        })
        .await?;
    let accepted = subscription.acknowledged().await?;
    println!(
        "Subscription {:?} acknowledged: toolsListChanged={:?}",
        subscription.id(),
        accepted.tools_list_changed
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    subscription.cancel().await?;
    println!("Subscription cancelled by closing its HTTP response stream");

    println!();
    println!("Done. All four stateless 2026-07-28 requests completed.");
    client.shutdown().await?;

    Ok(())
}
