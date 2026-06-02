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
}
