//! SEP-414: `traceparent` / `tracestate` in request `_meta` reach handlers as a
//! `TraceContext` on `RequestContext`, on both protocol revisions and for
//! tools, resources, and prompts. A missing or malformed value yields `None`
//! and never fails the request.

use serde_json::{Value, json};
use tower_mcp::extract::{Context, RawArgs, State};
use tower_mcp::protocol::McpNotification;
use tower_mcp::{
    CallToolResult, GetPromptResult, JsonRpcRequest, JsonRpcResponse, JsonRpcService, McpRouter,
    PromptBuilder, ReadResourceResult, ResourceBuilder, ToolBuilder, TraceContext,
};

const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

/// What a handler saw, as the text it returns.
fn describe(trace: Option<&TraceContext>) -> String {
    match trace {
        Some(trace) => format!(
            "trace_id={} parent_id={} sampled={} tracestate={}",
            trace.trace_id(),
            trace.parent_id(),
            trace.sampled(),
            trace.tracestate().unwrap_or("-"),
        ),
        None => "none".to_string(),
    }
}

fn router() -> McpRouter {
    let tool = ToolBuilder::new("probe")
        .description("Reports the trace context")
        .read_only()
        .extractor_handler(
            (),
            |State(()): State<()>, ctx: Context, RawArgs(_args): RawArgs| async move {
                Ok(CallToolResult::text(describe(ctx.trace_context())))
            },
        )
        .build();
    let resource = ResourceBuilder::new("probe://trace")
        .name("probe")
        .description("Reports the trace context")
        .handler_with_context(|ctx| async move {
            Ok(ReadResourceResult::text(
                "probe://trace",
                describe(ctx.trace_context()),
            ))
        })
        .build();
    let prompt = PromptBuilder::new("probe")
        .description("Reports the trace context")
        .handler_with_context(|ctx, _args| async move {
            Ok(GetPromptResult::user_message(describe(ctx.trace_context())))
        })
        .build();
    McpRouter::new()
        .server_info("trace-context-test", "0.0.0")
        .tool(tool)
        .resource(resource)
        .prompt(prompt)
}

/// A legacy (2025-11-25) service: initialized, with no per-request protocol
/// metadata, so trace context can only arrive through `params._meta`.
async fn legacy_service() -> JsonRpcService<McpRouter> {
    let router = router();
    let mut service = JsonRpcService::new(router.clone());
    let init = JsonRpcRequest::new(1, "initialize").with_params(json!({
        "protocolVersion": "2025-11-25",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    let response = service.call_single(init).await.unwrap();
    assert!(matches!(response, JsonRpcResponse::Result(_)));
    router.handle_notification(McpNotification::Initialized);
    service
}

/// Add the 2026-07-28 per-request keys to a `_meta` object.
#[cfg(feature = "stateless")]
fn with_modern_keys(mut meta: Value) -> Value {
    let object = meta.as_object_mut().unwrap();
    object.insert(
        "io.modelcontextprotocol/protocolVersion".into(),
        json!(tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28),
    );
    object.insert(
        "io.modelcontextprotocol/clientCapabilities".into(),
        json!({}),
    );
    meta
}

/// Send `method` with `params` plus an optional `_meta`, return the result.
async fn send(
    service: &mut JsonRpcService<McpRouter>,
    method: &str,
    mut params: Value,
    meta: Option<Value>,
) -> Value {
    if let Some(meta) = meta {
        params["_meta"] = meta;
    }
    let request = JsonRpcRequest::new(7, method).with_params(params);
    match service.call_single(request).await.unwrap() {
        JsonRpcResponse::Result(result) => result.result,
        other => panic!("expected a result, got {other:?}"),
    }
}

fn tool_text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

fn resource_text(result: &Value) -> &str {
    result["contents"][0]["text"].as_str().unwrap()
}

fn prompt_text(result: &Value) -> &str {
    result["messages"][0]["content"]["text"].as_str().unwrap()
}

const EXPECTED: &str = "trace_id=4bf92f3577b34da6a3ce929d0e0e4736 parent_id=00f067aa0ba902b7 \
                        sampled=true tracestate=vendor=value";

fn trace_meta() -> Value {
    json!({ "traceparent": TRACEPARENT, "tracestate": "vendor=value" })
}

#[tokio::test]
async fn legacy_tool_sees_trace_context() {
    let mut service = legacy_service().await;
    let result = send(
        &mut service,
        "tools/call",
        json!({ "name": "probe", "arguments": {} }),
        Some(trace_meta()),
    )
    .await;
    assert_eq!(tool_text(&result), EXPECTED);
}

#[tokio::test]
async fn legacy_resource_sees_trace_context() {
    let mut service = legacy_service().await;
    let result = send(
        &mut service,
        "resources/read",
        json!({ "uri": "probe://trace" }),
        Some(trace_meta()),
    )
    .await;
    assert_eq!(resource_text(&result), EXPECTED);
}

#[tokio::test]
async fn legacy_prompt_sees_trace_context() {
    let mut service = legacy_service().await;
    let result = send(
        &mut service,
        "prompts/get",
        json!({ "name": "probe", "arguments": {} }),
        Some(trace_meta()),
    )
    .await;
    assert_eq!(prompt_text(&result), EXPECTED);
}

#[tokio::test]
async fn legacy_traceparent_without_tracestate() {
    let mut service = legacy_service().await;
    let result = send(
        &mut service,
        "tools/call",
        json!({ "name": "probe", "arguments": {} }),
        Some(json!({ "traceparent": TRACEPARENT })),
    )
    .await;
    assert_eq!(
        tool_text(&result),
        "trace_id=4bf92f3577b34da6a3ce929d0e0e4736 parent_id=00f067aa0ba902b7 \
         sampled=true tracestate=-"
    );
}

#[tokio::test]
async fn absent_trace_context_is_none() {
    let mut service = legacy_service().await;
    for meta in [
        None,
        Some(json!({})),
        Some(json!({ "tracestate": "vendor=value" })),
    ] {
        let result = send(
            &mut service,
            "tools/call",
            json!({ "name": "probe", "arguments": {} }),
            meta,
        )
        .await;
        assert_eq!(tool_text(&result), "none");
    }
}

#[tokio::test]
async fn malformed_trace_context_is_none_and_the_request_succeeds() {
    let mut service = legacy_service().await;
    for traceparent in [
        json!("garbage"),
        json!("00-00000000000000000000000000000000-00f067aa0ba902b7-01"),
        json!("00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01"),
        json!("ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        json!(42),
        Value::Null,
    ] {
        let meta = json!({ "traceparent": traceparent, "tracestate": "vendor=value" });
        let result = send(
            &mut service,
            "tools/call",
            json!({ "name": "probe", "arguments": {} }),
            Some(meta.clone()),
        )
        .await;
        assert_eq!(tool_text(&result), "none", "{meta}");

        let result = send(
            &mut service,
            "resources/read",
            json!({ "uri": "probe://trace" }),
            Some(meta),
        )
        .await;
        assert_eq!(resource_text(&result), "none");
    }
}

#[tokio::test]
async fn invalid_tracestate_is_dropped_and_traceparent_kept() {
    let mut service = legacy_service().await;
    let meta = json!({
        "traceparent": TRACEPARENT,
        "tracestate": format!("k={}", "v".repeat(600)),
    });
    let result = send(
        &mut service,
        "tools/call",
        json!({ "name": "probe", "arguments": {} }),
        Some(meta),
    )
    .await;
    assert_eq!(
        tool_text(&result),
        "trace_id=4bf92f3577b34da6a3ce929d0e0e4736 parent_id=00f067aa0ba902b7 \
         sampled=true tracestate=-"
    );
}

#[cfg(feature = "stateless")]
mod modern {
    use super::*;

    fn modern_service() -> JsonRpcService<McpRouter> {
        JsonRpcService::new(router())
    }

    #[tokio::test]
    async fn modern_tool_sees_trace_context() {
        let result = send(
            &mut modern_service(),
            "tools/call",
            json!({ "name": "probe", "arguments": {} }),
            Some(with_modern_keys(trace_meta())),
        )
        .await;
        assert_eq!(tool_text(&result), EXPECTED);
    }

    #[tokio::test]
    async fn modern_resource_sees_trace_context() {
        let result = send(
            &mut modern_service(),
            "resources/read",
            json!({ "uri": "probe://trace" }),
            Some(with_modern_keys(trace_meta())),
        )
        .await;
        assert_eq!(resource_text(&result), EXPECTED);
    }

    #[tokio::test]
    async fn modern_prompt_sees_trace_context() {
        let result = send(
            &mut modern_service(),
            "prompts/get",
            json!({ "name": "probe", "arguments": {} }),
            Some(with_modern_keys(trace_meta())),
        )
        .await;
        assert_eq!(prompt_text(&result), EXPECTED);
    }

    #[tokio::test]
    async fn modern_absent_or_malformed_trace_context_is_none() {
        for meta in [
            json!({}),
            json!({ "traceparent": "garbage", "tracestate": "vendor=value" }),
            json!({ "traceparent": 1 }),
        ] {
            let result = send(
                &mut modern_service(),
                "tools/call",
                json!({ "name": "probe", "arguments": {} }),
                Some(with_modern_keys(meta.clone())),
            )
            .await;
            assert_eq!(tool_text(&result), "none", "{meta}");
        }
    }
}

/// End to end over HTTP on both revisions: the context survives the
/// transport's own extension plumbing.
#[cfg(feature = "http")]
mod http {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    use tower_mcp::HttpTransport;

    use super::*;

    async fn post(
        app: &axum::Router,
        body: Value,
        headers: &[(&str, &str)],
    ) -> (axum::http::HeaderMap, Value) {
        let mut request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", response.status());
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        if text.is_empty() {
            // A 202 for a notification.
            return (headers, Value::Null);
        }
        // The response is JSON, or a single SSE `data:` frame.
        let json = match serde_json::from_str(&text) {
            Ok(json) => json,
            Err(_) => {
                let data = text
                    .lines()
                    .find_map(|line| line.strip_prefix("data:"))
                    .expect("an SSE data frame");
                serde_json::from_str(data.trim()).unwrap()
            }
        };
        (headers, json)
    }

    #[tokio::test]
    async fn http_session_path_delivers_trace_context() {
        let app = HttpTransport::new(router())
            .disable_origin_validation()
            .into_router();
        let (headers, _) = post(
            &app,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1.0" }
                }
            }),
            &[],
        )
        .await;
        let session = headers
            .get("mcp-session-id")
            .expect("session id")
            .to_str()
            .unwrap()
            .to_string();
        let common = [
            ("Mcp-Session-Id", session.as_str()),
            ("Mcp-Protocol-Version", "2025-11-25"),
        ];
        post(
            &app,
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            &common,
        )
        .await;
        let (_, body) = post(
            &app,
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {
                    "name": "probe", "arguments": {}, "_meta": trace_meta()
                }
            }),
            &common,
        )
        .await;
        assert_eq!(tool_text(&body["result"]), EXPECTED);
    }

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn http_stateless_path_delivers_trace_context() {
        let app = HttpTransport::new(router())
            .disable_origin_validation()
            .into_router();
        let (_, body) = post(
            &app,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {
                    "name": "probe", "arguments": {},
                    "_meta": with_modern_keys(trace_meta())
                }
            }),
            &[
                ("Mcp-Protocol-Version", "2026-07-28"),
                ("Mcp-Method", "tools/call"),
                ("Mcp-Name", "probe"),
            ],
        )
        .await;
        assert_eq!(tool_text(&body["result"]), EXPECTED);
    }
}
