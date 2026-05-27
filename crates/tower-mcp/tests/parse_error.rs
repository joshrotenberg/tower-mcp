//! Wire-format integration tests for JSON-RPC error responses from the
//! HTTP transport's parse / validation layer.
//!
//! These tests cover the two error codes that must round-trip with the
//! spec-correct shape when input is unparseable or invalid:
//! - `-32700` Parse error: malformed JSON
//! - `-32600` Invalid Request: well-formed JSON that does not satisfy the
//!   JSON-RPC 2.0 request shape (missing `method`, wrong `jsonrpc`, etc.)
//!
//! Both must produce a response with `jsonrpc == "2.0"`, `id` present
//! (null when unrecoverable), and `error.{code,message}` populated.
//!
//! Regression coverage for #802 / #803 at the transport-integration level.

#![cfg(feature = "http")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};
use tower_mcp_types::testing::assert_jsonrpc_error_response;

fn router() -> McpRouter {
    let echo = ToolBuilder::new("echo")
        .description("Echo")
        .read_only()
        .handler(
            |input: serde_json::Value| async move { Ok(CallToolResult::text(input.to_string())) },
        )
        .build();
    McpRouter::new().tool(echo)
}

async fn post_body(body: &'static str) -> (StatusCode, serde_json::Value) {
    let app = HttpTransport::new(router())
        .disable_origin_validation()
        .into_router();
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(Body::from(body))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

#[tokio::test]
async fn malformed_json_returns_parse_error_with_null_id() {
    let (_status, json) = post_body("not valid json{{{").await;
    assert_jsonrpc_error_response(&json);
    assert!(
        json["id"].is_null(),
        "id must be null when input is unparseable: {json}"
    );
    assert_eq!(json["error"]["code"].as_i64().unwrap(), -32700);
}

#[tokio::test]
async fn truncated_json_returns_parse_error_with_null_id() {
    // Closing brace missing; serde_json's parser errors before recovering id.
    let (_status, json) = post_body(r#"{"jsonrpc":"2.0","id":1,"method":"#).await;
    assert_jsonrpc_error_response(&json);
    assert!(
        json["id"].is_null(),
        "id must be null when the parser cannot reach it: {json}"
    );
    assert_eq!(json["error"]["code"].as_i64().unwrap(), -32700);
}

#[tokio::test]
async fn empty_body_returns_parse_error() {
    let (_status, json) = post_body("").await;
    assert_jsonrpc_error_response(&json);
    assert!(json["id"].is_null());
    assert_eq!(json["error"]["code"].as_i64().unwrap(), -32700);
}
