//! `ctx.client_info()`: handlers read the calling client's identity from the
//! `initialize` handshake on a 2025 session, or from per-request `_meta` on
//! the 2026-07-28 lifecycle.

#![cfg(feature = "http")]

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;
use tower_mcp::extract::{Context, RawArgs, State};
use tower_mcp::protocol::Implementation;
use tower_mcp::session_store::{MemorySessionStore, SessionStore};
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

/// Records what `ctx.client_info()` returned, so a test can assert on it
/// after the HTTP round-trip.
#[derive(Default, Clone)]
struct Captured(Arc<Mutex<Option<Implementation>>>);

fn router(captured: Captured) -> McpRouter {
    let tool = ToolBuilder::new("whoami")
        .description("Returns the calling client's name and version.")
        .read_only()
        .extractor_handler(
            captured,
            |State(captured): State<Captured>, ctx: Context, RawArgs(_args): RawArgs| async move {
                let info = ctx.client_info();
                let answer = info
                    .as_ref()
                    .map(|i| format!("{}/{}", i.name, i.version))
                    .unwrap_or_else(|| "absent".into());
                *captured.0.lock().unwrap() = info;
                Ok(CallToolResult::text(answer))
            },
        )
        .build();
    McpRouter::new().tool(tool)
}

async fn post(
    app: &axum::Router,
    body: serde_json::Value,
    headers: &[(&str, &str)],
) -> (axum::http::HeaderMap, serde_json::Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let response_headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    // A session response may arrive as SSE; take the data line either way.
    let json = serde_json::from_str(&text).unwrap_or_else(|_| {
        match text.lines().find_map(|l| l.strip_prefix("data: ")) {
            Some(data) => serde_json::from_str(data).expect("SSE data is JSON"),
            None => serde_json::Value::Null,
        }
    });
    (response_headers, json)
}

/// Run `initialize` and `notifications/initialized`, returning the session id.
async fn initialize(app: &axum::Router, name: &str, version: &str) -> String {
    let (headers, init) = post(
        app,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": name, "version": version}
            }
        }),
        &[],
    )
    .await;
    assert!(init["result"].is_object(), "initialize failed: {init}");
    let session = headers
        .get("mcp-session-id")
        .expect("session id")
        .to_str()
        .unwrap()
        .to_string();
    post(
        app,
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        &[("mcp-session-id", &session)],
    )
    .await;
    session
}

async fn call_whoami(app: &axum::Router, session: &str) -> serde_json::Value {
    let (_, response) = post(
        app,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "whoami", "arguments": {}}
        }),
        &[("mcp-session-id", session)],
    )
    .await;
    response
}

#[tokio::test]
async fn handler_sees_the_initializing_clients_info_on_a_session() {
    let captured = Captured::default();
    let app = HttpTransport::new(router(captured.clone()))
        .disable_origin_validation()
        .into_router();
    let session = initialize(&app, "session-client", "1.2.3").await;

    let response = call_whoami(&app, &session).await;
    assert_eq!(
        response["result"]["content"][0]["text"], "session-client/1.2.3",
        "unexpected response: {response}"
    );
    let info = captured.0.lock().unwrap().clone().expect("client info");
    assert_eq!(info.name, "session-client");
    assert_eq!(info.version, "1.2.3");
}

#[tokio::test]
async fn restored_session_keeps_the_initializing_clients_info() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());

    // First instance: initialize, then drop it. Only the stored record survives.
    let first = HttpTransport::new(router(Captured::default()))
        .disable_origin_validation()
        .session_store(store.clone())
        .into_router();
    let session = initialize(&first, "restored-client", "4.5.6").await;
    drop(first);

    // Second instance: the request restores the session from the record.
    let captured = Captured::default();
    let second = HttpTransport::new(router(captured.clone()))
        .disable_origin_validation()
        .session_store(store)
        .into_router();
    let response = call_whoami(&second, &session).await;
    assert_eq!(
        response["result"]["content"][0]["text"], "restored-client/4.5.6",
        "unexpected response: {response}"
    );
}

#[cfg(feature = "stateless")]
mod final_protocol {
    use super::*;

    async fn call(meta_client_info: Option<serde_json::Value>) -> serde_json::Value {
        let version = tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28;
        let captured = Captured::default();
        let app = HttpTransport::new(router(captured))
            .disable_origin_validation()
            .into_router();
        let mut meta = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": version,
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        if let Some(info) = meta_client_info {
            meta["io.modelcontextprotocol/clientInfo"] = info;
        }
        let (_, response) = post(
            &app,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "whoami", "arguments": {}, "_meta": meta}
            }),
            &[
                ("Mcp-Protocol-Version", version),
                ("Mcp-Method", "tools/call"),
                ("Mcp-Name", "whoami"),
            ],
        )
        .await;
        response
    }

    #[tokio::test]
    async fn handler_sees_client_info_from_per_request_meta() {
        let response = call(Some(
            serde_json::json!({"name": "meta-client", "version": "7.8.9"}),
        ))
        .await;
        assert_eq!(
            response["result"]["content"][0]["text"], "meta-client/7.8.9",
            "unexpected response: {response}"
        );
    }

    #[tokio::test]
    async fn client_info_is_none_when_the_meta_omits_it() {
        let response = call(None).await;
        assert_eq!(
            response["result"]["content"][0]["text"], "absent",
            "unexpected response: {response}"
        );
    }
}
