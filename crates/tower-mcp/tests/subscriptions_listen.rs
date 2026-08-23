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

#[cfg(feature = "stateless")]
fn final_request(id: &str, method: &str, params: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", method)
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

#[cfg(feature = "stateless")]
fn final_meta() -> serde_json::Value {
    serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
    })
}

#[cfg(feature = "stateless")]
async fn response_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[cfg(feature = "stateless")]
async fn next_body_data(body: &mut Body) -> String {
    use http_body_util::BodyExt;

    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), body.frame())
            .await
            .expect("timed out waiting for SSE data")
            .expect("SSE stream ended early")
            .expect("SSE stream failed");
        if let Ok(data) = frame.into_data() {
            return String::from_utf8(data.to_vec()).unwrap();
        }
    }
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
async fn subscription_capacity_rejects_n_plus_one_without_blocking_other_requests() {
    let (app, handle) = HttpTransport::new(router())
        .subscription_limits(tower_mcp::SubscriptionLimits::default().max_active(1))
        .disable_origin_validation()
        .disable_host_validation()
        .into_router_with_handle();
    let listen_params = || {
        serde_json::json!({
            "_meta": final_meta(),
            "notifications": { "resourceSubscriptions": ["file:///wanted"] },
        })
    };

    let first = app
        .clone()
        .oneshot(final_request(
            "listen-capacity-1",
            "subscriptions/listen",
            listen_params(),
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(handle.subscription_count(), 1);

    let rejected = app
        .clone()
        .oneshot(final_request(
            "listen-capacity-2",
            "subscriptions/listen",
            listen_params(),
        ))
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::OK);
    let rejected: serde_json::Value = serde_json::from_str(&response_text(rejected).await).unwrap();
    assert_eq!(rejected["id"], "listen-capacity-2");
    assert_eq!(rejected["error"]["code"], -32603);
    assert_eq!(rejected["error"]["message"], "Subscription limit reached");
    assert_eq!(handle.subscription_count(), 1);

    let tools = app
        .clone()
        .oneshot(final_request(
            "tools-while-full",
            "tools/list",
            serde_json::json!({ "_meta": final_meta() }),
        ))
        .await
        .unwrap();
    assert_eq!(tools.status(), StatusCode::OK);
    let tools: serde_json::Value = serde_json::from_str(&response_text(tools).await).unwrap();
    assert_eq!(tools["id"], "tools-while-full");
    assert_eq!(tools["result"]["tools"][0]["name"], "echo");

    drop(first);
    assert_eq!(handle.subscription_count(), 0);

    let reopened = app
        .oneshot(final_request(
            "listen-capacity-3",
            "subscriptions/listen",
            listen_params(),
        ))
        .await
        .unwrap();
    assert_eq!(reopened.status(), StatusCode::OK);
    assert_eq!(handle.subscription_count(), 1);
    drop(reopened);
    assert_eq!(handle.subscription_count(), 0);
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn authenticated_principal_capacity_isolated_on_the_http_path() {
    use tower_mcp::SubscriptionLimits;
    use tower_mcp::auth::{ApiKeyValidator, AuthLayer};

    let (transport, handle) = HttpTransport::new(router())
        .subscription_limits(
            SubscriptionLimits::default()
                .max_active(3)
                .max_active_per_principal(1),
        )
        .disable_origin_validation()
        .disable_host_validation()
        .into_router_with_handle();
    let app = transport.layer(AuthLayer::new(ApiKeyValidator::new([
        "alice-key-1".to_string(),
        "bob-key-1".to_string(),
    ])));
    let params = || {
        serde_json::json!({
            "_meta": final_meta(),
            "notifications": { "toolsListChanged": true },
        })
    };
    let request = |id: &str, key: &'static str| {
        let mut request = final_request(id, "subscriptions/listen", params());
        request
            .headers_mut()
            .insert("Authorization", axum::http::HeaderValue::from_static(key));
        request
    };

    let alice = app
        .clone()
        .oneshot(request("alice-1", "alice-key-1"))
        .await
        .unwrap();
    assert_eq!(alice.status(), StatusCode::OK);
    assert_eq!(handle.subscription_count(), 1);

    let alice_rejected = app
        .clone()
        .oneshot(request("alice-2", "alice-key-1"))
        .await
        .unwrap();
    assert_eq!(alice_rejected.status(), StatusCode::OK);
    let error: serde_json::Value =
        serde_json::from_str(&response_text(alice_rejected).await).unwrap();
    assert_eq!(error["id"], "alice-2");
    assert_eq!(error["error"]["code"], -32603);

    let bob = app.oneshot(request("bob-1", "bob-key-1")).await.unwrap();
    assert_eq!(bob.status(), StatusCode::OK);
    assert_eq!(handle.subscription_count(), 2);

    drop(alice);
    drop(bob);
    assert_eq!(handle.subscription_count(), 0);
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn notification_overflow_closes_only_the_slow_subscription() {
    use std::sync::{Arc, Mutex};
    use tower_mcp::{
        SubscriptionClose, SubscriptionCloseReason, SubscriptionLimits, SubscriptionObserver,
    };

    #[derive(Default)]
    struct RecordingObserver {
        closes: Mutex<Vec<SubscriptionClose>>,
    }
    impl SubscriptionObserver for RecordingObserver {
        fn on_close(&self, close: SubscriptionClose) {
            self.closes.lock().unwrap().push(close);
        }
    }

    let observer = Arc::new(RecordingObserver::default());
    let router = router().with_subscription_observer(observer.clone());
    let publisher = router.clone();
    let (app, handle) = HttpTransport::new(router)
        .subscription_limits(
            SubscriptionLimits::default()
                .max_active(2)
                .max_buffered_messages(1),
        )
        .disable_origin_validation()
        .disable_host_validation()
        .into_router_with_handle();
    let listen_params = || {
        serde_json::json!({
            "_meta": final_meta(),
            "notifications": { "resourceSubscriptions": ["file:///wanted"] },
        })
    };

    let slow = app
        .clone()
        .oneshot(final_request(
            "listen-slow",
            "subscriptions/listen",
            listen_params(),
        ))
        .await
        .unwrap();
    let healthy = app
        .oneshot(final_request(
            "listen-healthy",
            "subscriptions/listen",
            listen_params(),
        ))
        .await
        .unwrap();
    assert_eq!(handle.subscription_count(), 2);

    let mut healthy_body = healthy.into_body();
    let acknowledgment = next_body_data(&mut healthy_body).await;
    assert!(acknowledgment.contains("notifications/subscriptions/acknowledged"));

    assert!(publisher.notify_resource_updated("file:///wanted"));
    let first = next_body_data(&mut healthy_body).await;
    assert!(first.contains("notifications/resources/updated"));

    // The unread stream still has its first notification buffered, while the
    // healthy stream has made room. A second publish must evict only the slow
    // subscriber.
    assert!(publisher.notify_resource_updated("file:///wanted"));
    assert_eq!(handle.subscription_count(), 1);
    let second = next_body_data(&mut healthy_body).await;
    assert!(second.contains("notifications/resources/updated"));

    let slow = response_text(slow).await;
    let acknowledged = slow
        .find("notifications/subscriptions/acknowledged")
        .expect("overflowed stream must still acknowledge first");
    let terminal = slow
        .find("\"code\":-32603")
        .expect("overflowed stream must end with an internal JSON-RPC error");
    assert!(
        acknowledged < terminal,
        "acknowledgment must precede error: {slow}"
    );
    assert!(slow.contains("Subscription notification buffer exceeded"));
    assert!(
        !slow.contains("notifications/resources/updated"),
        "queued data is discarded when a stream overflows: {slow}"
    );

    {
        let closes = observer.closes.lock().unwrap();
        let overflows: Vec<_> = closes
            .iter()
            .filter(|close| {
                close.subscription_id == tower_mcp::RequestId::String("listen-slow".into())
            })
            .collect();
        assert_eq!(overflows.len(), 1, "overflow is observed exactly once");
        assert_eq!(overflows[0].reason, SubscriptionCloseReason::BufferOverflow);
    }

    assert_eq!(handle.close_subscriptions(), 1);
    let rest = axum::body::to_bytes(healthy_body, usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8(rest.to_vec())
            .unwrap()
            .contains("\"resultType\":\"complete\"")
    );
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
    let notification = body
        .find("notifications/resources/updated")
        .expect("queued notifications must be drained");
    let completion = body
        .find("\"resultType\":\"complete\"")
        .expect("stream must end with a graceful result");
    assert!(acknowledgment < notification);
    assert!(notification < completion);
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
                    "notifications": { "taskIds": [task_id] },
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
            body.contains(&format!("\"taskIds\":[\"{task_id}\"]")),
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

    /// Closing an overloaded notification stream must not affect the task
    /// store: clients can reconnect and recover the authoritative state with
    /// `tasks/get`.
    #[tokio::test]
    async fn task_state_remains_available_after_subscription_buffer_overflow() {
        let delayed = ToolBuilder::new("delayed")
            .description("Complete after the listen stream has been registered")
            .task_support(TaskSupportMode::Optional)
            .handler(|_: serde_json::Value| async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                Ok(CallToolResult::text("done"))
            })
            .build();
        let task_router = McpRouter::new()
            .server_info("listen-test-server", "1.0.0")
            .tool(delayed)
            .with_tasks();
        let (app, handle) = HttpTransport::new(task_router)
            .subscription_limits(tower_mcp::SubscriptionLimits::default().max_buffered_messages(0))
            .disable_origin_validation()
            .disable_host_validation()
            .into_router_with_handle();

        let create = app
            .clone()
            .oneshot(final_request(
                "call-overflow",
                "tools/call",
                serde_json::json!({
                    "_meta": meta(true),
                    "name": "delayed",
                    "arguments": {},
                }),
            ))
            .await
            .unwrap();
        assert_eq!(create.status(), StatusCode::OK);
        let created: serde_json::Value = serde_json::from_str(&body_string(create).await).unwrap();
        let task_id = created["result"]["taskId"].as_str().unwrap().to_string();

        let listen = app
            .clone()
            .oneshot(final_request(
                "listen-overflow",
                "subscriptions/listen",
                serde_json::json!({
                    "_meta": meta(true),
                    "notifications": { "taskIds": [&task_id] },
                }),
            ))
            .await
            .unwrap();
        assert_eq!(listen.status(), StatusCode::OK);
        assert_eq!(handle.subscription_count(), 1);

        let body = tokio::time::timeout(Duration::from_secs(10), body_string(listen))
            .await
            .expect("task completion must overflow and close the listen stream");
        let acknowledgment = body
            .find("notifications/subscriptions/acknowledged")
            .expect("overflowed stream must acknowledge first");
        let terminal = body
            .find("\"code\":-32603")
            .expect("overflowed stream must end with an internal JSON-RPC error");
        assert!(acknowledgment < terminal);
        assert!(body.contains("Subscription notification buffer exceeded"));
        assert_eq!(handle.subscription_count(), 0);

        let mut get_request = final_request(
            "get-after-overflow",
            "tasks/get",
            serde_json::json!({ "_meta": meta(true), "taskId": &task_id }),
        );
        get_request
            .headers_mut()
            .insert("Mcp-Name", task_id.parse().unwrap());
        let get = app.oneshot(get_request).await.unwrap();
        let status = get.status();
        let get_body = body_string(get).await;
        assert_eq!(status, StatusCode::OK, "unexpected tasks/get: {get_body}");
        let get: serde_json::Value = serde_json::from_str(&get_body).unwrap();
        assert_eq!(get["id"], "get-after-overflow");
        assert_eq!(get["result"]["taskId"], task_id);
        assert_eq!(get["result"]["status"], "completed");
    }

    /// A subscriber that named a different owned task never receives this one.
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

        let other = app
            .clone()
            .oneshot(final_request(
                "call-2",
                "tools/call",
                serde_json::json!({
                    "_meta": meta(true),
                    "name": "slow",
                    "arguments": {},
                }),
            ))
            .await
            .unwrap();
        let other: serde_json::Value = serde_json::from_str(&body_string(other).await).unwrap();
        let other_task_id = other["result"]["taskId"].as_str().unwrap().to_string();

        let listen = app
            .clone()
            .oneshot(final_request(
                "listen-1",
                "subscriptions/listen",
                serde_json::json!({
                    "_meta": meta(true),
                    "notifications": { "taskIds": [other_task_id] },
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

    #[cfg(feature = "oauth")]
    fn oauth_token(subject: &str, audience: &str) -> String {
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &serde_json::json!({
                "sub": subject,
                "aud": audience,
                "scope": "mcp:read",
            }),
            &jsonwebtoken::EncodingKey::from_secret(b"subscription-auth-test-secret"),
        )
        .unwrap()
    }

    #[cfg(feature = "oauth")]
    fn authenticated_final_request(
        id: &str,
        method: &str,
        params: serde_json::Value,
        token: &str,
    ) -> Request<Body> {
        let mut request = final_request(id, method, params);
        request.headers_mut().insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        request
    }

    /// The protected HTTP builder must carry validated claims through the
    /// transport-owned listen path, and the router must enforce TaskOwner
    /// before registering the stream.
    #[cfg(feature = "oauth")]
    #[tokio::test]
    async fn oauth_listen_accepts_only_the_task_owner() {
        const RESOURCE: &str = "https://mcp.example.com/subscriptions";

        let metadata = tower_mcp::oauth::ProtectedResourceMetadata::new(RESOURCE)
            .authorization_server("https://auth.example.com")
            .scope("mcp:read");
        let validator =
            tower_mcp::oauth::JwtValidator::from_secret(b"subscription-auth-test-secret")
                .disable_exp_validation();
        let (app, handle) = HttpTransport::new(task_router())
            .disable_origin_validation()
            .disable_host_validation()
            .into_oauth_router_with_handle(
                validator,
                metadata,
                tower_mcp::oauth::ScopePolicy::new().default_scope("mcp:read"),
            )
            .unwrap();
        let alice = oauth_token("alice", RESOURCE);
        let bob = oauth_token("bob", RESOURCE);

        let create = app
            .clone()
            .oneshot(authenticated_final_request(
                "call-alice",
                "tools/call",
                serde_json::json!({
                    "_meta": meta(true),
                    "name": "slow",
                    "arguments": {},
                }),
                &alice,
            ))
            .await
            .unwrap();
        assert_eq!(create.status(), StatusCode::OK);
        let created: serde_json::Value = serde_json::from_str(&body_string(create).await).unwrap();
        let task_id = created["result"]["taskId"].as_str().unwrap().to_string();

        let listen_params = |task_id: &str| {
            serde_json::json!({
                "_meta": meta(true),
                "notifications": { "taskIds": [task_id] },
            })
        };
        let foreign = app
            .clone()
            .oneshot(authenticated_final_request(
                "listen-bob",
                "subscriptions/listen",
                listen_params(&task_id),
                &bob,
            ))
            .await
            .unwrap();
        assert_eq!(foreign.status(), StatusCode::BAD_REQUEST);
        let foreign: serde_json::Value = serde_json::from_str(&body_string(foreign).await).unwrap();

        let missing_id = "task_that_was_never_issued";
        let missing = app
            .clone()
            .oneshot(authenticated_final_request(
                "listen-missing",
                "subscriptions/listen",
                listen_params(missing_id),
                &bob,
            ))
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
        let missing: serde_json::Value = serde_json::from_str(&body_string(missing).await).unwrap();
        assert_eq!(foreign["error"]["code"], missing["error"]["code"]);
        assert_eq!(foreign["error"]["data"], missing["error"]["data"]);
        assert_eq!(
            foreign["error"]["message"]
                .as_str()
                .unwrap()
                .replace(&task_id, "<id>"),
            missing["error"]["message"]
                .as_str()
                .unwrap()
                .replace(missing_id, "<id>"),
            "a foreign task must look exactly like a missing task"
        );

        let mixed = app
            .clone()
            .oneshot(authenticated_final_request(
                "listen-mixed",
                "subscriptions/listen",
                serde_json::json!({
                    "_meta": meta(true),
                    "notifications": { "taskIds": [&task_id, missing_id] },
                }),
                &alice,
            ))
            .await
            .unwrap();
        assert_eq!(mixed.status(), StatusCode::BAD_REQUEST);
        let mixed: serde_json::Value = serde_json::from_str(&body_string(mixed).await).unwrap();
        assert_eq!(mixed["error"]["code"], missing["error"]["code"]);
        assert_eq!(handle.subscription_count(), 0);

        let anonymous = app
            .clone()
            .oneshot(final_request(
                "listen-anonymous",
                "subscriptions/listen",
                listen_params(&task_id),
            ))
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

        let owned = app
            .oneshot(authenticated_final_request(
                "listen-alice",
                "subscriptions/listen",
                listen_params(&task_id),
                &alice,
            ))
            .await
            .unwrap();
        assert_eq!(owned.status(), StatusCode::OK);
        assert_eq!(handle.subscription_count(), 1);
        assert_eq!(handle.close_subscriptions(), 1);
        let body = body_string(owned).await;
        assert!(body.contains("notifications/subscriptions/acknowledged"));
        assert!(body.contains(&task_id));
    }

    #[derive(Clone)]
    struct ApplicationPrincipal(String);

    async fn attach_application_principal(
        mut request: Request<Body>,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        if let Some(subject) = request
            .headers()
            .get("x-test-principal")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
        {
            request
                .extensions_mut()
                .insert(ApplicationPrincipal(subject));
        }
        next.run(request).await
    }

    fn application_request(
        id: &str,
        method: &str,
        params: serde_json::Value,
        subject: Option<&str>,
    ) -> Request<Body> {
        let mut request = final_request(id, method, params);
        if let Some(subject) = subject {
            request
                .headers_mut()
                .insert("x-test-principal", subject.parse().unwrap());
        }
        request
    }

    /// A host-authenticated extension crosses the HTTP bridge for both
    /// creation and the transport-owned subscription path.
    #[tokio::test]
    async fn application_principal_listen_accepts_only_the_task_owner() {
        let router = task_router().task_owner_from_extension::<ApplicationPrincipal>(|principal| {
            format!("https://identity.example#{}", principal.0)
        });
        let (app, handle) = HttpTransport::new(router)
            .disable_origin_validation()
            .disable_host_validation()
            .bridge_extension::<ApplicationPrincipal>()
            .into_router_with_handle();
        let app = app.layer(axum::middleware::from_fn(attach_application_principal));

        let create = app
            .clone()
            .oneshot(application_request(
                "call-alice",
                "tools/call",
                serde_json::json!({
                    "_meta": meta(true),
                    "name": "slow",
                    "arguments": {},
                }),
                Some("alice"),
            ))
            .await
            .unwrap();
        assert_eq!(create.status(), StatusCode::OK);
        let created: serde_json::Value = serde_json::from_str(&body_string(create).await).unwrap();
        let task_id = created["result"]["taskId"].as_str().unwrap().to_string();

        let listen_params = || {
            serde_json::json!({
                "_meta": meta(true),
                "notifications": { "taskIds": [&task_id] },
            })
        };
        for (id, subject) in [("listen-bob", Some("bob")), ("listen-anon", None)] {
            let denied = app
                .clone()
                .oneshot(application_request(
                    id,
                    "subscriptions/listen",
                    listen_params(),
                    subject,
                ))
                .await
                .unwrap();
            assert_eq!(denied.status(), StatusCode::BAD_REQUEST);
            let denied: serde_json::Value =
                serde_json::from_str(&body_string(denied).await).unwrap();
            assert_eq!(denied["error"]["code"], -32602);
            assert!(
                denied["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("not found")
            );
        }
        assert_eq!(handle.subscription_count(), 0);

        let owned = app
            .oneshot(application_request(
                "listen-alice",
                "subscriptions/listen",
                listen_params(),
                Some("alice"),
            ))
            .await
            .unwrap();
        assert_eq!(owned.status(), StatusCode::OK);
        assert_eq!(handle.subscription_count(), 1);
        assert_eq!(handle.close_subscriptions(), 1);
        let body = body_string(owned).await;
        assert!(body.contains("notifications/subscriptions/acknowledged"));
        assert!(body.contains(&task_id));
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
