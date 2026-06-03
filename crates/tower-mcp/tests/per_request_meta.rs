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

/// Verify that `stash_per_request_meta` fires in the session-based (2025-11-25)
/// path. Initialize to get a session id, then POST tools/call with a session
/// header AND `_meta` in the body -- assert the handler sees the meta.
/// This guards against accidental removal of the `stash_per_request_meta` call
/// at line ~2506 of http.rs.
#[tokio::test]
async fn session_path_stashes_per_request_meta() {
    let captured = Captured::default();
    let app = HttpTransport::new(router(captured.clone()))
        .disable_origin_validation()
        .into_router();

    // Initialize to create a session.
    let init_req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "session-test", "version": "1.0" }
                }
            })
            .to_string(),
        ))
        .unwrap();
    let init_resp = app.clone().oneshot(init_req).await.unwrap();
    let session_id = init_resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .expect("initialize must return session id");

    // Send notifications/initialized before any other requests (spec requirement).
    let notif_req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Session-Id", &session_id)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            })
            .to_string(),
        ))
        .unwrap();
    app.clone().oneshot(notif_req).await.unwrap();

    // tools/call with session header AND _meta in the body.
    let tool_req = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("Mcp-Session-Id", &session_id)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "inspect_meta",
                    "arguments": {},
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2025-11-25",
                        "io.modelcontextprotocol/clientInfo": {
                            "name": "session-client",
                            "version": "0.1"
                        },
                        "io.modelcontextprotocol/clientCapabilities": {}
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();
    let tool_resp = app.oneshot(tool_req).await.unwrap();
    assert!(
        tool_resp.status().is_success(),
        "expected 2xx, got {}",
        tool_resp.status()
    );
    let bytes = axum::body::to_bytes(tool_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(json.get("error").is_none(), "unexpected error: {json}");

    // The handler must have seen the meta via the session-based path.
    let meta = captured
        .0
        .lock()
        .unwrap()
        .clone()
        .expect("session-based path must stash per-request meta on the handler context");
    assert_eq!(
        meta.protocol_version.as_deref(),
        Some("2025-11-25"),
        "handler must see protocol_version from session-based _meta"
    );
    let info = meta
        .client_info
        .expect("clientInfo must be threaded through");
    assert_eq!(info.name, "session-client");
}

/// Verify that the `log_level` field in `_meta` (`io.modelcontextprotocol/logLevel`)
/// is deserialized and available on the `StatelessRequestMeta` seen by the handler.
#[tokio::test]
async fn handler_sees_log_level_from_meta() {
    let (_, captured) = call_tool(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "inspect_meta",
            "arguments": {},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {
                    "name": "log-test",
                    "version": "1.0"
                },
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/logLevel": "warning"
            }
        }
    }))
    .await;

    let meta = captured
        .0
        .lock()
        .unwrap()
        .clone()
        .expect("meta must be present");
    use tower_mcp::stateless::LogLevel;
    assert_eq!(
        meta.log_level,
        Some(LogLevel::Warning),
        "log_level must be threaded through to the handler context"
    );
}
