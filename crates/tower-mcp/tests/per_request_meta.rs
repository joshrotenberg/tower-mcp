//! SEP-2575 per-request `_meta` plumbing: when an incoming JSON-RPC request
//! has an `_meta` object with the spec-keyed fields, the HTTP transport
//! deserializes it into `StatelessRequestMeta` and stashes it on the
//! `RequestContext` extensions so handlers can read it via
//! `ctx.per_request_meta()`.

#![cfg(all(feature = "http", feature = "stateless"))]

use std::sync::Arc;
use std::sync::Mutex;

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;
use tower_mcp::extract::{Context, RawArgs, State};
use tower_mcp::stateless::StatelessRequestMeta;
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

/// Captures the per-request meta the handler saw, so the test can assert
/// on it after the HTTP round-trip.
#[derive(Default, Clone)]
struct Captured(Arc<Mutex<Option<StatelessRequestMeta>>>);

fn router(captured: Captured) -> McpRouter {
    let tool = ToolBuilder::new("inspect_meta")
        .description("Returns the protocolVersion from the per-request meta, if any.")
        .read_only()
        .extractor_handler(
            captured,
            |State(captured): State<Captured>, ctx: Context, RawArgs(_args): RawArgs| async move {
                let meta = ctx.per_request_meta().cloned();
                let answer = meta
                    .as_ref()
                    .and_then(|m| m.protocol_version.clone())
                    .unwrap_or_else(|| "absent".into());
                *captured.0.lock().unwrap() = meta;
                Ok(CallToolResult::text(answer))
            },
        )
        .build();
    McpRouter::new().tool(tool)
}

async fn call_tool(body: serde_json::Value) -> (serde_json::Value, Captured) {
    let captured = Captured::default();
    let app = HttpTransport::new(router(captured.clone()))
        .disable_origin_validation()
        .into_router();
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(Body::from(body.to_string()))
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
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    (json, captured)
}

#[tokio::test]
async fn handler_sees_per_request_meta_from_io_namespace() {
    // SEP-2575 reverse-DNS keys. Handler should observe protocolVersion +
    // clientInfo from the per-request meta.
    let (resp, captured) = call_tool(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "inspect_meta",
            "arguments": {},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {
                    "name": "test-client",
                    "version": "9.9.9"
                },
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    }))
    .await;

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, "2026-07-28");

    let meta = captured
        .0
        .lock()
        .unwrap()
        .clone()
        .expect("per-request meta must be present on the handler context");
    assert_eq!(meta.protocol_version.as_deref(), Some("2026-07-28"));
    let info = meta.client_info.expect("clientInfo carried through");
    assert_eq!(info.name, "test-client");
    assert_eq!(info.version, "9.9.9");
    assert!(meta.client_capabilities.is_some());
}

#[tokio::test]
async fn handler_sees_none_when_no_meta_present() {
    let (resp, captured) = call_tool(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "inspect_meta",
            "arguments": {}
        }
    }))
    .await;

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, "absent");
    assert!(captured.0.lock().unwrap().is_none());
}

#[tokio::test]
async fn legacy_namespace_keys_are_silently_dropped() {
    // The SEP-1442 draft `modelcontextprotocol.io/...` prefix is no longer
    // recognized. The meta deserializes, but the legacy keys map to no
    // field; the handler sees an empty StatelessRequestMeta.
    let (_resp, captured) = call_tool(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "inspect_meta",
            "arguments": {},
            "_meta": {
                "modelcontextprotocol.io/mcpProtocolVersion": "2025-11-25",
                "modelcontextprotocol.io/sessionId": "abc"
            }
        }
    }))
    .await;

    let meta = captured.0.lock().unwrap().clone().unwrap();
    assert!(meta.protocol_version.is_none(), "legacy key must not match");
    assert!(meta.client_info.is_none());
}
