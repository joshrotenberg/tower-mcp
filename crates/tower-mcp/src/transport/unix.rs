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

use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::router::McpRouter;
use crate::transport::http::{HttpTransport, SessionConfig, SessionHandle};

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
}

impl UnixSocketTransport {
    /// Create a new Unix socket transport wrapping an MCP router.
    pub fn new(router: McpRouter) -> Self {
        Self {
            inner: HttpTransport::new(router),
            cleanup_on_bind: true,
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

    /// Serve the transport on the given Unix socket path.
    ///
    /// If `cleanup_on_bind` is enabled (the default), any existing file at
    /// `path` is removed before binding. The socket file is **not**
    /// automatically removed on shutdown -- callers should handle cleanup
    /// if needed (e.g., via a signal handler or `Drop` guard).
    pub async fn serve<P: AsRef<Path>>(self, path: P) -> crate::Result<()> {
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

        let router = self.inner.into_router();
        axum::serve(listener, router)
            .await
            .map_err(|e| Error::Transport(format!("Server error: {}", e)))?;

        Ok(())
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
