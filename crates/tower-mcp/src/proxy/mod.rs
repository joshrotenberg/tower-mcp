//! MCP Proxy -- aggregate multiple backend MCP servers behind a single endpoint.
//!
//! The proxy connects to N backend MCP servers and exposes their combined
//! tools, resources, and prompts through a unified `Service<RouterRequest>`
//! interface. Each backend's capabilities are namespaced to avoid collisions.
//!
//! # Architecture
//!
//! ```text
//!                     McpProxy
//!                    /    |    \
//!           Backend A  Backend B  Backend C
//!           (stdio)    (HTTP)     (stdio)
//! ```
//!
//! Each backend is an [`McpClient`](crate::client::McpClient) that the proxy
//! initializes and manages. Tool/resource/prompt discovery runs concurrently
//! across all backends. Results are cached and automatically refreshed when
//! backends emit `tools/list_changed`, `resources/list_changed`, or
//! `prompts/list_changed` notifications.
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use tower_mcp::proxy::McpProxy;
//! use tower_mcp::client::StdioClientTransport;
//!
//! # async fn example() -> Result<(), tower_mcp::BoxError> {
//! let proxy = McpProxy::builder("my-proxy", "1.0.0")
//!     .backend("db", StdioClientTransport::spawn("db-server", &[]).await?)
//!     .await
//!     .backend("fs", StdioClientTransport::spawn("fs-server", &[]).await?)
//!     .await
//!     .build()
//!     .await?;
//!
//! // Tools become db_query, fs_read, etc. (namespace + separator + name)
//! // Serve over any transport -- stdio, HTTP, WebSocket.
//! let mut transport = tower_mcp::GenericStdioTransport::new(proxy);
//! transport.run().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Namespacing
//!
//! All tools, resources, and prompts from each backend are prefixed with
//! `{namespace}{separator}` to avoid naming collisions. The default separator
//! is `_`, so a tool named `query` on the `db` backend becomes `db_query`.
//!
//! Use [`McpProxyBuilder::separator()`] to change the separator:
//!
//! ```rust,ignore
//! McpProxy::builder("proxy", "1.0.0")
//!     .backend("db", transport).await
//!     .separator(".")   // tools become "db.query" instead of "db_query"
//!     .build().await?;
//! ```
//!
//! # Per-Backend Middleware
//!
//! Apply Tower middleware to individual backends using
//! [`McpProxyBuilder::backend_layer()`]. This is useful for backend-specific
//! timeouts, rate limits, or retry policies:
//!
//! ```rust,ignore
//! use std::time::Duration;
//! use tower::timeout::TimeoutLayer;
//!
//! let proxy = McpProxy::builder("proxy", "1.0.0")
//!     // Fast backend: tight timeout
//!     .backend("cache", cache_transport).await
//!     .backend_layer(TimeoutLayer::new(Duration::from_secs(2)))
//!     // Slow backend: generous timeout
//!     .backend("llm", llm_transport).await
//!     .backend_layer(TimeoutLayer::new(Duration::from_secs(60)))
//!     // No middleware on this one
//!     .backend("db", db_transport).await
//!     .build().await?;
//! ```
//!
//! Middleware errors (e.g., `tower::timeout::error::Elapsed`) are automatically
//! converted to JSON-RPC error responses via
//! [`CatchError`](crate::transport::CatchError), preserving the
//! `Error = Infallible` contract required by transports.
//!
//! # Proxy-Level Middleware
//!
//! Because [`McpProxy`] implements `Service<RouterRequest>`, standard Tower
//! middleware composes naturally at the proxy level:
//!
//! ```rust,ignore
//! use tower::ServiceBuilder;
//!
//! let service = ServiceBuilder::new()
//!     .layer(AuthLayer::new(validator))
//!     .layer(RateLimitLayer::new(100, Duration::from_secs(1)))
//!     .service(proxy);
//! ```
//!
//! # Notification Forwarding
//!
//! When a backend emits a list-changed notification (e.g., after adding or
//! removing tools at runtime), the proxy:
//!
//! 1. Refreshes its cached capabilities for that backend
//! 2. Forwards the notification to connected downstream clients
//!
//! To enable forwarding, provide a [`NotificationSender`](crate::context::NotificationSender)
//! via [`McpProxyBuilder::notification_sender()`] and wire it to a transport
//! that supports notifications:
//!
//! ```rust,ignore
//! use tower_mcp::context::notification_channel;
//!
//! let (notif_tx, notif_rx) = notification_channel(32);
//!
//! let proxy = McpProxy::builder("proxy", "1.0.0")
//!     .notification_sender(notif_tx)
//!     .backend("tools", transport).await
//!     .build().await?;
//!
//! // GenericStdioTransport forwards notifications to connected clients
//! let mut transport = tower_mcp::GenericStdioTransport::with_notifications(proxy, notif_rx);
//! transport.run().await?;
//! ```
//!
//! # Health Checks
//!
//! Ping all backends concurrently to verify connectivity:
//!
//! ```rust,ignore
//! let health = proxy.health_check().await;
//! for h in &health {
//!     println!("{}: {}", h.namespace, if h.healthy { "ok" } else { "down" });
//! }
//! ```
//!
//! # Request Coalescing
//!
//! Use [`tower-resilience`](https://docs.rs/tower-resilience)'s `CoalesceLayer`
//! to deduplicate concurrent identical requests. This is especially useful for
//! list operations when multiple clients connect simultaneously:
//!
//! ```rust,ignore
//! use std::mem::discriminant;
//! use tower::Layer;
//! use tower_resilience::coalesce::CoalesceLayer;
//!
//! // Coalesce by MCP method type -- concurrent list_tools calls share
//! // a single execution, while call_tool runs independently.
//! let coalesced = CoalesceLayer::new(|req: &RouterRequest| {
//!     discriminant(&req.inner)
//! }).layer(proxy);
//! ```
//!
//! Note that `CoalesceLayer` changes the error type from `Infallible` to
//! `CoalesceError<Infallible>`. Use [`CatchError`](crate::transport::CatchError)
//! to convert back to `Infallible` for transport compatibility.
//!
//! # Language-Agnostic Backends
//!
//! The proxy communicates with backends over standard MCP (JSON-RPC), so
//! backends can be written in any language or framework -- Python (FastMCP),
//! TypeScript, Go, or anything that speaks the protocol. This makes
//! tower-mcp a natural aggregation and middleware layer for polyglot MCP
//! deployments:
//!
//! ```rust,ignore
//! let proxy = McpProxy::builder("polyglot-proxy", "1.0.0")
//!     // Python FastMCP server
//!     .backend("ml", StdioClientTransport::spawn("python", &["-m", "ml_server"]).await?)
//!     .await
//!     // TypeScript MCP server
//!     .backend("docs", StdioClientTransport::spawn("npx", &["docs-server"]).await?)
//!     .await
//!     // Rust tower-mcp server
//!     .backend("data", StdioClientTransport::spawn("data-server", &[]).await?)
//!     .await
//!     .build().await?;
//! ```

mod backend;
mod builder;
mod service;
mod tests;

pub use builder::McpProxyBuilder;
pub use service::{BackendHealth, McpProxy};

// Re-export BackendService so users can write layer bounds against it
pub use backend::BackendService;
