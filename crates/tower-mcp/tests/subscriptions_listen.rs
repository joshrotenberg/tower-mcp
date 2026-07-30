//! Integration tests for the `subscriptions/listen` RPC (SEP-2575 / SEP-2567).
//!
//! `subscriptions/listen` opens a server-to-client notification stream over HTTP
//! POST. The server responds with `Content-Type: text/event-stream` (SSE)
//! for the final 2026-07-28 protocol. Requests that target an older-protocol
//! server receive a JSON-RPC `Method Not Found` (-32601) error instead.

#![cfg(feature = "http")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

fn router() -> McpRouter {
    let echo = ToolBuilder::new("echo")
        .description("Echo a value")
        .read_only()
        .handler(|v: serde_json::Value| async move { Ok(CallToolResult::text(v.to_string())) })
        .build();
    McpRouter::new()
        .server_info("listen-test-server", "1.0.0")
        .tool(echo)
}

/// Build a transport with origin/host validation disabled (test convenience).
fn app() -> axum::Router {
    HttpTransport::new(router())
        .disable_origin_validation()
        .disable_host_validation()
        .into_router()
}

/// POST a `subscriptions/listen` request with the given protocol version.
async fn post_subscriptions_listen(protocol_version: Option<&str>) -> axum::response::Response {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");

    if let Some(v) = protocol_version {
        builder = builder
            .header("Mcp-Protocol-Version", v)
            .header("Mcp-Method", "subscriptions/listen");
    }

    let params = if let Some(version) = protocol_version {
        serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": version,
                "io.modelcontextprotocol/clientCapabilities": {}
            },
            "notifications": {}
        })
    } else {
        serde_json::json!({})
    };
    let request = builder
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "subscriptions/listen",
                "params": params
            })
            .to_string(),
        ))
        .unwrap();

    app().oneshot(request).await.unwrap()
}

#[tokio::test]
#[cfg(feature = "stateless")]
async fn subscriptions_listen_returns_sse_when_protocol_is_2026_07_28() {
    // Clients that request protocol 2026-07-28 get an SSE stream back.
    let response = post_subscriptions_listen(Some("2026-07-28")).await;

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "expected 200 OK for subscriptions/listen with protocol 2026-07-28"
    );

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    assert!(
        content_type.contains("text/event-stream"),
        "expected Content-Type: text/event-stream, got: {content_type}"
    );
}

#[tokio::test]
async fn subscriptions_listen_returns_method_not_found_for_old_protocol() {
    // Without a 2026-07-28 Mcp-Protocol-Version header, the session falls back
    // to the 2025-11-25 negotiated version, which does not support
    // subscriptions/listen. The server must return a JSON-RPC Method Not Found error.
    let response = post_subscriptions_listen(None).await;

    // JSON-RPC errors ride on 200 OK at the HTTP level.
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "JSON-RPC errors should have HTTP 200"
    );

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.contains("application/json"),
        "expected JSON error body, Content-Type was: {content_type}"
    );

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    // Must be a JSON-RPC error, not a result.
    assert!(
        body.get("error").is_some(),
        "expected JSON-RPC error object, got: {body}"
    );
    assert!(
        body.get("result").is_none(),
        "must not have a result field when an error is returned: {body}"
    );

    // Code -32601 = Method Not Found
    let code = body["error"]["code"].as_i64().unwrap();
    assert_eq!(
        code, -32601,
        "expected Method Not Found (-32601), got code: {code}"
    );

    // JSON-RPC spec: error response MUST echo the request id.
    let id = body["id"].as_i64();
    assert_eq!(
        id,
        Some(1),
        "error response must echo the request id, got: {body}"
    );
}

/// Legacy session and replay headers are ignored by the sessionless final
/// protocol.
#[cfg(feature = "stateless")]
#[tokio::test]
async fn subscriptions_listen_ignores_legacy_session_headers() {
    let a = app();
    let listen_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("mcp-session-id", "legacy-session-that-does-not-exist")
        .header("last-event-id", "legacy-event")
        .header("Mcp-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "subscriptions/listen")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":2,"method":"subscriptions/listen","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}},"notifications":{"toolsListChanged":true}}}"#,
        ))
        .unwrap();

    let listen_resp = a.oneshot(listen_request).await.unwrap();
    assert_eq!(
        listen_resp.status(),
        StatusCode::OK,
        "final subscriptions/listen must ignore legacy session state"
    );

    let content_type = listen_resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.contains("text/event-stream"),
        "expected Content-Type: text/event-stream, got: {content_type}"
    );
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn subscriptions_listen_requires_notification_filter() {
    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "subscriptions/listen")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":3,"method":"subscriptions/listen","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#,
        ))
        .unwrap();
    let response = app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["id"], 3);
    assert_eq!(body["error"]["code"], -32602);
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn server_gracefully_drains_http_subscription_streams() {
    let router = router();
    let publisher = router.clone();
    let (app, handle) = HttpTransport::new(router)
        .disable_origin_validation()
        .disable_host_validation()
        .into_router_with_handle();
    let request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "subscriptions/listen")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":"graceful-http","method":"subscriptions/listen","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}},"notifications":{"resourceSubscriptions":["file:///wanted"]}}}"#,
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(handle.subscription_count(), 1);
    assert!(publisher.notify_resource_updated("file:///ignored"));
    assert!(publisher.notify_resource_updated("file:///wanted"));
    assert_eq!(handle.close_subscriptions(), 1);
    assert_eq!(handle.subscription_count(), 0);

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    let acknowledgment = body
        .find("notifications/subscriptions/acknowledged")
        .expect("stream must begin with an acknowledgment");
    let completion = body
        .find("\"resultType\":\"complete\"")
        .expect("stream must end with a graceful result");
    assert!(acknowledgment < completion);
    assert!(body.contains("\"id\":\"graceful-http\""));
    assert!(body.contains("\"io.modelcontextprotocol/subscriptionId\":\"graceful-http\""));
    assert!(body.contains("\"io.modelcontextprotocol/serverInfo\""));
    assert!(body.contains("\"name\":\"listen-test-server\""));
    assert!(body.contains("notifications/resources/updated"));
    assert!(body.contains("file:///wanted"));
    assert!(!body.contains("file:///ignored"));
}

/// Exercise the pure session-fallback branch (no header, session carries version).
///
/// Without final-protocol support compiled in, the legacy routing behavior
/// remains MethodNotFound.
#[cfg(not(feature = "stateless"))]
#[tokio::test]
async fn subscriptions_listen_session_fallback_placeholder() {
    // When 2026-07-28 is in SUPPORTED_PROTOCOL_VERSIONS, initialize with that
    // version (no stateless feature), capture the session ID, then POST
    // subscriptions/listen with no Mcp-Protocol-Version header. The session record
    // will carry 2026-07-28 and the server should return SSE.
    //
    // For now: just verify the existing 2025-11-25 path (no header = MethodNotFound)
    // still works correctly, confirming the test runs in CI.
    let response = post_subscriptions_listen(None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        body.get("error").is_some(),
        "headerless request on 2025-11-25 session must return Method Not Found"
    );
}
