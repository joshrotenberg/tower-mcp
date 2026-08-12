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
        crate::transport::graceful::serve_with_shutdown(listener, router, signal, drain_timeout)
            .await
    }

    /// Serve the transport on a listener the caller already owns, forever.
    ///
    /// Everything [`serve`](Self::serve) does except the two steps that
    /// belong to whoever owns the socket: binding, and the
    /// [`cleanup_on_bind`](Self::cleanup_on_bind) removal that precedes it.
    ///
    /// Use this to put a policy in front of `accept`. See
    /// [`serve_with_listener_and_shutdown`](Self::serve_with_listener_and_shutdown)
    /// for a peer-credential example.
    ///
    /// This future never resolves on its own. Use
    /// [`serve_with_listener_and_shutdown`](Self::serve_with_listener_and_shutdown)
    /// in any process that has to stop the server without exiting.
    pub async fn serve_with_listener<L>(self, listener: L) -> crate::Result<()>
    where
        L: axum::serve::Listener,
        L::Addr: std::fmt::Debug,
    {
        self.serve_with_listener_and_shutdown(listener, std::future::pending::<()>())
            .await
    }

    /// Serve the transport on a caller-owned listener until `signal` resolves.
    ///
    /// The shutdown behaviour is identical to
    /// [`serve_with_shutdown`](Self::serve_with_shutdown), including
    /// [`drain_timeout`](Self::drain_timeout); only the listener changes
    /// hands. Nothing here binds or removes a socket file, so the caller owns
    /// that lifecycle end to end.
    ///
    /// # Peer credentials
    ///
    /// Checking who is on the other end is close to the point of choosing a
    /// unix socket over TCP, and it needs the accepted stream before axum
    /// takes it. A [`Listener`](axum::serve::Listener) wrapper is that seam:
    /// `accept` returns no error, so a connection that fails the policy is
    /// dropped and the loop simply waits for the next one.
    ///
    /// ```rust,no_run
    /// use tokio::net::{UnixListener, UnixStream};
    /// use tokio::net::unix::SocketAddr;
    /// use tower_mcp::{McpRouter, UnixSocketTransport};
    ///
    /// /// Accepts only connections from `uid`.
    /// struct PeerUid {
    ///     inner: UnixListener,
    ///     uid: u32,
    /// }
    ///
    /// impl axum::serve::Listener for PeerUid {
    ///     type Io = UnixStream;
    ///     type Addr = SocketAddr;
    ///
    ///     async fn accept(&mut self) -> (Self::Io, Self::Addr) {
    ///         loop {
    ///             let Ok((stream, addr)) = self.inner.accept().await else {
    ///                 continue;
    ///             };
    ///             match stream.peer_cred() {
    ///                 // The peer is who we expect; hand it to axum.
    ///                 Ok(cred) if cred.uid() == self.uid => return (stream, addr),
    ///                 // Wrong user, or no credentials to check. Drop the
    ///                 // connection and wait for the next one.
    ///                 _ => continue,
    ///             }
    ///         }
    ///     }
    ///
    ///     fn local_addr(&self) -> std::io::Result<Self::Addr> {
    ///         self.inner.local_addr()
    ///     }
    /// }
    ///
    /// # async fn example(allowed_uid: u32) -> Result<(), tower_mcp::BoxError> {
    /// let listener = PeerUid {
    ///     inner: UnixListener::bind("/tmp/mcp.sock")?,
    ///     uid: allowed_uid,
    /// };
    ///
    /// let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    /// UnixSocketTransport::new(McpRouter::new())
    ///     .serve_with_listener_and_shutdown(listener, async {
    ///         rx.await.ok();
    ///     })
    ///     .await?;
    /// # let _ = tx;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn serve_with_listener_and_shutdown<L, F>(
        self,
        listener: L,
        signal: F,
    ) -> crate::Result<()>
    where
        L: axum::serve::Listener,
        L::Addr: std::fmt::Debug,
        F: Future<Output = ()> + Send + 'static,
    {
        let drain_timeout = self.drain_timeout;
        let router = self.inner.into_router();
        crate::transport::graceful::serve_with_shutdown(listener, router, signal, drain_timeout)
            .await
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};

    /// A socket path short enough for `sun_path`, unique per test.
    fn socket_path() -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        std::env::temp_dir().join(format!(
            "tm-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn test_transport() -> UnixSocketTransport {
        UnixSocketTransport::new(McpRouter::new().server_info("unix-listener-test", "0.0.0"))
            .disable_origin_validation()
            .disable_host_validation()
    }

    fn initialize_frame() -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "unix-test", "version": "1.0.0"}
            }
        })
        .to_string()
    }

    /// One HTTP/1.1 POST over a unix socket, returning the whole raw response.
    /// Empty means the server hung up without answering.
    async fn post(path: &Path, body: &str) -> String {
        let mut stream = UnixStream::connect(path).await.expect("connect");
        let request = format!(
            "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len(),
        );
        stream.write_all(request.as_bytes()).await.expect("write");
        let mut response = String::new();
        match stream.read_to_string(&mut response).await {
            Ok(_) => {}
            // A peer that drops the connection with our request still unread
            // resets it on Linux rather than closing cleanly, where macOS
            // reports plain end-of-input. Both mean the same thing here, so
            // keep whatever arrived and let the caller judge it.
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            Err(error) => panic!("read: {error}"),
        }
        response
    }

    /// #1286: a caller that binds the listener itself still gets the
    /// transport's serve behaviour, graceful shutdown included.
    #[tokio::test]
    async fn serves_on_a_caller_owned_listener() {
        let path = socket_path();
        let listener = UnixListener::bind(&path).expect("bind");

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(test_transport().serve_with_listener_and_shutdown(
            listener,
            async {
                rx.await.ok();
            },
        ));

        let response = post(&path, &initialize_frame()).await;
        assert!(
            response.contains("200 OK"),
            "expected a served response, got: {response}"
        );
        assert!(
            response.contains("serverInfo"),
            "expected an initialize result, got: {response}"
        );

        tx.send(()).ok();
        let served = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("shutdown signal must stop the server");
        served.expect("join").expect("serve");

        cleanup_socket(&path);
    }

    /// The seam the issue asked for: a wrapper in front of `accept` decides
    /// which connections reach the transport at all. This one refuses every
    /// connection, which is what a peer-credential check does to a caller it
    /// does not recognise.
    #[tokio::test]
    async fn a_rejecting_listener_wrapper_is_consulted() {
        struct RefuseAll {
            inner: UnixListener,
            seen: Arc<AtomicUsize>,
        }

        impl axum::serve::Listener for RefuseAll {
            type Io = UnixStream;
            type Addr = tokio::net::unix::SocketAddr;

            async fn accept(&mut self) -> (Self::Io, Self::Addr) {
                loop {
                    if let Ok((stream, _addr)) = self.inner.accept().await {
                        self.seen.fetch_add(1, Ordering::SeqCst);
                        drop(stream);
                    }
                }
            }

            fn local_addr(&self) -> std::io::Result<Self::Addr> {
                self.inner.local_addr()
            }
        }

        let path = socket_path();
        let seen = Arc::new(AtomicUsize::new(0));
        let listener = RefuseAll {
            inner: UnixListener::bind(&path).expect("bind"),
            seen: seen.clone(),
        };

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(test_transport().serve_with_listener_and_shutdown(
            listener,
            async {
                rx.await.ok();
            },
        ));

        let response = post(&path, &initialize_frame()).await;
        assert!(
            response.is_empty(),
            "a refused connection must not be served: {response}"
        );
        assert_eq!(
            seen.load(Ordering::SeqCst),
            1,
            "the wrapper must be the one accepting"
        );

        tx.send(()).ok();
        server.abort();
        cleanup_socket(&path);
    }

    /// The use case behind #1286, end to end: the documented `peer_cred`
    /// filter admits a caller running as the expected uid.
    ///
    /// The uid to expect comes from a file this process just created, since
    /// that is the euid the socket peer will report.
    #[tokio::test]
    async fn a_peer_credential_filter_admits_the_expected_uid() {
        use std::os::unix::fs::MetadataExt;

        struct PeerUid {
            inner: UnixListener,
            uid: u32,
        }

        impl axum::serve::Listener for PeerUid {
            type Io = UnixStream;
            type Addr = tokio::net::unix::SocketAddr;

            async fn accept(&mut self) -> (Self::Io, Self::Addr) {
                loop {
                    let Ok((stream, addr)) = self.inner.accept().await else {
                        continue;
                    };
                    match stream.peer_cred() {
                        Ok(cred) if cred.uid() == self.uid => return (stream, addr),
                        _ => continue,
                    }
                }
            }

            fn local_addr(&self) -> std::io::Result<Self::Addr> {
                self.inner.local_addr()
            }
        }

        let marker = socket_path().with_extension("uid");
        std::fs::write(&marker, b"").expect("write marker");
        let uid = std::fs::metadata(&marker).expect("stat marker").uid();
        std::fs::remove_file(&marker).ok();

        let path = socket_path();
        let listener = PeerUid {
            inner: UnixListener::bind(&path).expect("bind"),
            uid,
        };

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(test_transport().serve_with_listener_and_shutdown(
            listener,
            async {
                rx.await.ok();
            },
        ));

        let response = post(&path, &initialize_frame()).await;
        assert!(
            response.contains("200 OK") && response.contains("serverInfo"),
            "a peer with the expected uid must be served: {response}"
        );

        tx.send(()).ok();
        let served = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("shutdown signal must stop the server");
        served.expect("join").expect("serve");

        cleanup_socket(&path);
    }

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
