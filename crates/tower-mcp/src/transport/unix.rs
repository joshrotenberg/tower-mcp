//! Unix Domain Socket transport for MCP.
//!
//! Serves the Streamable HTTP protocol over a Unix socket instead of TCP.
//! This is useful for local-only server deployments where network exposure
//! is unnecessary and IPC performance matters (e.g., containerized/sidecar
//! deployments).
//!
//! The transport reuses all HTTP transport machinery (sessions, SSE
//! notifications, sampling) -- the only difference is the listener type.
//!
//! # Example
//!
//! ```rust,no_run
//! use tower_mcp::{McpRouter, UnixSocketTransport};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), tower_mcp::BoxError> {
//!     let router = McpRouter::new().server_info("unix-example", "1.0.0");
//!     let transport = UnixSocketTransport::new(router);
//!     transport.serve("/tmp/mcp.sock").await?;
//!     Ok(())
//! }
//! ```

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::Error;
use crate::router::McpRouter;
use crate::transport::graceful::serve_with_shutdown;
use crate::transport::http::{HttpTransport, SessionConfig, SessionHandle};
use crate::{ProtocolSupport, ProtocolSupportError};

/// A transport that serves the MCP Streamable HTTP protocol over a Unix
/// domain socket.
///
/// `UnixSocketTransport` wraps [`HttpTransport`] and delegates all protocol
/// handling to it. The only difference is that [`serve`](Self::serve) binds
/// a [`tokio::net::UnixListener`] instead of a TCP listener.
///
/// All `HttpTransport` features work identically: sessions, SSE notification
/// streams, sampling, `.layer()` middleware, and OAuth.
pub struct UnixSocketTransport {
    inner: HttpTransport,
    cleanup_on_bind: bool,
    /// How long [`serve_with_shutdown`](Self::serve_with_shutdown) waits for
    /// open connections once the shutdown signal fires. `None` waits for all
    /// of them.
    drain_timeout: Option<Duration>,
}

impl UnixSocketTransport {
    /// Create a new Unix socket transport wrapping an MCP router.
    pub fn new(router: McpRouter) -> Self {
        Self {
            inner: HttpTransport::new(router),
            cleanup_on_bind: true,
            drain_timeout: None,
        }
    }

    /// Create a Unix socket transport from a pre-built service.
    ///
    /// See [`HttpTransport::from_service`] for details.
    pub fn from_service<S>(service: S) -> Self
    where
        S: tower::Service<
                crate::router::RouterRequest,
                Response = crate::router::RouterResponse,
                Error = std::convert::Infallible,
            > + Clone
            + Send
            + 'static,
        S::Future: Send,
    {
        Self {
            inner: HttpTransport::from_service(service),
            cleanup_on_bind: true,
            drain_timeout: None,
        }
    }

    /// Enable sampling support.
    ///
    /// See [`HttpTransport::with_sampling`] for details.
    pub fn with_sampling(mut self) -> Self {
        self.inner = self.inner.with_sampling();
        self
    }

    /// Require session headers on every request.
    ///
    /// By default sessions are optional (matching [`HttpTransport`] defaults).
    /// Call this to reject requests without a valid `mcp-session-id`.
    pub fn require_sessions(mut self) -> Self {
        self.inner = self.inner.require_sessions();
        self
    }

    /// Set the exact protocol versions accepted by the delegated HTTP binding.
    pub fn protocol_support(mut self, support: ProtocolSupport) -> Self {
        self.inner = self.inner.protocol_support(support);
        self
    }

    /// Construct and set an exact runtime protocol-version allow-list.
    pub fn protocol_versions<I, V>(
        mut self,
        versions: I,
    ) -> std::result::Result<Self, ProtocolSupportError>
    where
        I: IntoIterator<Item = V>,
        V: Into<String>,
    {
        self.inner = self.inner.protocol_versions(versions)?;
        Ok(self)
    }

    /// Set session configuration (TTL, max sessions, cleanup interval).
    pub fn session_config(mut self, config: SessionConfig) -> Self {
        self.inner = self.inner.session_config(config);
        self
    }

    /// Set session time-to-live.
    pub fn session_ttl(mut self, ttl: std::time::Duration) -> Self {
        self.inner = self.inner.session_ttl(ttl);
        self
    }

    /// Set the maximum number of concurrent sessions.
    pub fn max_sessions(mut self, max: usize) -> Self {
        self.inner = self.inner.max_sessions(max);
        self
    }

    /// Configure a pluggable [`SessionStore`](crate::session_store::SessionStore)
    /// for persisting session metadata.
    ///
    /// See [`HttpTransport::session_store`] for details.
    pub fn session_store(
        mut self,
        store: std::sync::Arc<dyn crate::session_store::SessionStore>,
    ) -> Self {
        self.inner = self.inner.session_store(store);
        self
    }

    /// Configure a pluggable [`EventStore`](crate::event_store::EventStore)
    /// for SSE event buffering and stream resumption.
    ///
    /// See [`HttpTransport::event_store`] for details.
    pub fn event_store(
        mut self,
        store: std::sync::Arc<dyn crate::event_store::EventStore>,
    ) -> Self {
        self.inner = self.inner.event_store(store);
        self
    }

    /// Enable auto-reinitialization for unknown session IDs.
    ///
    /// See [`HttpTransport::auto_reinitialize_sessions`] for details.
    pub fn auto_reinitialize_sessions(mut self, enabled: bool) -> Self {
        self.inner = self.inner.auto_reinitialize_sessions(enabled);
        self
    }

    /// Disable origin validation.
    ///
    /// Origin validation is less relevant for Unix sockets since they are
    /// not accessible over the network, but is still enabled by default
    /// for consistency with [`HttpTransport`].
    pub fn disable_origin_validation(mut self) -> Self {
        self.inner = self.inner.disable_origin_validation();
        self
    }

    /// Set allowed origins for CORS validation.
    pub fn allowed_origins(mut self, origins: Vec<String>) -> Self {
        self.inner = self.inner.allowed_origins(origins);
        self
    }

    /// Disable Host header validation.
    ///
    /// Like origin validation, host validation is less relevant for Unix
    /// sockets, but is still enabled by default for consistency.
    pub fn disable_host_validation(mut self) -> Self {
        self.inner = self.inner.disable_host_validation();
        self
    }

    /// Set allowed hosts for the `Host` header allowlist.
    pub fn allowed_hosts(mut self, hosts: Vec<String>) -> Self {
        self.inner = self.inner.allowed_hosts(hosts);
        self
    }

    /// Apply a tower middleware layer to MCP request processing.
    ///
    /// See [`HttpTransport::layer`] for details.
    pub fn layer<L>(mut self, layer: L) -> Self
    where
        L: tower::Layer<McpRouter> + Send + Sync + 'static,
        L::Service: tower::Service<crate::router::RouterRequest, Response = crate::router::RouterResponse>
            + Clone
            + Send
            + 'static,
        <L::Service as tower::Service<crate::router::RouterRequest>>::Error:
            std::fmt::Display + Send,
        <L::Service as tower::Service<crate::router::RouterRequest>>::Future: Send,
    {
        self.inner = self.inner.layer(layer);
        self
    }

    /// If `true` (the default), remove an existing socket file before
    /// binding. Set to `false` if you manage the socket lifecycle yourself.
    pub fn cleanup_on_bind(mut self, cleanup: bool) -> Self {
        self.cleanup_on_bind = cleanup;
        self
    }

    /// Bound how long [`serve_with_shutdown`](Self::serve_with_shutdown)
    /// waits for open connections after the shutdown signal fires.
    ///
    /// See [`HttpTransport::drain_timeout`], which this mirrors. The bound
    /// matters here for the same reason: an SSE notification stream stays
    /// open until its client hangs up, so an unbounded drain can outlive the
    /// shutdown that triggered it.
    pub fn drain_timeout(mut self, timeout: Duration) -> Self {
        self.drain_timeout = Some(timeout);
        self
    }

    /// Build the axum router for this transport.
    ///
    /// Use this when you want to serve the router yourself with a custom
    /// [`tokio::net::UnixListener`] setup.
    pub fn into_router(self) -> axum::Router {
        self.inner.into_router()
    }

    /// Build the axum router and return a [`SessionHandle`] for querying
    /// session metrics.
    pub fn into_router_with_handle(self) -> (axum::Router, SessionHandle) {
        self.inner.into_router_with_handle()
    }

    /// Serve the transport on the given Unix socket path, forever.
    ///
    /// If `cleanup_on_bind` is enabled (the default), any existing file at
    /// `path` is removed before binding. The socket file is **not**
    /// automatically removed on shutdown -- callers should handle cleanup
    /// if needed (e.g., via a signal handler or `Drop` guard).
    ///
    /// This future never resolves on its own. Use
    /// [`serve_with_shutdown`](Self::serve_with_shutdown) in any process that
    /// has to stop the server without exiting.
    pub async fn serve<P: AsRef<Path>>(self, path: P) -> crate::Result<()> {
        self.serve_with_shutdown(path, std::future::pending::<()>())
            .await
    }

    /// Serve the transport on the given Unix socket path until `signal`
    /// resolves.
    ///
    /// The signal has the same shape as
    /// `axum::serve(..).with_graceful_shutdown(..)`, because that is what it
    /// drives: once it resolves the listener stops accepting, connections
    /// already open are given a chance to finish, and then this future
    /// returns. Binding still happens up front, so a bind error is reported
    /// before the signal is ever awaited.
    ///
    /// Set [`drain_timeout`](Self::drain_timeout) to bound the wait for open
    /// connections. Without it a client holding an SSE stream open keeps the
    /// server alive.
    ///
    /// ```rust,no_run
    /// use tower_mcp::{McpRouter, UnixSocketTransport};
    ///
    /// # async fn example() -> Result<(), tower_mcp::BoxError> {
    /// let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    /// let server = tokio::spawn(async move {
    ///     UnixSocketTransport::new(McpRouter::new())
    ///         .serve_with_shutdown("/tmp/mcp.sock", async {
    ///             rx.await.ok();
    ///         })
    ///         .await
    /// });
    ///
    /// // Later, from wherever the stop decision is made.
    /// tx.send(()).ok();
    /// server.await??;
    /// std::fs::remove_file("/tmp/mcp.sock").ok();
    /// # Ok(())
    /// # }
    /// ```
    pub async fn serve_with_shutdown<P, F>(self, path: P, signal: F) -> crate::Result<()>
    where
        P: AsRef<Path>,
        F: Future<Output = ()> + Send + 'static,
    {
        let path = path.as_ref().to_path_buf();

        if self.cleanup_on_bind {
            cleanup_socket(&path);
        }

        let listener = tokio::net::UnixListener::bind(&path).map_err(|e| {
            Error::Transport(format!(
                "Failed to bind Unix socket {}: {}",
                path.display(),
                e
            ))
        })?;

        tracing::info!("MCP Unix socket transport listening on {}", path.display());

        let drain_timeout = self.drain_timeout;
        let router = self.inner.into_router();
        serve_with_shutdown(listener, router, signal, drain_timeout).await
    }
}

/// Remove an existing socket file, ignoring "not found" errors.
fn cleanup_socket(path: &PathBuf) {
    match std::fs::remove_file(path) {
        Ok(()) => {
            tracing::debug!("Removed existing socket file: {}", path.display());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(
                "Failed to remove existing socket file {}: {}",
                path.display(),
                e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delegates_runtime_protocol_configuration_to_http() {
        let transport =
            UnixSocketTransport::new(McpRouter::new().server_info("unix-protocol-test", "0.0.0"))
                .protocol_support(ProtocolSupport::stable())
                .protocol_versions(["2025-11-25"])
                .unwrap();

        // Building the router exercises the delegated HTTP configuration;
        // UnixSocketTransport only replaces the listener.
        let _router = transport.into_router();
    }

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn delegated_http_binding_enforces_protocol_selection() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let app =
            UnixSocketTransport::new(McpRouter::new().server_info("unix-protocol-test", "0.0.0"))
                .protocol_support(ProtocolSupport::stable())
                .disable_origin_validation()
                .into_router();
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-method", "server/discover")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "server/discover",
                    "params": {
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientCapabilities": {}
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], -32022);
        assert_eq!(body["error"]["data"]["requested"], "2026-07-28");
    }
}
