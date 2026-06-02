//! `server/discover` walkthrough example
//!
//! Demonstrates the `server/discover` RPC (SEP-2575) as the stateless
//! alternative to `initialize`. Unlike `initialize`, `server/discover` requires
//! no prior handshake, creates no session, and can be called at any time by any
//! client to learn what the server supports.
//!
//! ## How it relates to `initialize`
//!
//! `initialize` (2025-11-25):
//! - Creates a session, negotiates a single `protocolVersion`, returns a
//!   session ID in the `MCP-Session-Id` response header.
//! - All subsequent requests must carry that session ID.
//!
//! `server/discover` (2026-07-28):
//! - Stateless -- no session is created or required.
//! - Returns `supportedVersions` (plural), letting the client pick.
//! - The client signals its chosen version via `MCP-Protocol-Version` on
//!   every subsequent request instead of maintaining a session.
//!
//! ## What this example does
//!
//! 1. Starts an `HttpTransport` server in-process with `stateless` enabled so
//!    the 2026-07-28 code path is active.
//! 2. Calls `server/discover` with `MCP-Protocol-Version: 2026-07-28` and
//!    `Mcp-Method: server/discover` (both required in 2026-07-28 strict mode).
//! 3. Prints the discover response: supported versions, capabilities, server info.
//! 4. Uses the discover response to call `tools/list` without any session --
//!    showing the stateless flow end-to-end.
//! 5. Calls `initialize` with `2025-11-25` on the same server to show the
//!    session-based path still works alongside stateless clients.
//!
//! Run with:
//!   cargo run --example server_discover --features "http,stateless"

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, HttpTransport, McpRouter, PromptBuilder, ResourceBuilder, ToolBuilder,
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

    let config = ResourceBuilder::new("file:///config.json")
        .name("Configuration")
        .description("Server configuration")
        .json(serde_json::json!({"version": "1.0.0", "debug": true}));

    let greet = PromptBuilder::new("greet")
        .description("Generate a greeting")
        .required_arg("name", "Name to greet")
        .handler(|args| async move {
            let name = args.get("name").map(|s| s.as_str()).unwrap_or("World");
            Ok(tower_mcp::GetPromptResult {
                description: Some("A friendly greeting".to_string()),
                messages: vec![tower_mcp::PromptMessage {
                    role: tower_mcp::PromptRole::User,
                    content: tower_mcp::protocol::Content::Text {
                        text: format!("Please greet {} warmly.", name),
                        annotations: None,
                        meta: None,
                    },
                    meta: None,
                }],
                meta: None,
            })
        })
        .build();

    McpRouter::new()
        .server_info("discover-example-server", "1.0.0")
        .tool(add)
        .tool(echo)
        .resource(config)
        .prompt(greet)
}

/// Start an in-process server and return its URL.
async fn start_server() -> (String, tokio::task::JoinHandle<()>) {
    #[cfg(feature = "stateless")]
    let transport = HttpTransport::new(build_router())
        .disable_origin_validation()
        .stateless(tower_mcp::stateless::StatelessConfig::new());

    #[cfg(not(feature = "stateless"))]
    let transport = HttpTransport::new(build_router()).disable_origin_validation();

    let axum_router = transport.into_router();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://127.0.0.1:{}", addr.port());

    let handle = tokio::spawn(async move {
        axum::serve(listener, axum_router).await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    (url, handle)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("server_discover=debug,tower_mcp=info")
        .init();

    let (url, _server) = start_server().await;
    println!("Server listening at {}", url);
    println!();

    let client = reqwest::Client::new();

    // -------------------------------------------------------------------------
    // Step 1: server/discover -- stateless capability advertisement
    //
    // No session, no prior handshake. Include:
    //   MCP-Protocol-Version: 2026-07-28  (selects the 2026-07-28 path)
    //   Mcp-Method: server/discover       (required in strict/2026-07-28 mode)
    // -------------------------------------------------------------------------
    println!("=== server/discover (stateless, 2026-07-28) ===");
    println!();

    let discover_resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "server/discover")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "server/discover"
        }))
        .send()
        .await?;

    let status = discover_resp.status();
    println!("HTTP status: {}", status);

    // Echo back the protocol version header the server attaches to the response.
    if let Some(pv) = discover_resp.headers().get("mcp-protocol-version") {
        println!("Response MCP-Protocol-Version: {}", pv.to_str()?);
    }
    // Confirm no session ID is returned (stateless mode never creates sessions).
    if discover_resp.headers().get("mcp-session-id").is_some() {
        println!("NOTE: server returned an unexpected mcp-session-id");
    } else {
        println!("No mcp-session-id header (expected for stateless discover)");
    }
    println!();

    let discover_body: serde_json::Value = discover_resp.json().await?;

    // The result uses supportedVersions (plural), NOT protocolVersion (singular).
    // That is the key structural difference from initialize.
    let result = &discover_body["result"];
    println!("Server info:");
    println!(
        "  name:    {}",
        result["serverInfo"]["name"].as_str().unwrap_or("?")
    );
    println!(
        "  version: {}",
        result["serverInfo"]["version"].as_str().unwrap_or("?")
    );
    println!();

    let supported = result["supportedVersions"]
        .as_array()
        .expect("supportedVersions must be an array");
    println!("Supported protocol versions ({}):", supported.len());
    for v in supported {
        println!("  - {}", v.as_str().unwrap_or("?"));
    }
    println!();

    let caps = &result["capabilities"];
    println!("Capabilities advertised by the server:");
    if caps.get("tools").is_some() {
        println!("  tools:     yes");
    }
    if caps.get("resources").is_some() {
        println!("  resources: yes");
    }
    if caps.get("prompts").is_some() {
        println!("  prompts:   yes");
    }
    println!();

    // -------------------------------------------------------------------------
    // Step 2: use the discover result to call tools/list -- no session needed.
    //
    // A 2026-07-28 client reads supportedVersions from the discover result,
    // picks one, and uses it on every subsequent request. No initialization
    // handshake, no session ID storage.
    // -------------------------------------------------------------------------
    let chosen_version = supported
        .iter()
        .find(|v| v.as_str() == Some("2025-11-25"))
        .or_else(|| supported.first())
        .and_then(|v| v.as_str())
        .unwrap_or("2025-11-25");

    println!(
        "=== tools/list (stateless, using version '{}' from discover) ===",
        chosen_version
    );
    println!();

    let tools_resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "tools/list")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        }))
        .send()
        .await?;

    println!("HTTP status: {}", tools_resp.status());
    let tools_body: serde_json::Value = tools_resp.json().await?;
    let tools = tools_body["result"]["tools"]
        .as_array()
        .expect("tools must be an array");
    println!("Tools ({}):", tools.len());
    for tool in tools {
        println!(
            "  - {} : {}",
            tool["name"].as_str().unwrap_or("?"),
            tool["description"].as_str().unwrap_or("(no description)")
        );
    }
    println!();

    // -------------------------------------------------------------------------
    // Step 3: initialize with 2025-11-25 on the same server.
    //
    // Demonstrates that the same server serves both stateless 2026-07-28
    // clients and session-based 2025-11-25 clients simultaneously.
    // -------------------------------------------------------------------------
    println!("=== initialize (session-based, 2025-11-25) ===");
    println!();

    let init_resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {
                    "name": "server-discover-example",
                    "version": "1.0.0"
                }
            }
        }))
        .send()
        .await?;

    println!("HTTP status: {}", init_resp.status());

    let session_id = init_resp
        .headers()
        .get("mcp-session-id")
        .map(|v| v.to_str().unwrap_or("?").to_string());
    if let Some(ref sid) = session_id {
        println!("Session ID: {}", sid);
    }

    let init_body: serde_json::Value = init_resp.json().await?;
    let init_result = &init_body["result"];
    println!(
        "Negotiated protocolVersion: {}",
        init_result["protocolVersion"].as_str().unwrap_or("?")
    );
    println!(
        "Server info: {} v{}",
        init_result["serverInfo"]["name"].as_str().unwrap_or("?"),
        init_result["serverInfo"]["version"].as_str().unwrap_or("?")
    );
    println!();

    // -------------------------------------------------------------------------
    // Summary: key differences between server/discover and initialize
    // -------------------------------------------------------------------------
    println!("=== Key differences ===");
    println!();
    println!("server/discover:");
    println!("  - No session created, no MCP-Session-Id in response");
    println!("  - Returns supportedVersions (plural) -- client picks");
    println!("  - Requires MCP-Protocol-Version: 2026-07-28 + Mcp-Method header");
    println!("  - Idempotent: safe to call any number of times");
    println!();
    println!("initialize:");
    println!("  - Creates a session, returns MCP-Session-Id");
    println!("  - Returns negotiated protocolVersion (singular)");
    println!("  - Must be called exactly once per session");
    println!("  - Subsequent requests must carry the session ID");

    Ok(())
}
