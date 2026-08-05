#![doc = r####"
# HTTP deployment guide

This guide covers mounting and operating tower-mcp's Streamable HTTP server.
It focuses on crate behavior and deployment choices; the authoritative wire
requirements live in the MCP specifications:

- [2025-11-25 Streamable HTTP](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)
- [2026-07-28 specification](https://modelcontextprotocol.io/specification/2026-07-28)
- [2026-07-28 key changes](https://modelcontextprotocol.io/specification/2026-07-28/changelog)

Read the [OAuth authorization guide](crate::guides::oauth) before exposing an MCP endpoint
outside a trusted network.

## Install the server transport

```toml
[dependencies]
tower-mcp = { version = "0.19", features = ["http"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal"] }
axum = "0.8"
```

Add `protocol-2026-07-28` when this deployment should accept the released
sessionless protocol. Compile-time availability and runtime selection are
separate; see the [protocol-version guide](crate::guides::protocol_versions).

## Standalone or mounted

`serve` is the shortest standalone path. It mounts the MCP endpoint at `/` and
the health check at `/health`:

```rust,no_run
use tower_mcp::{BoxError, HttpTransport, McpRouter};

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let mcp = McpRouter::new().server_info("my-server", "1.0.0");
    HttpTransport::new(mcp).serve("127.0.0.1:3000").await?;
    Ok(())
}
```

Bind to loopback for a local server. Binding to `0.0.0.0` makes the service
reachable from other hosts and should be paired with TLS termination,
authentication, and explicit origin/host policy.

For an existing axum application, mount the complete transport router at a
stable path:

```rust,no_run
use axum::{Router, routing::get};
use tower_mcp::{BoxError, HttpTransport, McpRouter};

async fn ready() -> &'static str { "ready" }

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let mcp_service = McpRouter::new().server_info("my-server", "1.0.0");
    let mcp = HttpTransport::new(mcp_service).into_router_at("/mcp");

    let app = Router::new()
        .route("/readyz", get(ready))
        .merge(mcp);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    axum::serve(listener, app).await?;
    Ok(())
}
```

The MCP endpoint is now `/mcp` and its built-in health route is
`/mcp/health`. Use `into_router_at_with_handle` when an admin endpoint needs a
`SessionHandle` for session count, inspection, or termination. The
[`axum_embedding` example](https://github.com/joshrotenberg/tower-mcp/blob/main/examples/axum_embedding.rs) is the canonical
shared-router pattern.

When OAuth is enabled, prefer `into_oauth_router_at` rather than nesting an
already-built OAuth router manually. It derives the protected-resource
metadata path and resource identifier from the mount path correctly.

## Origin and host validation

`HttpTransport` validates browser `Origin` headers by default:

- a request without `Origin` is accepted;
- localhost origins are accepted;
- a non-localhost origin is rejected unless it exactly matches
  `allowed_origins`;
- an empty allowlist therefore rejects browser cross-origin access.

Host validation is also enabled. Localhost variants are accepted. For
compatibility, non-localhost hosts are accepted when `allowed_hosts` is empty;
set an explicit allowlist in production to turn it into a strict check.

```rust,no_run
use tower_mcp::{HttpTransport, McpRouter};

let transport = HttpTransport::new(McpRouter::new())
    .allowed_origins(vec!["https://app.example.com".to_string()])
    .allowed_hosts(vec!["mcp.example.com".to_string()]);
```

Entries in `allowed_hosts` may include a port. Configure the reverse proxy to
preserve the public `Host` or authority value that appears in the allowlist.
Avoid `"*"` origins for authenticated endpoints. Disabling either validation
is intended for controlled tests and unusual trusted-network topologies, not
as a production fix for a proxy misconfiguration.

Origin validation is a security check, not a CORS response policy. Browser
clients may also need an axum/tower-http CORS layer on the returned router to
emit `Access-Control-Allow-*` headers. Allow the MCP methods and headers the
client actually uses; final-protocol HTTP includes `MCP-Protocol-Version`,
`Mcp-Method`, `Mcp-Name`, and possibly `Mcp-Param-*` headers.

## Stateful legacy and sessionless final traffic

One `HttpTransport` can serve both protocol eras on the same URL when
`protocol-2026-07-28` is compiled and enabled:

| Client traffic | tower-mcp path | Operational state |
|---|---|---|
| `initialize`, then `MCP-Session-Id` | legacy `2025-11-25` / `2025-03-26` | session state, GET/POST SSE, optional event replay |
| `MCP-Protocol-Version: 2026-07-28`, required request metadata, no session ID | final `2026-07-28` | independent requests, `server/discover`, explicit `subscriptions/listen` streams |

Legacy sessions are optional by default for compatibility with clients that
do not retain the session header. Use `require_sessions()` when the application
depends on strict 2025-11-25 session semantics:

```rust,no_run
use std::time::Duration;
use tower_mcp::{HttpTransport, McpRouter};

let transport = HttpTransport::new(McpRouter::new())
    .require_sessions()
    .session_ttl(Duration::from_secs(30 * 60))
    .max_sessions(10_000);
```

The default session TTL is 30 minutes and the default cleanup interval is one
minute. There is no default maximum; set one to bound per-process session
memory. The built-in session and event stores are in-memory.

The method named `stateless(StatelessConfig)` controls a historical SEP-1442
compatibility path. It does **not** enable the released 2026-07-28 lifecycle.
For new deployments, enable `protocol-2026-07-28` and select versions with
`ProtocolSupport` instead.

## Resumption and disconnection

Legacy Streamable HTTP can attach event IDs to SSE messages. A reconnecting
client sends `Last-Event-ID`, and the configured `EventStore` supplies later
events. The default in-memory store can replay only while the same process and
buffer survive. Supply an external implementation for restart or cross-node
replay.

The 2026-07-28 protocol removed SSE event IDs, `Last-Event-ID`, and replay. A
broken final-protocol response loses that in-flight request; a client may
issue a new request with a new request ID only when application semantics make
that safe. A broken `subscriptions/listen` stream is opened again as a new
subscription.

Do not interpret an SSE disconnect as proof that a tool was cancelled or did
not run. Cancellation and idempotency are application concerns at this
boundary.

## Horizontal scaling

Choose a topology from the protocol behavior, not just from where metadata is
stored:

| Topology | Works well for | Limitation |
|---|---|---|
| One instance, in-memory stores | local and small deployments | sessions and replay buffers disappear on restart |
| Sticky routing by legacy session | legacy sessions and bidirectional POST streams | rebalancing or instance loss breaks live process-local state |
| Shared `SessionStore` + `EventStore` | restoring legacy metadata and replaying buffered events across nodes | does not migrate pending requests, service futures, notification channels, or an open SSE response |
| Sessionless final requests | ordinary 2026-07-28 calls across any healthy node | an active `subscriptions/listen` connection remains owned by one node and has no replay |

For legacy traffic, session affinity remains the safe default. A Layer 7 load
balancer can hash `MCP-Session-Id` after initialization. If you use shared
stores, keep associated POST streams and their client responses on the node
that owns the originating request; roots, sampling, and elicitation channels
are process-local.

`SessionStore` persists identity, capabilities, timestamps, and negotiated
version. `EventStore` persists resumable legacy notification events. Configure
both when failover needs both behaviors:

```rust,no_run
use std::sync::Arc;
use tower_mcp::{HttpTransport, McpRouter};
use tower_mcp::event_store::{EventStore, MemoryEventStore};
use tower_mcp::session_store::{MemorySessionStore, SessionStore};

let sessions: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
let events: Arc<dyn EventStore> = Arc::new(MemoryEventStore::new());

let transport = HttpTransport::new(McpRouter::new())
    .session_store(sessions)
    .event_store(events);
```

The snippet shows the trait wiring; replace both memory implementations with
an external backend for a real multi-instance deployment. See
[`horizontal_scaling.rs`](https://github.com/joshrotenberg/tower-mcp/blob/main/examples/horizontal_scaling.rs),
[`session_store.rs`](https://github.com/joshrotenberg/tower-mcp/blob/main/examples/session_store.rs), and
[`event_store.rs`](https://github.com/joshrotenberg/tower-mcp/blob/main/examples/event_store.rs).

## Reverse proxies and SSE

The proxy must stream response bytes promptly and keep long-lived responses
open. An nginx location for a path-mounted endpoint looks like:

```nginx
location /mcp {
    proxy_pass http://mcp_backend;
    proxy_http_version 1.1;

    proxy_buffering off;
    proxy_cache off;
    proxy_set_header Connection "";
    chunked_transfer_encoding on;

    proxy_read_timeout 3600s;
    proxy_send_timeout 3600s;

    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
}
```

Also verify these behaviors in the chosen ingress or CDN:

- response buffering and compression do not batch SSE events;
- idle timeouts exceed the intended stream lifetime or keepalives arrive
  frequently enough;
- request body and header limits permit MCP payloads and `Mcp-Param-*` values;
- `MCP-Protocol-Version`, `MCP-Session-Id`, `Last-Event-ID`, `Mcp-Method`,
  `Mcp-Name`, `Authorization`, and `WWW-Authenticate` pass unchanged;
- 202 responses and streaming content types are not rewritten;
- graceful deployment drain gives in-flight POST responses time to finish.

Test through the public proxy URL. A direct-to-pod test will not detect
buffering, header stripping, public-host allowlist mistakes, or TLS/resource
identifier mismatches.

## Timeouts and size limits

The transport rejects POST bodies larger than 4 MiB by default. Change that
with `max_body_size`; axum's `DefaultBodyLimit` does not control the MCP
endpoint because tower-mcp reads the raw request body itself.

Place timeouts according to what should be cancelled:

1. Put a handler-specific timeout on `ToolBuilder`, `ResourceBuilder`, or
   `PromptBuilder` when only that operation needs a deadline.
2. Use `HttpTransport::layer` for an MCP-wide request-processing policy.
3. Use an outer axum layer for HTTP concerns such as header parsing or a
   short admission timeout.
4. Do not put a short generic response timeout around the entire axum router;
   it will also terminate intended SSE and `subscriptions/listen` streams.

Always maintain an absolute operation deadline. Progress notifications are
evidence of activity, but should not permit an unbounded request. Configure
proxy and load-balancer idle timeouts separately from application deadlines.

## Middleware order and scope

tower-mcp has three relevant middleware boundaries:

| Boundary | API | Sees | Good uses |
|---|---|---|---|
| HTTP router | `transport.into_router[_at]().layer(...)` | MCP HTTP, health, and metadata routes | CORS response policy, request IDs, HTTP tracing, forwarded-header policy |
| MCP service | `HttpTransport::layer(...)` | parsed MCP requests | principal extensions, MCP metrics, global concurrency or timeout policy |
| One handler | `.layer(...)` on a tool/resource/prompt builder | only that operation | operation timeout, rate limit, circuit breaker, audit policy |

An outer axum layer runs before transport validation and parsing. An MCP layer
runs after the request has become a typed router request. A handler layer runs
only after routing selected that capability. Build authentication with
`into_oauth_router`/`into_oauth_router_at`; those helpers keep bearer validation
and operation-scope enforcement in the intended order.

When using `HttpTransport::from_service`, wrap the supplied service before
construction. `HttpTransport::layer` is intentionally unavailable because the
transport cannot reconstruct the opaque service stack.

## Health, shutdown, and observability

The built-in health route returns process liveness. It does not prove an
external authorization server, session store, event store, or downstream tool
dependency is ready. Add a separate application readiness route when those
dependencies should gate traffic.

Own the axum server lifecycle when you need graceful shutdown:

```rust,no_run
use tower_mcp::{BoxError, HttpTransport, McpRouter};

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let app = HttpTransport::new(McpRouter::new()).into_router_at("/mcp");
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
```

Use structured `tracing` output and propagate a request ID or OpenTelemetry
context at the HTTP boundary. Avoid logging authorization headers, access
tokens, tool secrets, or sensitive arguments. `into_router_with_handle` can
feed active-session metrics, but high-cardinality session IDs should not be
metric labels.

## Production checklist

- [ ] Mount a stable public endpoint and test the exact public URL.
- [ ] Terminate TLS and configure OAuth or another appropriate trust boundary.
- [ ] Keep Origin validation enabled and configure exact browser origins.
- [ ] Configure the public host allowlist and preserve `Host` through proxies.
- [ ] Select the intended runtime protocol versions explicitly.
- [ ] Decide whether legacy sessions are optional or required.
- [ ] Use affinity and/or shared stores according to the legacy features in use.
- [ ] Disable proxy buffering and set stream-aware idle timeouts.
- [ ] Bound bodies, sessions, handler execution, and graceful drain time.
- [ ] Test JSON responses, POST SSE, legacy GET SSE/resumption if supported,
      final `subscriptions/listen`, 202 notifications, and error bodies.
- [ ] Separate liveness from dependency-aware readiness.
- [ ] Redact secrets and confirm logs remain on stderr for stdio deployments.

## Runnable examples

```bash
# Standalone HTTP server
cargo run --example http_server --features http

# Mount under /mcp in an existing axum app
cargo run --example axum_embedding --features http

# Shared-store and cross-instance patterns
cargo run --example horizontal_scaling --features http
cargo run --example session_store --features http
cargo run --example event_store --features http

# Final sessionless server/client in one process
cargo run --example stateless_http_client \
  --features "http,http-client,protocol-2026-07-28"
```

The feature-gated
[`tower_mcp::deployment`](https://docs.rs/tower-mcp/latest/tower_mcp/deployment/)
reference contains additional systemd, proxy, and capacity-planning notes.
"####]
