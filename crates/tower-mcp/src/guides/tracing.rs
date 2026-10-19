#![doc = r####"
# Trace context and OpenTelemetry

SEP-414 reserves three keys in a request's `_meta` for distributed tracing:
`traceparent`, `tracestate`, and `baggage`. Their values follow the
[W3C Trace Context](https://www.w3.org/TR/trace-context/) and
[W3C Baggage](https://www.w3.org/TR/baggage/) formats, and the keys are an
exception to the usual vendor-prefix rule for `_meta`. A client that is inside
a trace sends them so the server's work joins the same trace.

tower-mcp handles `traceparent` and `tracestate` and has no OpenTelemetry
dependency. It does not read or send `baggage`.

| Side | What tower-mcp does |
|------|---------------------|
| Server | Reads both keys from every request, on 2025-11-25 and on 2026-07-28, and exposes a [`TraceContext`](crate::TraceContext) through [`RequestContext::trace_context`](crate::context::RequestContext::trace_context). [`McpTracingLayer`](crate::middleware::McpTracingLayer) records `trace_id` and `parent_span_id` on its span. A malformed `traceparent` is ignored and never fails the request. |
| Client | Asks a provider you register with [`McpClientBuilder::trace_context_provider`](crate::client::McpClientBuilder::trace_context_provider) for a context on every request and puts it in `params._meta`. Notifications do not carry it. |

Linking either side to OpenTelemetry takes a few lines of glue, shown below.

## The two ends without OpenTelemetry

A provider is any `Fn() -> Option<TraceContext>`. It runs on the task that
makes the call, so it can read whatever is current there. [`TraceContext::parse`](crate::TraceContext::parse)
is the only constructor, so a provider cannot hand over a malformed value: one
it cannot parse comes back as `None`, and the request goes out with no trace
keys. A request whose `_meta` already has `traceparent` or `tracestate` is left
as the caller wrote it.

```rust
use tower_mcp::client::McpClient;
use tower_mcp::TraceContext;

let builder = McpClient::builder().trace_context_provider(|| {
    TraceContext::parse(
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        Some("vendor=value"),
    )
});
# let _ = builder;
```

On the server, a handler reads the context from its
[`RequestContext`](crate::context::RequestContext). [`TraceContext::entries`](crate::TraceContext::entries)
yields the `(key, value)` pairs, which are also what an OpenTelemetry
propagator reads from a carrier.

```rust
use tower_mcp::{CallToolResult, ToolBuilder};
use tower_mcp::extract::{Context, RawArgs, State};

let tool = ToolBuilder::new("whoami")
    .description("Report the caller's trace id")
    .extractor_handler(
        (),
        |State(()): State<()>, ctx: Context, RawArgs(_): RawArgs| async move {
            let trace_id = ctx
                .trace_context()
                .map(|trace| trace.trace_id().to_string())
                .unwrap_or_else(|| "none".to_string());
            Ok(CallToolResult::text(trace_id))
        },
    )
    .build();
```

## Linking to OpenTelemetry

The examples in this section were written against these crate versions and
checked by building them, along with a client and a server that exchanged a
trace over `ChannelTransport`, against tower-mcp 0.24:

```toml
opentelemetry = "0.33"
opentelemetry_sdk = "0.33"
tracing-opentelemetry = "0.34"
```

They are marked `ignore` because tower-mcp does not depend on these crates.
Install a `tracing_opentelemetry::layer()` with a tracer in your `tracing`
subscriber as usual; without it the client below finds no context to send.

### Client: send the current span

The provider reads the OpenTelemetry context of the current `tracing` span,
writes it into a carrier with the W3C propagator, and hands the two strings to
`TraceContext::parse`. When the current span has no valid OpenTelemetry
context the carrier stays empty and the provider returns `None`.

```rust,ignore
use std::collections::HashMap;

use opentelemetry::propagation::TextMapPropagator;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tower_mcp::TraceContext;
use tracing_opentelemetry::OpenTelemetrySpanExt;

fn current_trace_context() -> Option<TraceContext> {
    let cx = tracing::Span::current().context();
    let mut carrier = HashMap::new();
    TraceContextPropagator::new().inject_context(&cx, &mut carrier);
    TraceContext::parse(
        carrier.get("traceparent")?,
        carrier.get("tracestate").map(String::as_str),
    )
}

let client = McpClient::builder()
    .trace_context_provider(current_trace_context)
    .connect_simple(transport)
    .await?;
```

Call the client from inside the span you want as the parent, for example with
`.instrument(span)` or `#[tracing::instrument]`. The provider reads
`Span::current()` at the moment each request is built, so a call made outside
any span sends nothing.

### Server: parent the request span

A tower layer reads the `TraceContext` from the request extensions, turns it
back into an OpenTelemetry context with the propagator, and makes it the
parent of a span that wraps the inner services.

```rust,ignore
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use opentelemetry::propagation::TextMapPropagator;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tower::Layer;
use tower_mcp::router::{RouterRequest, RouterResponse};
use tower_mcp::TraceContext;
use tower_service::Service;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

#[derive(Clone)]
struct OtelParentLayer;

impl<S> Layer<S> for OtelParentLayer {
    type Service = OtelParent<S>;

    fn layer(&self, inner: S) -> OtelParent<S> {
        OtelParent { inner }
    }
}

#[derive(Clone)]
struct OtelParent<S> {
    inner: S,
}

impl<S> Service<RouterRequest> for OtelParent<S>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = Infallible>,
    S::Future: Send + 'static,
{
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let span = tracing::info_span!("mcp_server_request");
        if let Some(trace) = req.extensions.get::<TraceContext>() {
            let carrier: HashMap<String, String> = trace
                .entries()
                .map(|(key, value)| (key.to_string(), value))
                .collect();
            let parent = TraceContextPropagator::new().extract(&carrier);
            // Must run before the span is first entered.
            let _ = span.set_parent(parent);
        }
        // Spans the inner services create in `call` become children of `span`.
        let future = {
            let _entered = span.enter();
            self.inner.call(req)
        };
        Box::pin(future.instrument(span))
    }
}
```

Install it outside `McpTracingLayer`, so the `mcp_request` span that layer
creates is a child of the linked span:

```rust,ignore
let transport = StdioTransport::new(router)
    .layer(tower::layer::layer_fn(|inner| {
        OtelParentLayer.layer(McpTracingLayer::new().layer(inner))
    }));
```

Two details are easy to get wrong:

- `set_parent` only works before a span is first entered. By the time a layer
  inside `McpTracingLayer` runs, that layer's `mcp_request` span has been
  entered, `set_parent` returns `Err(AlreadyStarted)`, and the span keeps its
  own trace. That is why the layer above owns a span of its own.
- The layer creates the span in `call` and enters it only while calling the
  inner service. Spans that inner layers create synchronously in their own
  `call`, as `McpTracingLayer` does, then pick it up as their parent.

The first request of a session, `initialize`, usually has no trace context and
starts a trace of its own.

## Related

- [`TraceContext`](crate::TraceContext) documents the parsing rules.
- [`McpTracingLayer`](crate::middleware::McpTracingLayer) documents the span
  fields.
- [Protocol versions](crate::guides::protocol_versions) covers the 2026-07-28
  `_meta` keys the client adds beside the trace keys.
"####]
