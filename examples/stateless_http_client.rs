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
//! - `messages/listen` replaces per-session SSE for server-push notifications.
//!
//! This example spins up an in-process HTTP server on a random port, then makes
//! the four stateless requests in sequence and prints the results.
//!
//! # Running
//!
//! ```bash
//! cargo run --example stateless_http_client --features "http,stateless"
//! ```
//!
//! No separate server process is needed -- the server is started in-process.
//! The `stateless` feature is required for the server to route 2026-07-28
//! requests without a session.

use std::time::Duration;

use futures::StreamExt;
use reqwest::Client;
use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

const PROTOCOL_VERSION: &str = "2026-07-28";

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

    let client = Client::new();

    // -----------------------------------------------------------------------
    // Step 1: server/discover
    //
    // Replaces `initialize`. No session is created; no MCP-Session-Id is
    // returned. The client learns what protocol versions and capabilities
    // the server supports.
    // -----------------------------------------------------------------------
    println!("=== Step 1: server/discover ===");
    println!("POST {} (no session, no initialize)", base_url);

    let discover_response = client
        .post(&base_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("MCP-Protocol-Version", PROTOCOL_VERSION)
        .header("Mcp-Method", "server/discover")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "server/discover"
        }))
        .send()
        .await?;

    // Stateless: no MCP-Session-Id must be present.
    assert!(
        discover_response.headers().get("mcp-session-id").is_none(),
        "server returned an unexpected MCP-Session-Id in stateless mode"
    );
    println!("Confirmed: no session ID returned (stateless)");

    let discover_body: serde_json::Value = discover_response.json().await?;
    let result = &discover_body["result"];
    println!(
        "Server: {}",
        result["serverInfo"]["name"].as_str().unwrap_or("(unknown)")
    );
    if let Some(versions) = result["supportedVersions"].as_array() {
        let version_list: Vec<&str> = versions.iter().filter_map(|v| v.as_str()).collect();
        println!("Supported protocol versions: {}", version_list.join(", "));
    }
    println!();

    // -----------------------------------------------------------------------
    // Step 2: tools/list
    //
    // No session ID. The Mcp-Method header is required by SEP-2243 when
    // using the 2026-07-28 protocol version.
    // -----------------------------------------------------------------------
    println!("=== Step 2: tools/list ===");

    let list_response = client
        .post(&base_url)
        .header("Content-Type", "application/json")
        .header("MCP-Protocol-Version", PROTOCOL_VERSION)
        .header("Mcp-Method", "tools/list")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        }))
        .send()
        .await?;

    let list_body: serde_json::Value = list_response.json().await?;
    let tools = list_body["result"]["tools"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default();

    println!("Available tools ({}):", tools.len());
    for tool in tools {
        println!(
            "  - {} : {}",
            tool["name"].as_str().unwrap_or("(unnamed)"),
            tool["description"].as_str().unwrap_or("(no description)")
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

    let call_response = client
        .post(&base_url)
        .header("Content-Type", "application/json")
        .header("MCP-Protocol-Version", PROTOCOL_VERSION)
        .header("Mcp-Method", "tools/call")
        .header("Mcp-Name", "add")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "add",
                "arguments": { "a": 10, "b": 32 }
            }
        }))
        .send()
        .await?;

    let call_body: serde_json::Value = call_response.json().await?;
    if let Some(content) = call_body["result"]["content"].as_array() {
        for item in content {
            if item["type"] == "text" {
                println!("Result: {}", item["text"].as_str().unwrap_or("(empty)"));
            }
        }
    }
    println!();

    // -----------------------------------------------------------------------
    // Step 4: messages/listen
    //
    // Opens a server-push SSE stream. In the 2026-07-28 protocol this is a
    // POST (not a GET) with Accept: text/event-stream. The server delivers
    // notifications for any in-flight requests that carry a progressToken.
    // We read for a short window and then disconnect.
    // -----------------------------------------------------------------------
    println!("=== Step 4: messages/listen (SSE stream, 1-second window) ===");

    let listen_response = client
        .post(&base_url)
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .header("MCP-Protocol-Version", PROTOCOL_VERSION)
        .header("Mcp-Method", "messages/listen")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "messages/listen",
            "params": {}
        }))
        .send()
        .await?;

    let content_type = listen_response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    println!("Response Content-Type: {}", content_type);

    let mut stream = listen_response.bytes_stream();
    let deadline = tokio::time::sleep(Duration::from_secs(1));
    tokio::pin!(deadline);

    let mut event_count = 0usize;
    loop {
        tokio::select! {
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        let text = String::from_utf8_lossy(&bytes);
                        // Print non-empty, non-comment SSE lines for visibility.
                        for line in text.lines() {
                            if !line.is_empty() && !line.starts_with(':') {
                                println!("  SSE: {}", line);
                            }
                        }
                        event_count += 1;
                    }
                    Some(Err(e)) => {
                        println!("  Stream error: {}", e);
                        break;
                    }
                    None => {
                        println!("  Stream closed by server");
                        break;
                    }
                }
            }
            _ = &mut deadline => {
                println!("  Disconnecting after 1-second window ({} chunk(s) received)", event_count);
                break;
            }
        }
    }

    println!();
    println!("Done. All four stateless 2026-07-28 requests completed.");

    Ok(())
}
