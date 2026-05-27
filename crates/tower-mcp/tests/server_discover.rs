//! End-to-end tests for the `server/discover` RPC (SEP-2575).
//!
//! `server/discover` is the stateless replacement for `initialize` in the
//! 2026-07-28 protocol. It MUST work without an `Mcp-Session-Id`, without
//! a prior `initialize` handshake, and without affecting any session
//! state -- multiple calls are idempotent.
//!
//! These tests exercise the HTTP transport to confirm the wire shape and
//! the session-independence invariant.

#![cfg(feature = "http")]

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

fn router() -> McpRouter {
    let echo = ToolBuilder::new("echo")
        .description("Echo a value")
        .read_only()
        .handler(|v: serde_json::Value| async move { Ok(CallToolResult::text(v.to_string())) })
        .build();
    McpRouter::new()
        .server_info("discover-test-server", "9.9.9")
        .tool(echo)
}

async fn post_discover() -> serde_json::Value {
    let app = HttpTransport::new(router())
        .disable_origin_validation()
        .into_router();
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        // Deliberately no Mcp-Session-Id and no prior initialize.
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":1,"method":"server/discover"}"#,
        ))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert!(
        response.status().is_success(),
        "expected 2xx, got {}",
        response.status()
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn discover_returns_supported_versions_and_server_info() {
    let resp = post_discover().await;
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    let result = &resp["result"];
    assert!(
        result["supportedVersions"].is_array(),
        "supportedVersions must be an array, got: {result}"
    );
    let versions = result["supportedVersions"].as_array().unwrap();
    assert!(
        !versions.is_empty(),
        "supportedVersions must list at least one version"
    );
    assert!(
        versions.iter().all(|v| v.is_string()),
        "supportedVersions entries must be strings"
    );
    assert_eq!(result["serverInfo"]["name"], "discover-test-server");
    assert_eq!(result["serverInfo"]["version"], "9.9.9");
    assert!(
        result.get("protocolVersion").is_none(),
        "server/discover must NOT include singular protocolVersion -- that's the initialize shape: {result}"
    );
}

#[tokio::test]
async fn discover_does_not_require_initialize() {
    // The whole point of SEP-2575: this works without any prior handshake.
    // post_discover above does exactly that; this test exists as the
    // documented invariant.
    let resp = post_discover().await;
    assert!(resp["result"].is_object());
    assert!(resp.get("error").is_none());
}

#[tokio::test]
async fn discover_is_idempotent() {
    // SEP-2567: tools/list and discovery RPCs MUST NOT depend on per-
    // connection state. Two discover calls back-to-back must return
    // identical capability data.
    let a = post_discover().await;
    let b = post_discover().await;
    assert_eq!(a["result"], b["result"]);
}
