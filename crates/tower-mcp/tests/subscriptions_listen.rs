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

// =============================================================================
// Task status notifications (SEP-2663)
// =============================================================================

#[cfg(feature = "stateless")]
mod tasks {
    use super::*;
    use std::time::Duration;
    use tower_mcp::TaskSupportMode;

    const TASKS_EXTENSION: &str = "io.modelcontextprotocol/tasks";

    fn task_router() -> McpRouter {
        let slow = ToolBuilder::new("slow")
            .description("Finish after a beat, so the task is observably working first")
            .task_support(TaskSupportMode::Optional)
            .handler(|_: serde_json::Value| async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(CallToolResult::text("done"))
            })
            .build();
        McpRouter::new()
            .server_info("listen-test-server", "1.0.0")
            .tool(slow)
            .with_tasks()
    }

    fn final_request(id: &str, method: &str, params: serde_json::Value) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Mcp-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", method);
        // SEP-2243 requires Mcp-Name to mirror params.name on tools/call.
        if let Some(name) = params.get("name").and_then(serde_json::Value::as_str) {
            builder = builder.header("Mcp-Name", name);
        }
        builder
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                })
                .to_string(),
            ))
            .unwrap()
    }

    fn meta(declares_tasks: bool) -> serde_json::Value {
        let capabilities = if declares_tasks {
            serde_json::json!({ "extensions": { TASKS_EXTENSION: {} } })
        } else {
            serde_json::json!({})
        };
        serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": capabilities,
        })
    }

    async fn body_string(response: axum::response::Response) -> String {
        drain(response.into_body()).await
    }

    async fn drain(body: Body) -> String {
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// Read SSE frames until `needle` appears, returning what was read and the
    /// still-open body.
    ///
    /// A listen stream only terminates once `close_subscriptions()` fires, so a
    /// test cannot collect the whole body and then assert on it: it has to
    /// close the stream first. Reading frame by frame lets the test wait for
    /// the notification it is asserting about, rather than sleeping long enough
    /// to probably cover it. The router records a task's terminal state and
    /// publishes `notifications/tasks` at two separate points, so a timed close
    /// can land between them.
    async fn read_until(response: axum::response::Response, needle: &str) -> (String, Body) {
        use http_body_util::BodyExt;

        let mut body = response.into_body();
        let mut seen = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);

        loop {
            let frame = tokio::time::timeout_at(deadline, body.frame())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}; saw: {seen}"));
            match frame {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        seen.push_str(&String::from_utf8_lossy(&data));
                        if seen.contains(needle) {
                            return (seen, body);
                        }
                    }
                }
                Some(Err(e)) => panic!("stream failed while waiting for {needle:?}: {e}"),
                None => panic!("stream ended before {needle:?} arrived; saw: {seen}"),
            }
        }
    }

    /// Poll `tasks/get` until the task is no longer `working`.
    ///
    /// A test asserting that a notification did *not* reach a stream only means
    /// something once the transition it would have announced has happened.
    async fn await_terminal(app: &axum::Router, task_id: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let response = app
                .clone()
                .oneshot(final_request(
                    "get-1",
                    "tasks/get",
                    serde_json::json!({ "_meta": meta(true), "taskId": task_id }),
                ))
                .await
                .unwrap();
            let value: serde_json::Value =
                serde_json::from_str(&body_string(response).await).unwrap();
            if value["result"]["status"] != "working" {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "task {task_id} never left `working`"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A task subscriber sees the terminal transition even though the
    /// `tools/call` that created the task has already returned.
    #[tokio::test]
    async fn completing_a_task_reaches_a_subscribed_listen_stream() {
        let (app, handle) = HttpTransport::new(task_router())
            .disable_origin_validation()
            .disable_host_validation()
            .into_router_with_handle();

        let create = app
            .clone()
            .oneshot(final_request(
                "call-1",
                "tools/call",
                serde_json::json!({
                    "_meta": meta(true),
                    "name": "slow",
                    "arguments": {},
                }),
            ))
            .await
            .unwrap();
        assert_eq!(create.status(), StatusCode::OK);
        let created: serde_json::Value = serde_json::from_str(&body_string(create).await).unwrap();
        let task_id = created["result"]["taskId"]
            .as_str()
            .expect("final create result carries a flat taskId")
            .to_string();

        let listen = app
            .oneshot(final_request(
                "listen-1",
                "subscriptions/listen",
                serde_json::json!({
                    "_meta": meta(true),
                    "notifications": { "taskIds": [task_id, "some-other-task"] },
                }),
            ))
            .await
            .unwrap();
        assert_eq!(listen.status(), StatusCode::OK);
        assert_eq!(handle.subscription_count(), 1);

        // Wait for the notification itself, then drain so the body terminates.
        let (seen, rest) = read_until(listen, "notifications/tasks").await;
        assert_eq!(handle.close_subscriptions(), 1);

        let body = format!("{seen}{}", drain(rest).await);
        assert!(
            body.contains("notifications/subscriptions/acknowledged"),
            "stream must begin with an acknowledgment: {body}"
        );
        assert!(
            body.contains(&format!("\"taskIds\":[\"{task_id}\",\"some-other-task\"]")),
            "the acknowledgment must echo the accepted task IDs: {body}"
        );
        assert!(
            body.contains("notifications/tasks"),
            "the completion must reach the stream: {body}"
        );
        assert!(
            body.contains("\"status\":\"completed\""),
            "the notification must carry the terminal status: {body}"
        );
        assert!(
            body.contains(&format!("\"taskId\":\"{task_id}\"")),
            "the notification must name the task: {body}"
        );
        assert!(
            body.contains("\"io.modelcontextprotocol/subscriptionId\":\"listen-1\""),
            "the notification must be tagged with the subscription: {body}"
        );
    }

    /// A subscriber that named a different task hears nothing.
    #[tokio::test]
    async fn an_unnamed_task_never_reaches_a_stream() {
        let (app, handle) = HttpTransport::new(task_router())
            .disable_origin_validation()
            .disable_host_validation()
            .into_router_with_handle();

        let create = app
            .clone()
            .oneshot(final_request(
                "call-1",
                "tools/call",
                serde_json::json!({
                    "_meta": meta(true),
                    "name": "slow",
                    "arguments": {},
                }),
            ))
            .await
            .unwrap();
        let created: serde_json::Value = serde_json::from_str(&body_string(create).await).unwrap();
        let task_id = created["result"]["taskId"].as_str().unwrap().to_string();

        let listen = app
            .clone()
            .oneshot(final_request(
                "listen-1",
                "subscriptions/listen",
                serde_json::json!({
                    "_meta": meta(true),
                    "notifications": { "taskIds": ["a-task-this-client-does-not-own"] },
                }),
            ))
            .await
            .unwrap();
        assert_eq!(listen.status(), StatusCode::OK);

        // The task has to actually finish for its absence here to mean
        // anything: otherwise this passes because nothing has happened yet.
        await_terminal(&app, &task_id).await;
        handle.close_subscriptions();

        let body = body_string(listen).await;
        assert!(
            !body.contains("notifications/tasks"),
            "a task the stream did not name must not appear: {body}"
        );
        assert!(
            !body.contains(&task_id),
            "the unrelated task ID must not leak: {body}"
        );
    }

    /// SEP-2663: requesting task notifications without declaring the extension
    /// is answered with the missing-capability error, not a silent drop.
    #[tokio::test]
    async fn task_ids_without_the_extension_are_rejected() {
        let app = HttpTransport::new(task_router())
            .disable_origin_validation()
            .disable_host_validation()
            .into_router();

        let response = app
            .oneshot(final_request(
                "listen-1",
                "subscriptions/listen",
                serde_json::json!({
                    "_meta": meta(false),
                    "notifications": { "taskIds": ["task-a"] },
                }),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["id"], "listen-1");
        assert_eq!(body["error"]["code"], -32021);
        assert!(
            body["error"]["data"]["requiredCapabilities"]["extensions"][TASKS_EXTENSION]
                .is_object(),
            "the error must name the extension the client is missing: {body}"
        );
    }

    /// A server without the extension acknowledges the rest of the filter and
    /// declines the task IDs rather than promising notifications it will never
    /// send.
    #[tokio::test]
    async fn a_server_without_tasks_declines_the_task_ids() {
        let (app, handle) = HttpTransport::new(router())
            .disable_origin_validation()
            .disable_host_validation()
            .into_router_with_handle();

        let response = app
            .oneshot(final_request(
                "listen-1",
                "subscriptions/listen",
                serde_json::json!({
                    "_meta": meta(true),
                    "notifications": { "taskIds": ["task-a"], "toolsListChanged": true },
                }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        handle.close_subscriptions();

        let body = body_string(response).await;
        assert!(body.contains("notifications/subscriptions/acknowledged"));
        assert!(
            body.contains("\"toolsListChanged\":true"),
            "the rest of the filter still stands: {body}"
        );
        assert!(
            !body.contains("taskIds"),
            "a server without the extension must not acknowledge task IDs: {body}"
        );
    }
}
