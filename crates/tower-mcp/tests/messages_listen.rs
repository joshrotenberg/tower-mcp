//! Integration tests for the `messages/listen` RPC (SEP-2575 / SEP-2567).
//!
//! `messages/listen` opens a server-to-client notification stream over HTTP
//! POST. The server responds with `Content-Type: text/event-stream` (SSE)
//! when the negotiated or requested protocol version is >= 2026-07-28
//! (the UPCOMING_PROTOCOL_VERSION). Requests that target an older-protocol
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

/// POST a `messages/listen` request with the given `Mcp-Protocol-Version` header.
async fn post_messages_listen(protocol_version: Option<&str>) -> axum::response::Response {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");

    if let Some(v) = protocol_version {
        builder = builder.header("Mcp-Protocol-Version", v);
    }

    let request = builder
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":1,"method":"messages/listen","params":{}}"#,
        ))
        .unwrap();

    app().oneshot(request).await.unwrap()
}

#[tokio::test]
async fn messages_listen_returns_sse_when_protocol_is_2026_07_28() {
    // Clients that request protocol 2026-07-28 get an SSE stream back.
    let response = post_messages_listen(Some("2026-07-28")).await;

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "expected 200 OK for messages/listen with protocol 2026-07-28"
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
async fn messages_listen_returns_method_not_found_for_old_protocol() {
    // Without a 2026-07-28 Mcp-Protocol-Version header, the session falls back
    // to the 2025-11-25 negotiated version, which does not support
    // messages/listen. The server must return a JSON-RPC Method Not Found error.
    let response = post_messages_listen(None).await;

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

/// Initialize with 2025-11-25 (creates a session), then POST `messages/listen`
/// with a per-request `Mcp-Protocol-Version: 2026-07-28` header.
///
/// This exercises the header-override branch in `handle_post`: the session was
/// negotiated at 2025-11-25, but the per-request header promotes the effective
/// version to 2026-07-28, which enables the SSE path.
#[cfg(feature = "stateless")]
#[tokio::test]
async fn messages_listen_via_session_with_header_override() {
    let a = app();

    // Step 1: initialize with 2025-11-25 to create a session.
    let init_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1.0.0"}}}"#,
        ))
        .unwrap();

    let init_resp = a.clone().oneshot(init_request).await.unwrap();
    assert_eq!(
        init_resp.status(),
        StatusCode::OK,
        "initialize must succeed"
    );

    let session_id = init_resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .expect("initialize response must include mcp-session-id header");

    // Step 2: POST messages/listen with session ID + Mcp-Protocol-Version: 2026-07-28.
    // The session is at 2025-11-25 but the per-request header overrides the
    // effective version to 2026-07-28, so the server should return SSE.
    let listen_request = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .header("Mcp-Protocol-Version", "2026-07-28")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":2,"method":"messages/listen","params":{}}"#,
        ))
        .unwrap();

    let listen_resp = a.oneshot(listen_request).await.unwrap();
    assert_eq!(
        listen_resp.status(),
        StatusCode::OK,
        "messages/listen with header override must return 200"
    );

    let content_type = listen_resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.contains("text/event-stream"),
        "expected Content-Type: text/event-stream for session+header override, got: {content_type}"
    );
}

/// Exercise the pure session-fallback branch (no header, session carries version).
///
/// With the `stateless` feature enabled, a 2026-07-28 initialize creates a
/// stateless session (no persisted record). With `stateless` disabled the
/// router negotiates the version down to `LATEST_PROTOCOL_VERSION` (2025-11-25)
/// because 2026-07-28 is not yet in `SUPPORTED_PROTOCOL_VERSIONS`. In either
/// case the session record does not carry 2026-07-28 after a standard
/// `initialize` handshake, so the pure headerless fallback path cannot be
/// exercised through the HTTP API without the per-request header override.
///
/// The header-override test (`messages_listen_via_session_with_header_override`)
/// covers the adjacent code path; this placeholder documents the gap.
///
/// If 2026-07-28 is promoted to `SUPPORTED_PROTOCOL_VERSIONS`, this test can
/// be filled in: initialize without the stateless feature, capture the session
/// ID, and POST `messages/listen` with only the session ID header (no
/// `Mcp-Protocol-Version`).
#[cfg(not(feature = "stateless"))]
#[tokio::test]
async fn messages_listen_session_fallback_placeholder() {
    // When 2026-07-28 is in SUPPORTED_PROTOCOL_VERSIONS, initialize with that
    // version (no stateless feature), capture the session ID, then POST
    // messages/listen with no Mcp-Protocol-Version header. The session record
    // will carry 2026-07-28 and the server should return SSE.
    //
    // For now: just verify the existing 2025-11-25 path (no header = MethodNotFound)
    // still works correctly, confirming the test runs in CI.
    let response = post_messages_listen(None).await;
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
