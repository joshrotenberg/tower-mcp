//! SEP-414 from the client side: a provider registered with
//! `McpClientBuilder::trace_context_provider` puts `traceparent` and
//! `tracestate` in every request's `_meta`, and the server handler reads them
//! back with `RequestContext::trace_context`, on both protocol revisions.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tower_mcp::client::{ChannelTransport, McpClient, McpClientBuilder};
use tower_mcp::extract::{Context, RawArgs, State};
use tower_mcp::{CallToolResult, McpRouter, ToolBuilder, TraceContext};

const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
const CALLER_TRACEPARENT: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-00";

/// What the handler saw, as the text it returns.
fn describe(trace: Option<&TraceContext>) -> String {
    match trace {
        Some(trace) => format!(
            "{} tracestate={}",
            trace.traceparent(),
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
    McpRouter::new()
        .server_info("client-trace-test", "0.0.0")
        .tool(tool)
}

async fn legacy_client(builder: McpClientBuilder) -> McpClient {
    let client = builder
        .connect_simple(ChannelTransport::new(router()))
        .await
        .expect("connect");
    client
        .initialize("trace-client", "1.0.0")
        .await
        .expect("initialize");
    client
}

fn fixed_context() -> Option<TraceContext> {
    TraceContext::parse(TRACEPARENT, Some("vendor=value"))
}

#[tokio::test]
async fn provider_context_reaches_the_handler() {
    let client = legacy_client(McpClient::builder().trace_context_provider(fixed_context)).await;

    let result = client
        .call_tool("probe", serde_json::json!({}))
        .await
        .expect("call");

    assert_eq!(
        result.all_text(),
        format!("{TRACEPARENT} tracestate=vendor=value")
    );
}

#[tokio::test]
async fn provider_is_asked_for_every_request() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let client = legacy_client(McpClient::builder().trace_context_provider(move || {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        TraceContext::parse(
            &format!("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b{n}-01"),
            None,
        )
    }))
    .await;
    let before = calls.load(Ordering::SeqCst);

    let first = client
        .call_tool("probe", serde_json::json!({}))
        .await
        .unwrap();
    let second = client
        .call_tool("probe", serde_json::json!({}))
        .await
        .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), before + 2);
    assert_ne!(first.all_text(), second.all_text());
    assert!(
        first
            .all_text()
            .ends_with(&format!("b{before}-01 tracestate=-"))
    );
}

#[tokio::test]
async fn no_provider_sends_no_trace_context() {
    let client = legacy_client(McpClient::builder()).await;

    let result = client
        .call_tool("probe", serde_json::json!({}))
        .await
        .expect("call");

    assert_eq!(result.all_text(), "none");
}

#[tokio::test]
async fn provider_with_nothing_to_report_sends_no_trace_context() {
    let client = legacy_client(McpClient::builder().trace_context_provider(|| None)).await;

    let result = client
        .call_tool("probe", serde_json::json!({}))
        .await
        .expect("call");

    assert_eq!(result.all_text(), "none");
}

#[tokio::test]
async fn invalid_context_from_the_provider_is_not_sent() {
    let client = legacy_client(McpClient::builder().trace_context_provider(|| {
        // An all-zero trace id is invalid, so `parse` returns `None`.
        TraceContext::parse(
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            Some("vendor=value"),
        )
    }))
    .await;

    let result = client
        .call_tool("probe", serde_json::json!({}))
        .await
        .expect("call");

    assert_eq!(result.all_text(), "none");
}

#[tokio::test]
async fn caller_set_meta_is_not_overwritten() {
    let client = legacy_client(McpClient::builder().trace_context_provider(fixed_context)).await;

    let result: CallToolResult = client
        .request(
            "tools/call",
            &serde_json::json!({
                "name": "probe",
                "arguments": {},
                "_meta": {"traceparent": CALLER_TRACEPARENT},
            }),
        )
        .await
        .expect("call");

    assert_eq!(
        result.all_text(),
        format!("{CALLER_TRACEPARENT} tracestate=-")
    );
}

#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
mod final_protocol {
    use super::*;
    use tower_mcp::ProtocolSupport;

    async fn final_client(builder: McpClientBuilder) -> McpClient {
        let client = builder
            .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
            .connect_simple(ChannelTransport::new(router()))
            .await
            .expect("connect");
        client
            .discover("trace-client", "1.0.0")
            .await
            .expect("discover");
        client
    }

    #[tokio::test]
    async fn provider_context_reaches_the_handler() {
        let client = final_client(McpClient::builder().trace_context_provider(fixed_context)).await;

        let result = client
            .call_tool("probe", serde_json::json!({}))
            .await
            .expect("call");

        assert_eq!(
            result.all_text(),
            format!("{TRACEPARENT} tracestate=vendor=value")
        );
    }

    #[tokio::test]
    async fn no_provider_sends_no_trace_context() {
        let client = final_client(McpClient::builder()).await;

        let result = client
            .call_tool("probe", serde_json::json!({}))
            .await
            .expect("call");

        assert_eq!(result.all_text(), "none");
    }

    #[tokio::test]
    async fn caller_set_meta_is_not_overwritten() {
        let client = final_client(McpClient::builder().trace_context_provider(fixed_context)).await;

        let result: CallToolResult = client
            .request(
                "tools/call",
                &serde_json::json!({
                    "name": "probe",
                    "arguments": {},
                    "_meta": {"traceparent": CALLER_TRACEPARENT},
                }),
            )
            .await
            .expect("call");

        assert_eq!(
            result.all_text(),
            format!("{CALLER_TRACEPARENT} tracestate=-")
        );
    }
}
