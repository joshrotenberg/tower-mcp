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
//! Each backend is an [`McpClient`](crate::client::McpClient) that the proxy initializes and manages.
//! Tool/resource/prompt discovery runs concurrently across all backends with
//! per-backend timeouts. Results are cached and refreshed on `toolListChanged`
//! notifications.
//!
//! # Example
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
//! // Use with any transport -- stdio, HTTP, WebSocket
//! let mut transport = tower_mcp::GenericStdioTransport::new(proxy);
//! transport.run().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Middleware
//!
//! Because `McpProxy` implements `Service<RouterRequest>`, standard tower
//! middleware composes naturally:
//!
//! ```rust,ignore
//! // Auth -> RateLimit -> Proxy
//! let service = ServiceBuilder::new()
//!     .layer(AuthLayer::new(validator))
//!     .layer(RateLimitLayer::new(100, Duration::from_secs(1)))
//!     .service(proxy);
//! ```

mod backend;
mod builder;
mod service;
mod tests;

pub use builder::McpProxyBuilder;
pub use service::{BackendHealth, McpProxy};

// Re-export BackendService so users can write layer bounds against it
pub use backend::BackendService;
