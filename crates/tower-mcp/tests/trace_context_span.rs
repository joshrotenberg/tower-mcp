//! `McpTracingLayer` records the SEP-414 trace and parent span ids on its
//! `mcp_request` span, and a malformed `traceparent` emits a debug event.
//!
//! Its own test binary, and a single test, for the reason given in
//! `stdio_frame_tracing.rs`: what a `tracing` subscriber captures depends on
//! process-wide callsite interest, so nothing else may emit events here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tower::Layer;
use tower_mcp::middleware::McpTracingLayer;
use tower_mcp::{JsonRpcRequest, JsonRpcResponse, JsonRpcService, McpRouter, McpTracingService};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};

#[derive(Default)]
struct Fields(HashMap<String, String>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(
            field.name().to_string(),
            format!("{value:?}").replace('"', ""),
        );
    }
}

#[derive(Default, Clone)]
struct Capture {
    /// Fields of every `mcp_request` span, in creation order.
    spans: Arc<Mutex<Vec<HashMap<String, String>>>>,
    /// Messages of every event at DEBUG level.
    debug_events: Arc<Mutex<Vec<String>>>,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: Context<'_, S>,
    ) {
        if attrs.metadata().name() == "mcp_request" {
            let mut fields = Fields::default();
            attrs.record(&mut fields);
            self.spans.lock().unwrap().push(fields.0);
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if *event.metadata().level() == tracing::Level::DEBUG {
            let mut fields = Fields::default();
            event.record(&mut fields);
            if let Some(message) = fields.0.remove("message") {
                self.debug_events.lock().unwrap().push(message);
            }
        }
    }
}

async fn list_tools(
    service: &mut JsonRpcService<McpTracingService<McpRouter>>,
    meta: serde_json::Value,
) {
    let request = JsonRpcRequest::new(1, "tools/list").with_params(json!({ "_meta": meta }));
    let response = service.call_single(request).await.unwrap();
    assert!(
        matches!(response, JsonRpcResponse::Result(_)),
        "request must succeed regardless of trace context: {response:?}"
    );
}

#[tokio::test]
async fn span_records_trace_and_parent_ids() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let router = McpRouter::new().server_info("trace-span-test", "0.0.0");
    router.session().mark_preinitialized();
    let mut service = JsonRpcService::new(McpTracingLayer::new().layer(router));

    // A valid traceparent is recorded.
    list_tools(
        &mut service,
        json!({
            "traceparent": "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "tracestate": "vendor=value",
        }),
    )
    .await;
    // No trace context leaves the fields empty.
    list_tools(&mut service, json!({})).await;
    // A malformed traceparent leaves them empty too.
    list_tools(&mut service, json!({ "traceparent": "garbage" })).await;

    let spans = capture.spans.lock().unwrap();
    assert_eq!(spans.len(), 3, "{spans:?}");

    assert_eq!(spans[0]["method"], "tools/list");
    assert_eq!(spans[0]["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(spans[0]["parent_span_id"], "00f067aa0ba902b7");

    for span in &spans[1..] {
        assert!(!span.contains_key("trace_id"), "{span:?}");
        assert!(!span.contains_key("parent_span_id"), "{span:?}");
    }

    let events = capture.debug_events.lock().unwrap();
    let malformed = events
        .iter()
        .filter(|message| message.contains("malformed traceparent"))
        .count();
    assert_eq!(malformed, 1, "{events:?}");
}
