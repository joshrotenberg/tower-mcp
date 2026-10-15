//! A session restored from a `SessionStore` keeps the client's declared
//! capabilities and negotiated extensions in router session state, the same
//! as the instance that ran `initialize` (#1561).

#![cfg(feature = "http")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;
use tower_mcp::extract::{Context, RawArgs};
use tower_mcp::protocol::ClientCapabilities;
use tower_mcp::session_store::{MemorySessionStore, SessionStore};
use tower_mcp::{CallToolResult, ExtensionDeclaration, HttpTransport, McpRouter, ToolBuilder};

const SHARED: &str = "com.example/shared";

/// A tool that reports the client capabilities and negotiated extensions the
/// handler can see.
fn router() -> McpRouter {
    let tool = ToolBuilder::new("caps")
        .description("Reports the client capabilities visible to the handler.")
        .read_only()
        .extractor_handler((), |ctx: Context, RawArgs(_args): RawArgs| async move {
            let capabilities = ctx
                .session()
                .and_then(|session| session.get::<ClientCapabilities>());
            let shared = ctx
                .negotiated_extensions()
                .and_then(|negotiated| negotiated.get(SHARED))
                .map(|extension| {
                    (
                        extension.client_settings().clone(),
                        extension.server_settings().clone(),
                    )
                });
            let report = serde_json::json!({
                "capabilities_present": capabilities.is_some(),
                "elicitation": capabilities.as_ref().is_some_and(|c| c.elicitation.is_some()),
                "sampling": capabilities.as_ref().is_some_and(|c| c.sampling.is_some()),
                "shared": shared.map(|(client, server)| {
                    serde_json::json!({"client": client, "server": server})
                }),
            });
            Ok(CallToolResult::text(report.to_string()))
        })
        .build();
    McpRouter::new().tool(tool).with_protocol_extension(
        ExtensionDeclaration::new(SHARED, serde_json::json!({"server": true})).unwrap(),
    )
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
async fn initialize(app: &axum::Router, capabilities: serde_json::Value) -> String {
    let (headers, init) = post(
        app,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": capabilities,
                "clientInfo": {"name": "caps-client", "version": "1.0.0"}
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

/// Call `caps` and return what the handler reported.
async fn call_caps(app: &axum::Router, session: &str) -> serde_json::Value {
    let (_, response) = post(
        app,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "caps", "arguments": {}}
        }),
        &[("mcp-session-id", session)],
    )
    .await;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("unexpected response: {response}"));
    serde_json::from_str(text).expect("tool result is JSON")
}

fn client_capabilities() -> serde_json::Value {
    serde_json::json!({
        "elicitation": {},
        "extensions": {
            SHARED: {"client": true},
            "com.example/client-only": {}
        }
    })
}

#[tokio::test]
async fn handler_sees_the_clients_capabilities_on_a_session() {
    let app = HttpTransport::new(router())
        .disable_origin_validation()
        .into_router();
    let session = initialize(&app, client_capabilities()).await;

    let seen = call_caps(&app, &session).await;
    assert_eq!(seen["capabilities_present"], true, "{seen}");
    assert_eq!(seen["elicitation"], true, "{seen}");
    assert_eq!(seen["sampling"], false, "{seen}");
    assert_eq!(seen["shared"]["client"]["client"], true, "{seen}");
    assert_eq!(seen["shared"]["server"]["server"], true, "{seen}");
}

#[tokio::test]
async fn restored_session_keeps_the_clients_capabilities() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());

    // First instance: initialize, then drop it. Only the stored record survives.
    let first = HttpTransport::new(router())
        .disable_origin_validation()
        .session_store(store.clone())
        .into_router();
    let session = initialize(&first, client_capabilities()).await;
    drop(first);

    // Second instance: the request restores the session from the record.
    let second = HttpTransport::new(router())
        .disable_origin_validation()
        .session_store(store)
        .into_router();
    let seen = call_caps(&second, &session).await;
    assert_eq!(seen["capabilities_present"], true, "{seen}");
    assert_eq!(seen["elicitation"], true, "{seen}");
    assert_eq!(seen["sampling"], false, "{seen}");
}

#[tokio::test]
async fn restored_session_keeps_the_negotiated_extensions() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());

    let first = HttpTransport::new(router())
        .disable_origin_validation()
        .session_store(store.clone())
        .into_router();
    let session = initialize(&first, client_capabilities()).await;
    drop(first);

    let second = HttpTransport::new(router())
        .disable_origin_validation()
        .session_store(store)
        .into_router();
    let seen = call_caps(&second, &session).await;
    // Only the identifier both peers declared is negotiated, with each
    // peer's own settings.
    assert_eq!(seen["shared"]["client"]["client"], true, "{seen}");
    assert_eq!(seen["shared"]["server"]["server"], true, "{seen}");
}
