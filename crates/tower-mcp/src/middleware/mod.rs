//! MCP-specific tower middleware layers.
//!
//! This module provides middleware layers designed for MCP request processing.
//! All layers implement [`tower::Layer`] and work with
//! [`RouterRequest`](crate::router::RouterRequest) /
//! [`RouterResponse`](crate::router::RouterResponse) types.
//!
//! # Available Layers
//!
//! | Layer | Purpose |
//! |-------|---------|
//! | [`McpTracingLayer`] | Structured tracing for all MCP requests |
//! | [`ToolCallLoggingLayer`] | Focused audit logging for tool calls |
//! | [`AuditLayer`] | Comprehensive audit events for all MCP requests |
//!
//! # Resilience Middleware (feature = "resilience")
//!
//! With the `resilience` feature enabled, tower-resilience middleware is
//! re-exported with observable state handles for admin/monitoring APIs:
//!
//! | Layer | Handle | Purpose |
//! |-------|--------|---------|
//! | `CircuitBreakerLayer` | `CircuitBreakerHandle` | Failure-rate circuit breaking |
//! | `RateLimiterLayer` | `RateLimiterHandle` | Rate limiting |
//! | `BulkheadLayer` | `BulkheadHandle` | Concurrency limiting |
//!
//! Use `build_with_handle()` on any builder to get both a layer and a handle:
//!
//! ```rust,ignore
//! use tower_mcp::middleware::CircuitBreakerLayer;
//!
//! let (layer, handle) = CircuitBreakerLayer::builder()
//!     .failure_rate_threshold(0.5)
//!     .build_with_handle();
//!
//! // handle.state(), handle.health_status(), handle.metrics().await
//! ```
//!
//! Errors from resilience layers are automatically converted to JSON-RPC
//! errors by `CatchError` when used with `HttpTransport::layer()` or
//! `McpProxyBuilder::backend_layer()`.
//!
//! # Usage
//!
//! Apply at the transport level for all requests:
//!
//! ```rust,ignore
//! use tower::ServiceBuilder;
//! use tower_mcp::middleware::{AuditLayer, McpTracingLayer, ToolCallLoggingLayer};
//!
//! let transport = StdioTransport::new(router)
//!     .layer(
//!         ServiceBuilder::new()
//!             .layer(McpTracingLayer::new())
//!             .layer(AuditLayer::new())
//!             .into_inner(),
//!     );
//! ```
//!
//! # Writing Custom Middleware
//!
//! ## Preserving extensions when rewriting requests
//!
//! When middleware rewrites a [`RouterRequest`](crate::router::RouterRequest),
//! use [`with_inner`](crate::router::RouterRequest::with_inner) or
//! [`clone_with_inner`](crate::router::RouterRequest::clone_with_inner) to
//! preserve extensions set by earlier layers (token claims, RBAC context, etc.):
//!
//! ```rust,ignore
//! fn call(&mut self, req: RouterRequest) -> Self::Future {
//!     let rewritten = req.with_inner(new_mcp_request);
//!     self.inner.call(rewritten)
//! }
//! ```
//!
//! For traffic mirroring or fan-out (where the original request is still
//! needed), use `clone_with_inner`:
//!
//! ```rust,ignore
//! let mirror_req = req.clone_with_inner(req.inner.clone());
//! ```
//!
//! ## Error handling and retry
//!
//! tower-mcp services use `Error = Infallible` -- errors are carried inside
//! [`RouterResponse::inner`](crate::router::RouterResponse) as
//! `Result<McpResponse, JsonRpcError>`, not in the outer `Result`. This means
//! standard tower retry middleware (which checks `Result::Err`) will never
//! trigger retries.
//!
//! Use [`RouterResponse::is_error()`](crate::router::RouterResponse::is_error)
//! with a response-based retry predicate:
//!
//! ```rust,ignore
//! fn should_retry(response: &RouterResponse) -> bool {
//!     response.is_error()
//! }
//! ```

mod audit;
mod tool_call_logging;
mod tracing;

pub use audit::{AuditLayer, AuditService};
pub use tool_call_logging::{ToolCallLoggingLayer, ToolCallLoggingService};
pub use tracing::{McpTracingLayer, McpTracingService};

#[cfg(feature = "resilience")]
pub use tower_resilience::circuitbreaker::{
    CircuitBreakerError, CircuitBreakerHandle, CircuitBreakerLayer, CircuitState,
};

#[cfg(feature = "resilience")]
pub use tower_resilience::ratelimiter::{RateLimiterHandle, RateLimiterLayer};

#[cfg(feature = "resilience")]
pub use tower_resilience::bulkhead::{BulkheadHandle, BulkheadLayer};
