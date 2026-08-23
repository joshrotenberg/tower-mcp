//! Production deployment guide.
//!
//! This module is documentation only. It covers operational patterns for
//! running `tower-mcp` servers in production: load balancers, session
//! affinity, horizontal scaling, reverse proxies, observability, and
//! containerized deployments.
//!
//! # Overview
//!
//! The legacy 2025-11-25 lifecycle may be session-oriented. The
//! `mcp-session-id` header established during `initialize` ties subsequent
//! requests together, and `HttpTransport` keeps live state per session
//! (broadcast channel for SSE notifications, pending sampling requests,
//! service instance). The 2026-07-28 lifecycle is sessionless; ordinary
//! requests can be distributed independently, although application state and
//! long-lived subscription streams may still influence routing. Two shapes
//! cover most legacy or mixed-version deployments:
//!
//! 1. **Session affinity.** A load balancer routes all requests for a
//!    given session back to the same server instance. Any in-memory
//!    session store works; no cross-instance state needed.
//! 2. **Shared stores.** Server instances plug [`session_store`] and
//!    [`event_store`] into an external backend (Redis, Postgres, etc.) so
//!    session metadata and resumable notification event buffers survive
//!    across instances. This does not migrate live request state.
//!
//! Legacy sampling, elicitation, and roots requests are sent on the response
//! stream of the POST that caused them. Their channels are intentionally
//! process-local and non-resumable, so those exchanges require session
//! affinity even when shared stores are configured.
//!
//! # Single-Instance Deployment
//!
//! The minimal production server is the same shape as the examples:
//!
//! ```rust,no_run
//! use tower_mcp::{HttpTransport, McpRouter};
//!
//! # async fn run() -> Result<(), tower_mcp::BoxError> {
//! let router = McpRouter::new().server_info("my-server", "1.0.0");
//! let transport = HttpTransport::new(router);
//! transport.serve("0.0.0.0:3000").await?;
//! # Ok(()) }
//! ```
//!
//! ## systemd unit
//!
//! Run as a service with restart-on-failure and structured logging
//! captured by the journal:
//!
//! ```ini
//! [Unit]
//! Description=MCP server
//! After=network.target
//!
//! [Service]
//! ExecStart=/usr/local/bin/my-mcp-server
//! Restart=on-failure
//! RestartSec=5s
//! Environment=RUST_LOG=info
//!
//! # Drop privileges
//! User=mcp
//! Group=mcp
//!
//! # Systemd hardening (optional but recommended)
//! ProtectSystem=strict
//! ProtectHome=true
//! PrivateTmp=true
//! NoNewPrivileges=true
//!
//! [Install]
//! WantedBy=multi-user.target
//! ```
//!
//! ## Graceful shutdown
//!
//! Every transport that binds its own listener takes a shutdown future.
//! Once it resolves, the listener stops accepting, requests already in
//! flight are answered, and `serve_with_shutdown` returns:
//!
//! ```rust,no_run
//! # use tower_mcp::{HttpTransport, McpRouter};
//! # async fn run() -> Result<(), tower_mcp::BoxError> {
//! let router = McpRouter::new().server_info("my-server", "1.0.0");
//!
//! HttpTransport::new(router)
//!     .serve_with_shutdown("0.0.0.0:3000", async {
//!         tokio::signal::ctrl_c().await.ok();
//!     })
//!     .await?;
//! # Ok(()) }
//! ```
//!
//! The wait for in-flight work is unbounded by default, so nothing already
//! accepted is dropped. A client holding an SSE notification stream open is
//! also an in-flight request, and it can outlast the shutdown that started
//! it. [`drain_timeout`](crate::HttpTransport::drain_timeout) bounds that
//! wait:
//!
//! ```rust,no_run
//! # use std::time::Duration;
//! # use tower_mcp::{HttpTransport, McpRouter};
//! # async fn run() -> Result<(), tower_mcp::BoxError> {
//! # let router = McpRouter::new().server_info("my-server", "1.0.0");
//! HttpTransport::new(router)
//!     .drain_timeout(Duration::from_secs(10))
//!     .serve_with_shutdown("0.0.0.0:3000", async {
//!         tokio::signal::ctrl_c().await.ok();
//!     })
//!     .await?;
//! # Ok(()) }
//! ```
//!
//! `UnixSocketTransport` and `WebSocketTransport` take the same shutdown
//! future. A WebSocket leaves axum's connection tracking when it is
//! upgraded, so that transport has no `drain_timeout`: shutting down stops
//! new clients getting in, and open sockets run until their clients hang up.
//! If you serve the router yourself with `into_router()`, the equivalent of
//! all this is `axum::serve(..).with_graceful_shutdown(..)`.
//!
//! A built-in live Task handler is detached after its `tools/call` returns a
//! Task object, so transport request drain does not wait for that handler.
//! Obtain [`McpRouter::live_task_execution_handle`](crate::McpRouter::live_task_execution_handle)
//! before moving the router, close its admission when shutdown begins, and
//! then either await natural settlement or call
//! [`cancel_all`](crate::LiveTaskExecutionHandle::cancel_all) before a
//! caller-bounded [`drained`](crate::LiveTaskExecutionHandle::drained) wait.
//! The transport intentionally does not choose that cancellation policy.
//!
//! # Health Checks
//!
//! [`HttpTransport`] exposes `GET /health` returning `200 OK` with a JSON
//! body. Use it as the liveness/readiness probe for your load balancer or
//! orchestrator:
//!
//! ```yaml
//! # Kubernetes
//! livenessProbe:
//!   httpGet:
//!     path: /health
//!     port: 3000
//!   initialDelaySeconds: 5
//!   periodSeconds: 10
//!
//! readinessProbe:
//!   httpGet:
//!     path: /health
//!     port: 3000
//!   periodSeconds: 5
//! ```
//!
//! The health endpoint does not require a session; it's safe to probe
//! without establishing MCP state.
//!
//! # Load Balancer Patterns
//!
//! ## Session affinity (simplest)
//!
//! Configure the load balancer to consistently route the same session to
//! the same backend. Two common strategies:
//!
//! - **Hash on `mcp-session-id` header.** Gives exact session affinity.
//!   Requires a Layer 7 load balancer that can read application headers
//!   (nginx, HAProxy, Envoy, AWS ALB, Azure Application Gateway).
//! - **Source-IP hash.** Works with L4 load balancers but breaks when
//!   clients share NAT egress or reconnect from different networks.
//!
//! Session affinity is the right default: simpler to reason about, no
//! external dependencies, and matches how SSE streams behave naturally.
//!
//! ## Azure Load Balancer idle timeout (gotcha)
//!
//! Azure Standard Load Balancer defaults to a 4-minute TCP idle timeout.
//! Long-running SSE streams without traffic will be dropped. Raise the
//! idle timeout (`idle_timeout_in_minutes` in Terraform, up to 30 min) or
//! emit periodic SSE keepalives to keep the connection warm.
//!
//! AWS NLB and GCP NLB have similar idle-timeout semantics. Application
//! load balancers (AWS ALB, Azure Application Gateway) generally handle
//! this better.
//!
//! ## No affinity: use shared stores
//!
//! When you can't pin sessions (e.g. scaling events remap hashes, clients
//! cross VPC boundaries, stateless API gateways), externalize both:
//!
//! ```rust,no_run
//! # use std::sync::Arc;
//! # use tower_mcp::{HttpTransport, McpRouter};
//! # use tower_mcp::session_store::{MemorySessionStore, SessionStore};
//! # use tower_mcp::event_store::{EventStore, MemoryEventStore};
//! # async fn run() -> Result<(), tower_mcp::BoxError> {
//! # let router = McpRouter::new();
//! // Replace MemorySessionStore/MemoryEventStore with Redis-backed
//! // implementations in production.
//! let session_store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
//! let event_store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::new());
//!
//! HttpTransport::new(router)
//!     .session_store(session_store)
//!     .event_store(event_store)
//!     .serve("0.0.0.0:3000")
//!     .await?;
//! # Ok(()) }
//! ```
//!
//! See [`session_store`] and [`event_store`] for the trait definitions and
//! the included in-memory / caching implementations. Shared stores recover
//! session metadata and resumable notification events; they do not move a
//! live associated POST stream, its pending response handles, or its service
//! future to another instance. Route the response POSTs for server-initiated
//! requests back to the instance that owns the originating POST.
//!
//! # Reverse Proxies
//!
//! tower-mcp's SSE streams require buffering disabled and generous read
//! timeouts. Three common proxies:
//!
//! ## nginx
//!
//! ```nginx
//! location / {
//!     proxy_pass http://mcp_backend;
//!     proxy_http_version 1.1;
//!
//!     # Disable buffering for SSE
//!     proxy_buffering off;
//!     proxy_cache off;
//!
//!     # Preserve connection for SSE
//!     proxy_set_header Connection "";
//!     chunked_transfer_encoding on;
//!
//!     # Long-lived streams: must exceed session TTL (default 30 min)
//!     proxy_read_timeout 3600s;
//!     proxy_send_timeout 3600s;
//!
//!     # Preserve MCP headers
//!     proxy_set_header Host $host;
//!     proxy_set_header X-Real-IP $remote_addr;
//! }
//! ```
//!
//! ## Caddy
//!
//! ```caddyfile
//! mcp.example.com {
//!     reverse_proxy mcp-backend:3000 {
//!         flush_interval -1
//!         transport http {
//!             read_timeout 1h
//!             write_timeout 1h
//!         }
//!     }
//! }
//! ```
//!
//! `flush_interval -1` disables response buffering, which is required for
//! SSE to stream events as they happen.
//!
//! ## Traefik
//!
//! ```yaml
//! http:
//!   middlewares:
//!     sse-no-buffer:
//!       buffering:
//!         maxResponseBodyBytes: 0
//!   services:
//!     mcp:
//!       loadBalancer:
//!         servers:
//!           - url: "http://mcp-backend:3000"
//!         responseForwarding:
//!           flushInterval: "-1ms"
//! ```
//!
//! # Observability
//!
//! ## Structured logging
//!
//! Use `tracing-subscriber` with JSON output for machine-parseable logs.
//! The transport emits spans keyed by `session_id`, letting you correlate
//! every log line for a session:
//!
//! ```rust,ignore
//! // Requires the "json" feature on tracing-subscriber.
//! use tracing_subscriber::{EnvFilter, fmt};
//!
//! fmt()
//!     .json()
//!     .with_env_filter(EnvFilter::from_default_env())
//!     .init();
//! ```
//!
//! ## Tower middleware for request-level tracing
//!
//! [`HttpTransport::layer`](crate::HttpTransport::layer) stacks tower
//! middleware on every MCP request. `tower-http`'s `TraceLayer` is a good
//! starting point:
//!
//! ```rust,no_run
//! # use tower_mcp::{HttpTransport, McpRouter};
//! # use tower::ServiceBuilder;
//! # async fn run() -> Result<(), tower_mcp::BoxError> {
//! # let router = McpRouter::new();
//! // Wrap the MCP router in tracing middleware.
//! let transport = HttpTransport::new(router);
//! // Additional axum-level middleware can be applied to the resulting
//! // Router via .layer(), including tower_http::trace::TraceLayer.
//! let app = transport.into_router();
//! // app = app.layer(tower_http::trace::TraceLayer::new_for_http());
//! # Ok(()) }
//! ```
//!
//! ## Session metrics
//!
//! [`HttpTransport::into_router_with_handle`](crate::HttpTransport::into_router_with_handle)
//! returns a [`SessionHandle`](crate::SessionHandle) you can use from an
//! admin endpoint or metrics exporter:
//!
//! ```rust,no_run
//! # use tower_mcp::{HttpTransport, McpRouter};
//! # async fn run() -> Result<(), tower_mcp::BoxError> {
//! # let router = McpRouter::new();
//! let (app, handle) = HttpTransport::new(router).into_router_with_handle();
//!
//! // Somewhere else:
//! let count = handle.session_count().await;
//! for info in handle.list_sessions().await {
//!     tracing::info!(session_id = %info.id, age = ?info.created_at, "active");
//! }
//! # Ok(()) }
//! ```
//!
//! # Containerized and Sidecar Patterns
//!
//! For pod-local or sidecar deployments where only processes on the same
//! host need to talk to the server, use the Unix socket transport. It
//! avoids TCP overhead and removes the need to pick a port:
//!
//! ```rust,no_run
//! # #[cfg(all(unix, feature = "unix"))]
//! # async fn run() -> Result<(), tower_mcp::BoxError> {
//! # use tower_mcp::{McpRouter, UnixSocketTransport};
//! let router = McpRouter::new().server_info("sidecar", "1.0.0");
//! UnixSocketTransport::new(router)
//!     .serve("/run/mcp/server.sock")
//!     .await?;
//! # Ok(()) }
//! ```
//!
//! Pair this with a volume mount in Kubernetes to expose the socket to
//! other containers in the same pod, or to the host filesystem for
//! systemd socket activation.
//!
//! # Capacity Planning
//!
//! - **Per-session memory.** Each live session holds a broadcast channel
//!   (100 messages) and an event buffer (default 1000 events). Plan for
//!   roughly 100 KB/session as a rough ceiling.
//! - **Cleanup interval.** Default 1 minute. Expired sessions are removed
//!   from the registry and purged from the session/event stores on the
//!   cleanup pass.
//! - **Session TTL.** Default 30 minutes. Tune via
//!   [`HttpTransport::session_ttl`](crate::HttpTransport::session_ttl).
//!   Balance between memory pressure and forcing clients to re-initialize.
//! - **Max sessions.** Cap with
//!   [`HttpTransport::max_sessions`](crate::HttpTransport::max_sessions)
//!   to backpressure clients when the server is saturated.
//!
//! # See Also
//!
//! - [`crate::guides::deployment`] is the task-oriented starting point for
//!   endpoint mounting, proxy headers, protocol policy, timeouts, and
//!   production checklists.
//! - [`session_store`] for the `SessionStore` trait and implementations.
//! - [`event_store`] for the `EventStore` trait and SSE event persistence.
//! - The `session_store` and `event_store` examples in the repo show
//!   wrapping stores with logging/caching wrappers.
//!
//! [`session_store`]: crate::session_store
//! [`event_store`]: crate::event_store
//! [`HttpTransport`]: crate::HttpTransport
