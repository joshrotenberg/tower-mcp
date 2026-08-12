//! The stateless SSE fallback: what happens when a 2026-07-28 handler emits a
//! notification.
//!
//! A stateless request has no session stream, so a handler that emits a
//! notification cannot be answered with a plain JSON body. The response becomes
//! `text/event-stream` instead: the notifications first, then the terminal
//! response as the last event, then the stream ends.
//!
//! Not every notification takes that path. The subscription-scoped ones
//! (`resources/updated`, the `list_changed` family, task status) belong
//! exclusively on `subscriptions/listen` streams and are routed there whether
//! or not anyone is listening, so they never inline into a response. Only the
//! request-scoped kinds, logging and progress, open the fallback.
//!
//! That distinction and the ordering around it are the whole contract, and both
//! were untested (#1367).

#![cfg(all(feature = "http", feature = "stateless"))]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tower_mcp::extract::{Context, RawArgs};
use tower_mcp::protocol::{LogLevel, LoggingMessageParams};
use tower_mcp::{CallToolResult, Error, HttpTransport, McpRouter, ToolBuilder};

fn log(ctx: &Context, message: &str) {
    ctx.send_log(LoggingMessageParams {
        level: LogLevel::Info,
        logger: Some("fallback-test".into()),
        data: serde_json::json!({ "message": message }),
        meta: None,
    });
}

fn app() -> axum::Router {
    // One log line, then success.
    let once = ToolBuilder::new("log_once")
        .description("Emits a single log notification, then answers.")
        .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
            log(&ctx, "only");
            Ok(CallToolResult::text("done"))
        })
        .build();

    // Several, to pin the ordering rather than just the presence of one.
    let thrice = ToolBuilder::new("log_thrice")
        .description("Emits three log notifications, then answers.")
        .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
            for message in ["first", "second", "third"] {
                log(&ctx, message);
            }
            Ok(CallToolResult::text("done"))
        })
        .build();

    // Emits, then fails. The stream still has to terminate.
    let failing = ToolBuilder::new("log_then_fail")
        .description("Emits a log notification, then returns an error.")
        .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
            log(&ctx, "before the fall");
            Err(Error::tool("handler said no"))
        })
        .build();

    // Subscription-scoped: belongs on a listen stream, so it must not inline.
    let subscription_scoped = ToolBuilder::new("touch_resource")
        .description("Emits a resource-updated notification, then answers.")
        .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
            ctx.notify_resource_updated("mem://one");
            Ok(CallToolResult::text("done"))
        })
        .build();

    // Emits nothing at all.
    let quiet = ToolBuilder::new("quiet")
        .description("Answers without emitting anything.")
        .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
            Ok(CallToolResult::text("done"))
        })
        .build();

    HttpTransport::new(
        McpRouter::new()
            .server_info("sse-fallback-test", "1.0.0")
            .tool(once)
            .tool(thrice)
            .tool(failing)
            .tool(subscription_scoped)
            .tool(quiet),
    )
    .disable_origin_validation()
    .disable_host_validation()
    .into_router()
}

/// A stateless (2026-07-28) `tools/call`.
///
/// `logLevel` is required for log delivery: the final protocol dropped
/// `logging/setLevel` and authorizes logs per request instead, so a handler's
/// `send_log` is dropped without it.
fn call(tool: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "tools/call")
        // SEP-2243 requires the target name in a header, and the request is
        // rejected before dispatch without it.
        .header("Mcp-Name", tool)
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {},
                        "io.modelcontextprotocol/logLevel": "info"
                    },
                    "name": tool,
                    "arguments": {}
                }
            })
            .to_string(),
        ))
        .unwrap()
}

async fn body_of(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("the stream has to terminate on its own");
    String::from_utf8(bytes.to_vec()).expect("SSE frames are UTF-8")
}

/// The `data:` payloads of an SSE body, in order.
fn events(body: &str) -> Vec<serde_json::Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|json| {
            serde_json::from_str(json.trim())
                .unwrap_or_else(|e| panic!("event payload is not JSON ({e}): {json}"))
        })
        .collect()
}

fn content_type(response: &axum::response::Response) -> String {
    response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// The log messages carried by an event list, in order.
fn messages(events: &[serde_json::Value]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e["method"] == "notifications/message")
        .map(|e| e["params"]["data"]["message"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_handler_notification_turns_the_response_into_an_event_stream() {
    let response = app().oneshot(call("log_once")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        content_type(&response).starts_with("text/event-stream"),
        "a notification has nowhere to go on a JSON body, so the response has \
         to become a stream: {}",
        content_type(&response)
    );

    let events = events(&body_of(response).await);
    assert_eq!(
        events.len(),
        2,
        "one notification and the response: {events:?}"
    );
    assert_eq!(events[0]["method"], "notifications/message");
    assert_eq!(messages(&events), ["only"]);
}

/// The point of the ordering: a client that reads the response and stops has
/// not missed anything.
#[tokio::test]
async fn the_terminal_response_is_the_last_event() {
    let events = events(&body_of(app().oneshot(call("log_thrice")).await.unwrap()).await);

    let (last, rest) = events.split_last().expect("at least the response");
    assert_eq!(last["id"], 1, "the last event is the response: {last:?}");
    assert!(last.get("result").is_some(), "and it succeeded: {last:?}");
    for event in rest {
        assert!(
            event.get("id").is_none(),
            "everything before it is a notification: {event:?}"
        );
    }
}

#[tokio::test]
async fn notifications_keep_their_emission_order() {
    let events = events(&body_of(app().oneshot(call("log_thrice")).await.unwrap()).await);
    assert_eq!(messages(&events), ["first", "second", "third"]);
}

/// A handler error is still a terminal event. Without one the stream would end
/// with no response at all, and a client would wait forever for an id it was
/// never going to be told about.
#[tokio::test]
async fn a_failing_handler_still_terminates_the_stream() {
    let response = app().oneshot(call("log_then_fail")).await.unwrap();
    assert!(content_type(&response).starts_with("text/event-stream"));

    let events = events(&body_of(response).await);
    assert_eq!(messages(&events), ["before the fall"]);

    let last = events.last().expect("a terminal event");
    assert!(
        last.get("error").is_some() || last["result"]["isError"] == serde_json::Value::Bool(true),
        "the stream ends by reporting the failure: {last:?}"
    );
}

/// `resources/updated` and the rest of the subscription-scoped family are
/// routed to `subscriptions/listen` streams and to nowhere else. Inlining one
/// here would deliver it twice to a client that is also listening.
#[tokio::test]
async fn a_subscription_scoped_notification_does_not_open_a_stream() {
    let response = app().oneshot(call("touch_resource")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        content_type(&response).starts_with("application/json"),
        "it belongs on a listen stream, not in this response: {}",
        content_type(&response)
    );

    let body: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
    assert!(body.get("result").is_some(), "{body:?}");
}

/// The fallback is a fallback. A handler that emits nothing is answered with a
/// plain JSON body, which is the cheaper path and the common one.
#[tokio::test]
async fn a_quiet_handler_stays_on_the_json_path() {
    let response = app().oneshot(call("quiet")).await.unwrap();
    // The status matters as much as the content type here: a rejected request
    // is also `application/json`, so without it this passes for the wrong
    // reason, which is exactly what it did while the request was missing a
    // required header.
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        content_type(&response).starts_with("application/json"),
        "nothing was emitted, so there is no reason to open a stream: {}",
        content_type(&response)
    );

    let body: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
    assert!(body.get("result").is_some(), "{body:?}");
}
