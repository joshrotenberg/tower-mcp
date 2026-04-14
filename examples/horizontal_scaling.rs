//! Horizontal scaling example: two HTTP transports, one shared store.
//!
//! Spawns two independent [`HttpTransport`] instances on different ports
//! that share the same [`SessionStore`] and [`EventStore`]. A small
//! in-process client:
//!
//! 1. Initializes against instance A (port 3000). Instance A creates a
//!    session and writes its record to the shared store.
//! 2. Drops the connection and reconnects to instance B (port 3001) with
//!    the same `mcp-session-id`. Instance B has never seen the session
//!    locally, but finds it in the shared store and restores it
//!    transparently. The follow-up `tools/list` call succeeds.
//!
//! This simulates what happens behind a load balancer without session
//! affinity, or after an instance restarts.
//!
//! Run with: `cargo run --example horizontal_scaling --features http`

use std::sync::Arc;
use std::time::Duration;

use tower_mcp::{
    CallToolResult, HttpTransport, McpRouter, ToolBuilder,
    event_store::{EventStore, MemoryEventStore},
    session_store::{MemorySessionStore, SessionStore},
};

fn build_router(server_label: &str) -> McpRouter {
    let label = server_label.to_string();
    let who = ToolBuilder::new("whoami")
        .description("Returns which server instance handled the request")
        .handler(move |_: tower_mcp::NoParams| {
            let label = label.clone();
            async move { Ok(CallToolResult::text(label)) }
        })
        .build();

    McpRouter::new()
        .server_info("horizontal-scaling-demo", "1.0.0")
        .tool(who)
}

async fn run_server(
    port: u16,
    label: &str,
    sessions: Arc<dyn SessionStore>,
    events: Arc<dyn EventStore>,
) {
    let transport = HttpTransport::new(build_router(label))
        .disable_origin_validation()
        .session_store(sessions)
        .event_store(events);

    tracing::info!(port, label, "starting instance");
    if let Err(e) = transport.serve(&format!("127.0.0.1:{port}")).await {
        tracing::error!(error = %e, "server exited");
    }
}

async fn post_json(
    url: &str,
    session_id: Option<&str>,
    body: serde_json::Value,
) -> Result<(reqwest::StatusCode, Option<String>, serde_json::Value), tower_mcp::BoxError> {
    let client = reqwest::Client::new();
    let mut req = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(id) = session_id {
        req = req.header("mcp-session-id", id);
    }
    let resp = req.json(&body).send().await?;
    let status = resp.status();
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let payload: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    Ok((status, sid, payload))
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tower_mcp=info".parse()?)
                .add_directive("horizontal_scaling=info".parse()?),
        )
        .init();

    // Shared stores — in production these would be Redis-backed.
    let session_store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let event_store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::new());

    // Spawn two instances.
    tokio::spawn(run_server(
        3000,
        "instance-A",
        session_store.clone(),
        event_store.clone(),
    ));
    tokio::spawn(run_server(
        3001,
        "instance-B",
        session_store.clone(),
        event_store.clone(),
    ));

    // Give the servers a moment to bind.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let a = "http://127.0.0.1:3000/";
    let b = "http://127.0.0.1:3001/";

    tracing::info!("step 1: initialize against instance A");
    let (status, session_id, _) = post_json(
        a,
        None,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "demo-client", "version": "1.0.0" }
            }
        }),
    )
    .await?;
    assert!(status.is_success(), "initialize failed: {status}");
    let session_id = session_id.expect("expected mcp-session-id header");
    tracing::info!(session_id = %session_id, "instance A issued session id");

    tracing::info!("step 2: call whoami on instance A");
    let (_, _, resp) = post_json(
        a,
        Some(&session_id),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "whoami", "arguments": {} }
        }),
    )
    .await?;
    tracing::info!(result = ?resp["result"], "A says");

    tracing::info!("step 3: same session id against instance B (never seen it locally)");
    let (_, _, resp) = post_json(
        b,
        Some(&session_id),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "whoami", "arguments": {} }
        }),
    )
    .await?;

    // Instance B finds the record in the shared SessionStore, rebuilds the
    // runtime session locally, and serves the request.
    tracing::info!(result = ?resp["result"], "B says (after restore)");

    if resp.get("result").is_some() {
        tracing::info!("success: session continued transparently on instance B");
    } else {
        tracing::error!(response = ?resp, "instance B did not restore the session");
    }

    Ok(())
}
