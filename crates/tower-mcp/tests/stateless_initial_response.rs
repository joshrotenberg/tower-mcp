//! The initial-response policy for silent 2026-07-28 calls (#1620): a handler
//! that has produced nothing when the initial-response timeout elapses gets a
//! committed SSE response, so proxies with response-header or idle timeouts see
//! traffic while it runs.
//!
//! Everything runs in-process under paused time, so the timing assertions are
//! exact and the races below are deterministic.

#![cfg(all(feature = "http", feature = "stateless"))]

use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use http_body_util::BodyExt;
use tokio::time::Instant;
use tower::ServiceExt;
use tower_mcp::extract::{Context, RawArgs};
use tower_mcp::protocol::{LogLevel, LoggingMessageParams};
use tower_mcp::router::{RouterRequest, RouterResponse};
use tower_mcp::{CallToolResult, HttpTransport, JsonRpcError, McpRouter, ToolBuilder};

/// The documented default initial-response timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Fails a hung test. Paused time auto-advances, so a test that deadlocks
/// reaches this bound immediately instead of hanging.
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(4 * 3600), future)
        .await
        .expect("test exceeded its time bound")
}

fn log(ctx: &Context, message: &str) {
    ctx.send_log(LoggingMessageParams {
        level: LogLevel::Info,
        logger: Some("initial-response-test".into()),
        data: serde_json::json!({ "message": message }),
        meta: None,
    });
}

/// A tool that sleeps for `delay`, then answers.
fn sleeper(delay: Duration) -> tower_mcp::Tool {
    ToolBuilder::new("work")
        .description("Sleeps, then answers.")
        .extractor_handler((), move |RawArgs(_): RawArgs| async move {
            tokio::time::sleep(delay).await;
            Ok(CallToolResult::text("done"))
        })
        .build()
}

fn transport(tool: tower_mcp::Tool) -> HttpTransport {
    HttpTransport::new(
        McpRouter::new()
            .server_info("initial-response-test", "1.0.0")
            .tool(tool),
    )
    .disable_origin_validation()
    .disable_host_validation()
}

fn final_request(method: &str, name: Option<&str>) -> Request<Body> {
    let mut params = serde_json::json!({
        "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
            "io.modelcontextprotocol/logLevel": "info"
        }
    });
    let mut builder = Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", method);
    if let Some(name) = name {
        params["name"] = name.into();
        params["arguments"] = serde_json::json!({});
        builder = builder.header("Mcp-Name", name);
    }
    builder
        .body(Body::from(
            serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
                .to_string(),
        ))
        .unwrap()
}

fn content_type(response: &Response) -> &str {
    response.headers()["content-type"].to_str().unwrap()
}

/// One received SSE frame and when it arrived, relative to `start`.
struct Frame {
    at: Duration,
    text: String,
}

/// Read a body to its end, stamping each data frame with its arrival time.
async fn frames(response: Response, start: Instant) -> Vec<Frame> {
    let mut body = response.into_body();
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame.expect("body frame").into_data() {
            out.push(Frame {
                at: start.elapsed(),
                text: String::from_utf8(data.to_vec()).unwrap(),
            });
        }
    }
    out
}

/// The JSON-RPC payloads of the `data:` lines across frames, in order.
fn messages(frames: &[Frame]) -> Vec<serde_json::Value> {
    frames
        .iter()
        .flat_map(|frame| frame.text.lines())
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| serde_json::from_str(data.trim()).unwrap())
        .collect()
}

fn comments(frames: &[Frame]) -> Vec<Duration> {
    frames
        .iter()
        .filter(|frame| frame.text.starts_with(':'))
        .map(|frame| frame.at)
        .collect()
}

/// Assert the only terminal message is the last one and carries `text`.
fn assert_single_result(messages: &[serde_json::Value], text: &str) {
    let terminals: Vec<_> = messages.iter().filter(|m| m.get("id").is_some()).collect();
    assert_eq!(terminals.len(), 1, "exactly one terminal: {messages:?}");
    assert_eq!(terminals[0]["id"], 1);
    assert_eq!(terminals[0]["result"]["content"][0]["text"], text);
    assert!(
        messages.last().unwrap().get("id").is_some(),
        "terminal last"
    );
}

#[tokio::test(start_paused = true)]
async fn silent_call_commits_sse_at_the_default_deadline_and_keeps_alive() {
    bounded(async {
        let delay = Duration::from_secs(100);
        let app = transport(sleeper(delay)).into_router();
        let start = Instant::now();

        let response = app
            .oneshot(final_request("tools/call", Some("work")))
            .await
            .unwrap();

        assert_eq!(
            start.elapsed(),
            DEFAULT_TIMEOUT,
            "head waits only for the deadline"
        );
        assert_eq!(response.status(), StatusCode::OK);
        assert!(content_type(&response).starts_with("text/event-stream"));
        assert_eq!(response.headers()["mcp-protocol-version"], "2026-07-28");
        assert!(!response.headers().contains_key("mcp-session-id"));

        let frames = frames(response, start).await;
        assert_eq!(
            comments(&frames),
            [30, 60, 90].map(Duration::from_secs),
            "a comment at commit, then one per keepalive interval"
        );
        assert_single_result(&messages(&frames), "done");
        assert_eq!(frames.last().unwrap().at, delay);
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn interval_is_configurable() {
    bounded(async {
        let timeout = Duration::from_secs(5);
        let app = transport(sleeper(Duration::from_secs(8)))
            .initial_response_timeout(Some(timeout))
            .into_router();
        let start = Instant::now();

        let response = app
            .oneshot(final_request("tools/call", Some("work")))
            .await
            .unwrap();

        assert_eq!(start.elapsed(), timeout);
        assert!(content_type(&response).starts_with("text/event-stream"));
        assert_single_result(&messages(&frames(response, start).await), "done");
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn none_disables_the_deadline() {
    bounded(async {
        let delay = Duration::from_secs(300);
        let app = transport(sleeper(delay))
            .initial_response_timeout(None)
            .into_router();
        let start = Instant::now();

        let response = app
            .oneshot(final_request("tools/call", Some("work")))
            .await
            .unwrap();

        assert_eq!(start.elapsed(), delay);
        assert_eq!(content_type(&response), "application/json");
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn prompt_result_stays_json() {
    bounded(async {
        let delay = Duration::from_secs(1);
        let app = transport(sleeper(delay)).into_router();
        let start = Instant::now();

        let response = app
            .oneshot(final_request("tools/call", Some("work")))
            .await
            .unwrap();

        assert_eq!(start.elapsed(), delay);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(content_type(&response), "application/json");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["result"]["content"][0]["text"], "done");
    })
    .await;
}

/// A pre-built service that answers `-32601` after `delay`, which maps to
/// HTTP 404 when it is still possible to set a status.
fn failing_service(delay: Duration) -> axum::Router {
    let service = tower::service_fn(move |request: RouterRequest| async move {
        tokio::time::sleep(delay).await;
        Ok::<_, Infallible>(RouterResponse {
            id: request.id,
            inner: Err(JsonRpcError::method_not_found("gone")),
        })
    });
    HttpTransport::from_service(service)
        .disable_origin_validation()
        .disable_host_validation()
        .into_router()
}

#[tokio::test(start_paused = true)]
async fn error_before_the_deadline_keeps_its_http_status() {
    bounded(async {
        let delay = Duration::from_secs(10);
        let start = Instant::now();

        let response = failing_service(delay)
            .oneshot(final_request("tools/list", None))
            .await
            .unwrap();

        assert_eq!(start.elapsed(), delay);
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(content_type(&response), "application/json");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], -32601);
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn error_after_the_commit_travels_in_band_under_200() {
    bounded(async {
        let start = Instant::now();

        let response = failing_service(Duration::from_secs(45))
            .oneshot(final_request("tools/list", None))
            .await
            .unwrap();

        assert_eq!(start.elapsed(), DEFAULT_TIMEOUT);
        assert_eq!(response.status(), StatusCode::OK);
        assert!(content_type(&response).starts_with("text/event-stream"));

        let messages = messages(&frames(response, start).await);
        assert_eq!(messages.len(), 1, "exactly one terminal: {messages:?}");
        assert_eq!(messages[0]["id"], 1);
        assert_eq!(messages[0]["error"]["code"], -32601);
    })
    .await;
}

/// A tool that runs `steps` in order: wait, then log the given message.
fn logger(steps: &'static [(u64, Option<&'static str>)]) -> tower_mcp::Tool {
    ToolBuilder::new("work")
        .description("Sleeps and logs on a schedule, then answers.")
        .extractor_handler((), move |ctx: Context, RawArgs(_): RawArgs| async move {
            for (seconds, message) in steps {
                tokio::time::sleep(Duration::from_secs(*seconds)).await;
                if let Some(message) = message {
                    log(&ctx, message);
                }
            }
            Ok(CallToolResult::text("done"))
        })
        .build()
}

fn log_texts(messages: &[serde_json::Value]) -> Vec<&str> {
    messages
        .iter()
        .filter(|m| m["method"] == "notifications/message")
        .map(|m| m["params"]["data"]["message"].as_str().unwrap())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn result_ready_at_the_deadline_wins_and_stays_json() {
    bounded(async {
        let app = transport(sleeper(DEFAULT_TIMEOUT)).into_router();
        let start = Instant::now();

        let response = app
            .oneshot(final_request("tools/call", Some("work")))
            .await
            .unwrap();

        assert_eq!(start.elapsed(), DEFAULT_TIMEOUT);
        assert_eq!(content_type(&response), "application/json");
    })
    .await;
}

/// The handler logs in the same poll the deadline fires and keeps running.
/// Either way the response is SSE under 200; the notification follows the
/// commit and is delivered exactly once.
#[tokio::test(start_paused = true)]
async fn notification_at_the_deadline_is_delivered_once() {
    bounded(async {
        let app = transport(logger(&[(30, Some("at deadline")), (1, None)])).into_router();
        let start = Instant::now();

        let response = app
            .oneshot(final_request("tools/call", Some("work")))
            .await
            .unwrap();

        assert_eq!(start.elapsed(), DEFAULT_TIMEOUT);
        assert_eq!(response.status(), StatusCode::OK);
        assert!(content_type(&response).starts_with("text/event-stream"));
        let messages = messages(&frames(response, start).await);
        assert_eq!(log_texts(&messages), ["at deadline"]);
        assert_single_result(&messages, "done");
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn notification_and_result_ready_at_the_deadline_keep_their_order() {
    bounded(async {
        let app = transport(logger(&[(30, Some("last words"))])).into_router();
        let start = Instant::now();

        let response = app
            .oneshot(final_request("tools/call", Some("work")))
            .await
            .unwrap();

        assert_eq!(start.elapsed(), DEFAULT_TIMEOUT);
        assert!(content_type(&response).starts_with("text/event-stream"));
        let messages = messages(&frames(response, start).await);
        assert_eq!(log_texts(&messages), ["last words"]);
        assert_single_result(&messages, "done");
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn notifications_after_the_commit_are_kept_in_order() {
    bounded(async {
        let app = transport(logger(&[
            (40, Some("one")),
            (0, Some("two")),
            (5, Some("three")),
        ]))
        .into_router();
        let start = Instant::now();

        let response = app
            .oneshot(final_request("tools/call", Some("work")))
            .await
            .unwrap();

        assert_eq!(start.elapsed(), DEFAULT_TIMEOUT);
        let frames = frames(response, start).await;
        assert_eq!(comments(&frames), [DEFAULT_TIMEOUT]);
        let messages = messages(&frames);
        assert_eq!(log_texts(&messages), ["one", "two", "three"]);
        assert_single_result(&messages, "done");
    })
    .await;
}

/// A subscription-scoped notification does not count as a response on this
/// POST, so it neither upgrades the response nor restarts the deadline.
#[tokio::test(start_paused = true)]
async fn subscription_notifications_do_not_restart_the_deadline() {
    bounded(async {
        let tool = ToolBuilder::new("work")
            .description("Touches a resource, then keeps working.")
            .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
                tokio::time::sleep(Duration::from_secs(20)).await;
                ctx.notify_resource_updated("mem://one");
                tokio::time::sleep(Duration::from_secs(20)).await;
                Ok(CallToolResult::text("done"))
            })
            .build();
        let app = transport(tool).into_router();
        let start = Instant::now();

        let response = app
            .oneshot(final_request("tools/call", Some("work")))
            .await
            .unwrap();

        assert_eq!(start.elapsed(), DEFAULT_TIMEOUT);
        let messages = messages(&frames(response, start).await);
        assert_eq!(messages.len(), 1, "only the terminal: {messages:?}");
        assert_single_result(&messages, "done");
    })
    .await;
}

/// A tool that never answers on its own, and reports its request token.
fn cancellation_probe(cancelled: Arc<AtomicBool>) -> tower_mcp::Tool {
    ToolBuilder::new("work")
        .description("Sleeps for a long time; reports cancellation.")
        .extractor_handler((), move |ctx: Context, RawArgs(_): RawArgs| {
            let cancelled = cancelled.clone();
            async move {
                // Spawned work holding a token clone observes the disconnect
                // even after the request future is dropped.
                let token = ctx.cancellation_token();
                tokio::spawn(async move {
                    token.cancelled().await;
                    cancelled.store(true, Ordering::SeqCst);
                });
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(CallToolResult::text("done"))
            }
        })
        .build()
}

#[tokio::test(start_paused = true)]
async fn disconnect_after_the_commit_cancels_the_handler() {
    bounded(async {
        let cancelled = Arc::new(AtomicBool::new(false));
        let app = transport(cancellation_probe(cancelled.clone())).into_router();

        let response = app
            .oneshot(final_request("tools/call", Some("work")))
            .await
            .unwrap();
        assert!(content_type(&response).starts_with("text/event-stream"));

        let mut body = response.into_body();
        let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert!(frame.starts_with(b":"), "opening comment, got {frame:?}");

        // Connected and committed: not a cancellation yet.
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!cancelled.load(Ordering::SeqCst));

        drop(body);
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(cancelled.load(Ordering::SeqCst), "disconnect must cancel");
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn disconnect_before_the_deadline_cancels_the_handler() {
    bounded(async {
        let cancelled = Arc::new(AtomicBool::new(false));
        let app = transport(cancellation_probe(cancelled.clone())).into_router();

        let pending = app.oneshot(final_request("tools/call", Some("work")));
        // Abandoned before the head exists.
        let timed_out = tokio::time::timeout(Duration::from_secs(10), pending).await;
        assert!(timed_out.is_err(), "no head before the deadline");

        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(cancelled.load(Ordering::SeqCst), "disconnect must cancel");
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn completed_stream_does_not_cancel() {
    bounded(async {
        let cancelled = Arc::new(AtomicBool::new(false));
        let probe = cancelled.clone();
        let tool = ToolBuilder::new("work")
            .description("Finishes after the deadline; reports cancellation.")
            .extractor_handler((), move |ctx: Context, RawArgs(_): RawArgs| {
                let probe = probe.clone();
                async move {
                    let token = ctx.cancellation_token();
                    tokio::spawn(async move {
                        token.cancelled().await;
                        probe.store(true, Ordering::SeqCst);
                    });
                    tokio::time::sleep(Duration::from_secs(40)).await;
                    Ok(CallToolResult::text("done"))
                }
            })
            .build();
        let app = transport(tool).into_router();
        let start = Instant::now();

        let response = app
            .oneshot(final_request("tools/call", Some("work")))
            .await
            .unwrap();
        assert_single_result(&messages(&frames(response, start).await), "done");

        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(
            !cancelled.load(Ordering::SeqCst),
            "finished is not disconnected"
        );
    })
    .await;
}

/// A legacy session call past the deadline still waits and answers JSON.
#[tokio::test(start_paused = true)]
async fn legacy_session_call_has_no_initial_response_deadline() {
    bounded(async {
        let delay = Duration::from_secs(100);
        let app = transport(sleeper(delay)).into_router();

        let post = |body: serde_json::Value, session: Option<&str>, method: &str| {
            let mut builder = Request::builder()
                .method("POST")
                .uri("/")
                .header("Content-Type", "application/json")
                .header("Accept", "application/json, text/event-stream")
                .header("Mcp-Method", method);
            if let Some(session) = session {
                builder = builder
                    .header("Mcp-Session-Id", session)
                    .header("Mcp-Protocol-Version", "2025-11-25");
            }
            builder.body(Body::from(body.to_string())).unwrap()
        };

        let initialized = app
            .clone()
            .oneshot(post(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": { "name": "test", "version": "1.0" }
                    }
                }),
                None,
                "initialize",
            ))
            .await
            .unwrap();
        let session = initialized.headers()["mcp-session-id"]
            .to_str()
            .unwrap()
            .to_string();
        app.clone()
            .oneshot(post(
                serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
                Some(&session),
                "notifications/initialized",
            ))
            .await
            .unwrap();

        let start = Instant::now();
        let mut request = post(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": { "name": "work", "arguments": {} }
            }),
            Some(&session),
            "tools/call",
        );
        request
            .headers_mut()
            .insert("Mcp-Name", "work".parse().unwrap());
        let response = app.oneshot(request).await.unwrap();

        assert_eq!(start.elapsed(), delay);
        assert_eq!(content_type(&response), "application/json");
    })
    .await;
}

/// `subscriptions/listen` opens at once with its acknowledgment; the deadline
/// and its opening comment do not apply.
#[tokio::test(start_paused = true)]
async fn subscriptions_listen_is_unaffected() {
    bounded(async {
        let app = transport(sleeper(Duration::ZERO)).into_router();
        let mut request = final_request("subscriptions/listen", None);
        let body = axum::body::to_bytes(std::mem::take(request.body_mut()), usize::MAX)
            .await
            .unwrap();
        let mut json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        json["params"]["notifications"] = serde_json::json!({ "toolsListChanged": true });
        *request.body_mut() = Body::from(json.to_string());
        let start = Instant::now();

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(start.elapsed(), Duration::ZERO);
        assert!(content_type(&response).starts_with("text/event-stream"));
        let mut body = response.into_body();
        let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
        let text = String::from_utf8(frame.to_vec()).unwrap();
        assert!(
            text.contains("notifications/subscriptions/acknowledged"),
            "{text}"
        );
    })
    .await;
}
