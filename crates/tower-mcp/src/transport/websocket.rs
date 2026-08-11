//! WebSocket transport for MCP
//!
//! Provides full-duplex communication over WebSocket, ideal for:
//! - Bidirectional notifications
//! - Long-lived connections
//! - Lower latency than HTTP polling
//! - Server-to-client requests (sampling)
//!
//! # Example
//!
//! ```rust,no_run
//! use tower_mcp::{BoxError, McpRouter, ToolBuilder, CallToolResult};
//! use tower_mcp::transport::websocket::WebSocketTransport;
//! use schemars::JsonSchema;
//! use serde::Deserialize;
//!
//! #[derive(Debug, Deserialize, JsonSchema)]
//! struct Input { value: String }
//!
//! #[tokio::main]
//! async fn main() -> Result<(), BoxError> {
//!     let tool = ToolBuilder::new("echo")
//!         .handler(|i: Input| async move { Ok(CallToolResult::text(i.value)) })
//!         .build();
//!
//!     let router = McpRouter::new()
//!         .server_info("my-server", "1.0.0")
//!         .tool(tool);
//!
//!     let transport = WebSocketTransport::new(router);
//!     transport.serve("127.0.0.1:3000").await?;
//!     Ok(())
//! }
//! ```
//!
//! # Sampling Support
//!
//! The WebSocket transport supports server-to-client requests like sampling.
//! Use [`WebSocketTransport::new`] with [`with_sampling`](WebSocketTransport::with_sampling) to enable:
//!
//! ```rust,no_run
//! use tower_mcp::{BoxError, McpRouter, ToolBuilder, CallToolResult, CreateMessageParams, SamplingMessage};
//! use tower_mcp::extract::{Context, RawArgs};
//! use tower_mcp::transport::websocket::WebSocketTransport;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), BoxError> {
//!     let tool = ToolBuilder::new("ai-tool")
//!         .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
//!             // Request LLM completion from client
//!             let params = CreateMessageParams::new(
//!                 vec![SamplingMessage::user("Summarize this...")],
//!                 500,
//!             );
//!             let result = ctx.sample(params).await?;
//!             Ok(CallToolResult::text(format!("{:?}", result.content)))
//!         })
//!         .build();
//!
//!     let router = McpRouter::new()
//!         .server_info("my-server", "1.0.0")
//!         .tool(tool);
//!
//!     let transport = WebSocketTransport::new(router).with_sampling();
//!     transport.serve("127.0.0.1:3000").await?;
//!     Ok(())
//! }
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::Response,
    routing::get,
};
use futures::{SinkExt, StreamExt};
use tokio::sync::{Mutex, RwLock, watch};

use crate::context::{
    ChannelClientRequester, ClientRequesterHandle, OutgoingRequest, OutgoingRequestReceiver,
    OutgoingRequestSender, outgoing_request_channel,
};
use crate::error::{Error, JsonRpcError, Result};
use crate::jsonrpc::JsonRpcService;
use crate::protocol::{
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, McpNotification,
    RequestId,
};
use crate::router::{McpRouter, RouterRequest, RouterResponse};
use crate::transport::service::{
    CatchError, InjectAnnotations, McpBoxService, ServiceFactory, identity_factory,
};
use crate::{ProtocolSupport, ProtocolSupportError};

/// Session state for WebSocket transport
struct Session {
    id: String,
    router: McpRouter,
    service_factory: ServiceFactory,
    /// Sender to signal the active connection to close (zombie prevention).
    /// Sending `true` tells the current connection to shut down.
    cancel_tx: Mutex<watch::Sender<bool>>,
}

impl Session {
    fn new(router: McpRouter, service_factory: ServiceFactory) -> Self {
        let (cancel_tx, _) = watch::channel(false);
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            router,
            service_factory,
            cancel_tx: Mutex::new(cancel_tx),
        }
    }

    /// Create a middleware-wrapped service from this session's router.
    fn make_service(&self) -> McpBoxService {
        (self.service_factory)(self.router.clone())
    }

    /// Get a receiver that will be notified when this connection should close.
    async fn cancel_receiver(&self) -> watch::Receiver<bool> {
        self.cancel_tx.lock().await.subscribe()
    }

    /// Signal the current active connection to close and create a fresh
    /// cancellation channel for the replacement connection.
    async fn replace_connection(&self) -> watch::Receiver<bool> {
        let mut tx = self.cancel_tx.lock().await;
        // Signal the old connection to shut down
        let _ = tx.send(true);
        // Replace with a fresh channel so new subscribers start clean
        let (new_tx, new_rx) = watch::channel(false);
        *tx = new_tx;
        new_rx
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("router", &self.router)
            .finish_non_exhaustive()
    }
}

/// Session store for WebSocket connections
#[derive(Debug, Default)]
struct SessionStore {
    sessions: RwLock<HashMap<String, Arc<Session>>>,
}

impl SessionStore {
    fn new() -> Self {
        Self::default()
    }

    async fn create(
        &self,
        router: McpRouter,
        service_factory: ServiceFactory,
    ) -> (Arc<Session>, watch::Receiver<bool>) {
        let session = Arc::new(Session::new(router, service_factory));
        let cancel_rx = session.cancel_receiver().await;
        let mut sessions = self.sessions.write().await;
        sessions.insert(session.id.clone(), session.clone());
        tracing::debug!(session_id = %session.id, "Created WebSocket session");
        (session, cancel_rx)
    }

    /// Look up an existing session by ID and replace its active connection.
    ///
    /// Signals the previous connection to close and returns a new cancellation
    /// receiver for the replacement connection.
    #[cfg_attr(not(test), allow(dead_code))]
    async fn reconnect(&self, id: &str) -> Option<(Arc<Session>, watch::Receiver<bool>)> {
        let sessions = self.sessions.read().await;
        let session = sessions.get(id)?;
        let cancel_rx = session.replace_connection().await;
        tracing::info!(session_id = %id, "Replaced active WebSocket connection (zombie prevention)");
        Some((session.clone(), cancel_rx))
    }

    async fn remove(&self, id: &str) -> bool {
        let mut sessions = self.sessions.write().await;
        let removed = sessions.remove(id).is_some();
        if removed {
            tracing::debug!(session_id = %id, "Removed WebSocket session");
        }
        removed
    }
}

/// Pending request waiting for a response
struct PendingRequest {
    response_tx: tokio::sync::oneshot::Sender<Result<serde_json::Value>>,
}

/// Shared state for WebSocket transport
struct AppState {
    router_template: McpRouter,
    /// Types copied from each upgrade request's extensions into the
    /// per-connection MCP extensions (#1242). Empty by default.
    extension_bridges: Vec<crate::transport::extension_bridge::ExtensionBridge>,
    service_factory: ServiceFactory,
    sessions: SessionStore,
    protocol_support: ProtocolSupport,
    /// Whether sampling is enabled
    sampling_enabled: bool,
}

/// WebSocket transport for MCP servers
///
/// Provides full-duplex communication over WebSocket.
///
/// WebSocket is a tower-mcp custom transport binding, not a standard
/// 2026-07-28 MCP transport. JSON-RPC request bodies remain the source of
/// truth for final per-request metadata. The optional `mcp.version.*`
/// subprotocol is an upgrade-time compatibility hint constrained by this
/// transport's exact [`ProtocolSupport`] allow-list.
///
/// Connection and cancellation semantics remain those of this custom binding:
/// a WebSocket close terminates the connection, and reconnecting a stored
/// session closes the older socket. It does not currently cancel an in-flight
/// handler. `notifications/cancelled` is processed between requests, so it
/// cannot interrupt a handler that is already executing. Final
/// `subscriptions/listen` multiplexing is not implemented on this binding.
pub struct WebSocketTransport {
    router: McpRouter,
    /// Types copied from each upgrade request's extensions into the
    /// per-connection MCP extensions (#1242). Empty by default.
    extension_bridges: Vec<crate::transport::extension_bridge::ExtensionBridge>,
    sampling_enabled: bool,
    service_factory: ServiceFactory,
    protocol_support: ProtocolSupport,
    #[cfg(feature = "oauth")]
    oauth_config: Option<crate::oauth::ProtectedResourceMetadata>,
}

impl WebSocketTransport {
    /// Copy `T` out of each upgrade request's extensions into the
    /// per-connection MCP extensions.
    ///
    /// See [`HttpTransport::bridge_extension`](crate::HttpTransport::bridge_extension).
    /// The value is read once, from the HTTP request that opens the socket,
    /// and is then visible to every request on that connection.
    pub fn bridge_extension<T>(mut self) -> Self
    where
        T: Clone + Send + Sync + 'static,
    {
        self.extension_bridges
            .push(crate::transport::extension_bridge::extension_bridge::<T>());
        self
    }

    /// Create a new WebSocket transport
    pub fn new(router: McpRouter) -> Self {
        Self {
            router,
            extension_bridges: Vec::new(),
            sampling_enabled: false,
            service_factory: identity_factory(),
            protocol_support: ProtocolSupport::default(),
            #[cfg(feature = "oauth")]
            oauth_config: None,
        }
    }

    /// Enable sampling support for this transport.
    ///
    /// When sampling is enabled, tool handlers can use `ctx.sample()` to
    /// request LLM completions from connected clients.
    pub fn with_sampling(mut self) -> Self {
        self.sampling_enabled = true;
        self
    }

    /// Set the exact protocol versions this custom binding accepts.
    pub fn protocol_support(mut self, support: ProtocolSupport) -> Self {
        self.protocol_support = support;
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
        self.protocol_support = ProtocolSupport::try_new(versions)?;
        Ok(self)
    }

    /// Configure OAuth 2.1 Protected Resource Metadata for this transport.
    ///
    /// When set, adds a `GET` endpoint at the resource's path-aware RFC 9728
    /// well-known location. This method only serves metadata; prefer
    /// [`Self::into_oauth_router`] for a complete protected setup.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use tower_mcp::oauth::ProtectedResourceMetadata;
    /// use tower_mcp::transport::websocket::WebSocketTransport;
    /// use tower_mcp::McpRouter;
    ///
    /// let metadata = ProtectedResourceMetadata::new("https://mcp.example.com")
    ///     .authorization_server("https://auth.example.com")
    ///     .scope("mcp:read");
    ///
    /// let router = McpRouter::new().server_info("my-server", "1.0.0");
    /// let transport = WebSocketTransport::new(router).oauth(metadata);
    /// ```
    #[cfg(feature = "oauth")]
    pub fn oauth(mut self, metadata: crate::oauth::ProtectedResourceMetadata) -> Self {
        self.oauth_config = Some(metadata);
        self
    }

    /// Build a fully protected OAuth WebSocket resource-server router.
    ///
    /// This validates and serves Protected Resource Metadata, authenticates
    /// WebSocket upgrades, enforces the canonical resource audience, and
    /// installs fail-closed per-operation scope checks.
    #[cfg(feature = "oauth")]
    pub fn into_oauth_router<V>(
        self,
        validator: V,
        metadata: crate::oauth::ProtectedResourceMetadata,
        policy: crate::oauth::ScopePolicy,
    ) -> std::result::Result<Router, crate::oauth::ProtectedResourceMetadataError>
    where
        V: crate::oauth::TokenValidator,
    {
        metadata.validate()?;
        let oauth_layer =
            crate::oauth::OAuthLayer::new(validator, metadata.clone()).scope_policy(policy.clone());
        let router = self
            .layer(crate::oauth::ScopeEnforcementLayer::new(policy))
            .oauth(metadata)
            .into_router();
        Ok(router.layer(oauth_layer))
    }

    /// Build a path-mounted, fully protected OAuth WebSocket router.
    #[cfg(feature = "oauth")]
    pub fn into_oauth_router_at<V>(
        self,
        path: &str,
        validator: V,
        metadata: crate::oauth::ProtectedResourceMetadata,
        policy: crate::oauth::ScopePolicy,
    ) -> std::result::Result<Router, crate::oauth::ProtectedResourceMetadataError>
    where
        V: crate::oauth::TokenValidator,
    {
        metadata.validate()?;
        let oauth_layer =
            crate::oauth::OAuthLayer::new(validator, metadata.clone()).scope_policy(policy.clone());
        let router = self
            .layer(crate::oauth::ScopeEnforcementLayer::new(policy))
            .oauth(metadata)
            .into_router_at(path);
        Ok(router.layer(oauth_layer))
    }

    /// Apply a tower middleware layer to MCP request processing.
    ///
    /// The layer is applied to the [`McpRouter`] service within each session,
    /// wrapping the `Service<RouterRequest>` pipeline. This allows middleware
    /// like timeouts, rate limiting, or custom instrumentation to be applied
    /// at the MCP request level.
    ///
    /// Middleware errors are automatically converted into JSON-RPC error
    /// responses, so the transport's error handling remains unchanged.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use std::time::Duration;
    /// use tower::ServiceBuilder;
    /// use tower::timeout::TimeoutLayer;
    /// use tower_mcp::McpRouter;
    /// use tower_mcp::transport::websocket::WebSocketTransport;
    ///
    /// let router = McpRouter::new().server_info("my-server", "1.0.0");
    /// let transport = WebSocketTransport::new(router)
    ///     .layer(
    ///         ServiceBuilder::new()
    ///             .layer(TimeoutLayer::new(Duration::from_secs(30)))
    ///             .concurrency_limit(10)
    ///             .into_inner(),
    ///     );
    /// ```
    pub fn layer<L>(mut self, layer: L) -> Self
    where
        L: tower::Layer<McpRouter> + Send + Sync + 'static,
        L::Service:
            tower::Service<RouterRequest, Response = RouterResponse> + Clone + Send + 'static,
        <L::Service as tower::Service<RouterRequest>>::Error: std::fmt::Display + Send,
        <L::Service as tower::Service<RouterRequest>>::Future: Send,
    {
        self.service_factory = Arc::new(move |router: McpRouter| {
            let annotations = router.tool_annotations_map();
            let wrapped = layer.layer(router);
            tower::util::BoxCloneService::new(InjectAnnotations::new(
                CatchError::new(wrapped),
                annotations,
            ))
        });
        self
    }

    /// Build the axum router for this transport
    pub fn into_router(self) -> Router {
        #[cfg(feature = "oauth")]
        let oauth_config = self.oauth_config;

        let state = Arc::new(AppState {
            router_template: self.router,
            extension_bridges: self.extension_bridges,
            service_factory: self.service_factory,
            sessions: SessionStore::new(),
            protocol_support: self.protocol_support,
            sampling_enabled: self.sampling_enabled,
        });

        let router = Router::new()
            .route("/", get(handle_websocket))
            .with_state(state);

        #[cfg(feature = "oauth")]
        let router = add_oauth_route(router, "", oauth_config.as_ref());

        router
    }

    /// Build an axum router mounted at a specific path
    pub fn into_router_at(self, path: &str) -> Router {
        #[cfg(feature = "oauth")]
        let oauth_config = self.oauth_config;

        let state = Arc::new(AppState {
            router_template: self.router,
            extension_bridges: self.extension_bridges,
            service_factory: self.service_factory,
            sessions: SessionStore::new(),
            protocol_support: self.protocol_support,
            sampling_enabled: self.sampling_enabled,
        });

        let ws_router = Router::new()
            .route("/", get(handle_websocket))
            .with_state(state);

        let router = Router::new().nest(path, ws_router);

        #[cfg(feature = "oauth")]
        let router = add_oauth_route(router, path, oauth_config.as_ref());

        router
    }

    /// Serve the transport on the given address, forever.
    ///
    /// This future never resolves on its own. Use
    /// [`serve_with_shutdown`](Self::serve_with_shutdown) in any process that
    /// has to stop the server without exiting.
    pub async fn serve(self, addr: &str) -> Result<()> {
        self.serve_with_shutdown(addr, std::future::pending::<()>())
            .await
    }

    /// Serve the transport on the given address until `signal` resolves.
    ///
    /// The signal has the same shape as
    /// `axum::serve(..).with_graceful_shutdown(..)`, because that is what it
    /// drives: once it resolves the listener stops accepting and this future
    /// returns. Binding still happens up front, so a bind error is reported
    /// before the signal is ever awaited.
    ///
    /// # Open sockets are not part of the shutdown
    ///
    /// This transport has no equivalent of
    /// [`HttpTransport::drain_timeout`](crate::HttpTransport::drain_timeout),
    /// because there is nothing here for a bound to cut short. A WebSocket
    /// leaves the connection axum is tracking the moment it is upgraded, and
    /// runs from then on in a task of its own. Shutting down therefore
    /// neither waits for open sockets nor closes them: it stops new clients
    /// getting in and hands control back, and the sockets already up live
    /// until their clients hang up or the process exits.
    ///
    /// A server that has to close them itself should keep its own record of
    /// live connections, as it would for any other broadcast.
    ///
    /// ```rust,no_run
    /// use tower_mcp::{McpRouter, WebSocketTransport};
    ///
    /// # async fn example() -> Result<(), tower_mcp::BoxError> {
    /// WebSocketTransport::new(McpRouter::new())
    ///     .serve_with_shutdown("127.0.0.1:3000", async {
    ///         tokio::signal::ctrl_c().await.ok();
    ///     })
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn serve_with_shutdown<F>(self, addr: &str, signal: F) -> Result<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| Error::Transport(format!("Failed to bind to {}: {}", addr, e)))?;

        tracing::info!("MCP WebSocket transport listening on {}", addr);

        let router = self.into_router();
        crate::transport::graceful::serve_with_shutdown(listener, router, signal, None).await
    }
}

/// Add the OAuth Protected Resource Metadata well-known route if configured.
#[cfg(feature = "oauth")]
fn add_oauth_route(
    router: Router,
    _base_path: &str,
    metadata: Option<&crate::oauth::ProtectedResourceMetadata>,
) -> Router {
    if let Some(metadata) = metadata {
        let metadata = metadata.clone();
        let well_known_path =
            crate::oauth::ProtectedResourceMetadata::well_known_path_for_resource(
                &metadata.resource,
            )
            .unwrap_or_else(|_| {
                crate::oauth::ProtectedResourceMetadata::well_known_path().to_string()
            });
        router.route(
            &well_known_path,
            get(move || {
                let m = metadata.clone();
                async move { axum::Json(m) }
            }),
        )
    } else {
        router
    }
}

/// Parsed MCP WebSocket subprotocols from `Sec-WebSocket-Protocol` header.
///
/// Per SEP-1288, clients send `mcp.auth.{token}` and `mcp.version.{version}`
/// as WebSocket subprotocols for authentication and version negotiation.
#[derive(Debug, Default)]
struct McpSubprotocols {
    /// Authentication token extracted from `mcp.auth.{token}` subprotocol.
    auth_token: Option<String>,
    /// Protocol version extracted from `mcp.version.{version}` subprotocol.
    protocol_version: Option<String>,
    /// All matched subprotocol strings to echo back in the upgrade response.
    selected: Vec<String>,
}

/// Parse MCP subprotocols from the `Sec-WebSocket-Protocol` header.
///
/// Returns the parsed subprotocols and the negotiated protocol version (if valid).
fn parse_mcp_subprotocols(
    headers: &axum::http::HeaderMap,
    protocol_support: &ProtocolSupport,
) -> McpSubprotocols {
    let mut result = McpSubprotocols::default();

    let Some(header) = headers.get("sec-websocket-protocol") else {
        return result;
    };
    let Ok(header_str) = header.to_str() else {
        return result;
    };

    for protocol in header_str.split(',').map(|s| s.trim()) {
        if let Some(token) = protocol.strip_prefix("mcp.auth.") {
            if !token.is_empty() {
                result.auth_token = Some(token.to_string());
                result.selected.push(protocol.to_string());
            }
        } else if let Some(version) = protocol.strip_prefix("mcp.version.") {
            if protocol_support.contains(version) {
                result.protocol_version = Some(version.to_string());
                result.selected.push(protocol.to_string());
            } else {
                tracing::warn!(version = %version, "Unsupported MCP protocol version in subprotocol");
            }
        }
    }

    result
}

/// Handle WebSocket upgrade.
///
/// Uses a raw `Request` extractor and performs the WebSocket upgrade manually
/// so we can access HTTP request extensions (e.g., `TokenClaims` from OAuth
/// middleware) and parse MCP subprotocols before upgrading.
async fn handle_websocket(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    use axum::extract::FromRequestParts;
    use axum::response::IntoResponse;

    let (mut parts, _body) = request.into_parts();

    // Parse MCP subprotocols (mcp.auth.*, mcp.version.*) from Sec-WebSocket-Protocol
    let subprotocols = parse_mcp_subprotocols(&parts.headers, &state.protocol_support);
    if let Some(ref version) = subprotocols.protocol_version {
        tracing::debug!(version = %version, "Client requested MCP protocol version via subprotocol");
    }

    // Bridge TokenClaims from HTTP extensions to MCP extensions
    #[allow(unused_mut)]
    let mut mcp_extensions = crate::router::Extensions::new();
    #[cfg(feature = "oauth")]
    {
        if let Some(claims) = parts.extensions.get::<crate::oauth::token::TokenClaims>() {
            mcp_extensions.insert(claims.clone());
        }
    }
    crate::transport::extension_bridge::apply_extension_bridges(
        &state.extension_bridges,
        &parts.extensions,
        &mut mcp_extensions,
    );

    // Store subprotocol auth token in extensions for downstream use
    if let Some(ref token) = subprotocols.auth_token {
        mcp_extensions.insert(WebSocketAuthToken(token.clone()));
    }
    if let Some(ref version) = subprotocols.protocol_version
        && let Ok(revision) = version.parse::<crate::inspection::McpProtocolRevision>()
    {
        mcp_extensions.insert(revision);
    }

    // Perform the WebSocket upgrade from request parts
    let ws: WebSocketUpgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(ws) => ws,
        Err(e) => return e.into_response(),
    };

    // Echo back the matched subprotocols in the upgrade response
    let ws = if !subprotocols.selected.is_empty() {
        ws.protocols(subprotocols.selected)
    } else {
        ws
    };

    ws.on_upgrade(move |socket| handle_socket(socket, state, mcp_extensions))
}

/// Auth token extracted from the `mcp.auth.{token}` WebSocket subprotocol.
///
/// This is inserted into the MCP extensions map and can be accessed by
/// middleware or tool handlers via `Extensions::get::<WebSocketAuthToken>()`.
#[derive(Debug, Clone)]
pub struct WebSocketAuthToken(pub String);

/// Handle an individual WebSocket connection
async fn handle_socket(
    socket: WebSocket,
    state: Arc<AppState>,
    mcp_extensions: crate::router::Extensions,
) {
    // Use with_fresh_session() to ensure each session has its own state
    let (session, cancel_rx) = state
        .sessions
        .create(
            state.router_template.with_fresh_session(),
            state.service_factory.clone(),
        )
        .await;
    let session_id = session.id.clone();
    let protocol_support = state.protocol_support.clone();

    tracing::info!(session_id = %session_id, "WebSocket connection established");

    if state.sampling_enabled {
        handle_socket_bidirectional(
            socket,
            session,
            &session_id,
            mcp_extensions,
            protocol_support,
            cancel_rx,
        )
        .await;
    } else {
        handle_socket_simple(
            socket,
            session,
            &session_id,
            mcp_extensions,
            protocol_support,
            cancel_rx,
        )
        .await;
    }

    // Cleanup session
    state.sessions.remove(&session_id).await;
    tracing::info!(session_id = %session_id, "WebSocket connection closed");
}

/// Handle WebSocket connection without sampling (simple mode)
async fn handle_socket_simple(
    socket: WebSocket,
    session: Arc<Session>,
    session_id: &str,
    mcp_extensions: crate::router::Extensions,
    protocol_support: ProtocolSupport,
    mut cancel_rx: watch::Receiver<bool>,
) {
    let mut service = JsonRpcService::new(session.make_service())
        .with_extensions(mcp_extensions)
        .protocol_support(protocol_support);
    let (mut sender, mut receiver) = socket.split();

    // Process incoming messages, also watching for cancellation (zombie prevention)
    loop {
        let msg = tokio::select! {
            msg = receiver.next() => {
                match msg {
                    Some(msg) => msg,
                    None => break,
                }
            }
            _ = cancel_rx.changed() => {
                if *cancel_rx.borrow() {
                    tracing::info!(session_id = %session_id, "Connection superseded by new connection, closing");
                    let _ = sender.send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: 1000,
                        reason: "Connection replaced by newer WebSocket connection".into(),
                    }))).await;
                    break;
                }
                continue;
            }
        };
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(error = %e, "WebSocket receive error");
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                match process_message(&mut service, &session.router, &text).await {
                    Ok(Some(response)) => {
                        let response_json = match serde_json::to_string(&response) {
                            Ok(json) => json,
                            Err(e) => {
                                tracing::error!(error = %e, "Failed to serialize response");
                                continue;
                            }
                        };

                        if let Err(e) = sender.send(Message::Text(response_json.into())).await {
                            tracing::error!(error = %e, "Failed to send response");
                            break;
                        }
                    }
                    Ok(None) => {
                        // Notification, no response needed
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Error processing message");
                        let error_response = JsonRpcResponse::error(
                            None,
                            JsonRpcError::internal_error(e.to_string()),
                        );
                        if let Ok(json) = serde_json::to_string(&error_response) {
                            let _ = sender.send(Message::Text(json.into())).await;
                        }
                    }
                }
            }
            Message::Binary(_) => {
                // MCP spec (SEP-1288) requires text frames only.
                // Binary frames MUST result in close code 1003 (Unsupported Data).
                tracing::warn!(session_id = %session_id, "Received binary frame, closing with 1003");
                let _ = sender
                    .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: 1003,
                        reason: "Binary frames are not supported by MCP".into(),
                    })))
                    .await;
                break;
            }
            Message::Ping(data) => {
                if let Err(e) = sender.send(Message::Pong(data)).await {
                    tracing::error!(error = %e, "Failed to send pong");
                    break;
                }
            }
            Message::Pong(_) => {
                // Ignore pongs
            }
            Message::Close(_) => {
                tracing::info!(session_id = %session_id, "WebSocket close received");
                break;
            }
        }
    }
}

/// Handle WebSocket connection with sampling support (bidirectional mode)
async fn handle_socket_bidirectional(
    socket: WebSocket,
    session: Arc<Session>,
    session_id: &str,
    _mcp_extensions: crate::router::Extensions,
    protocol_support: ProtocolSupport,
    mut cancel_rx: watch::Receiver<bool>,
) {
    // Create channels for outgoing requests
    let (request_tx, mut request_rx): (OutgoingRequestSender, OutgoingRequestReceiver) =
        outgoing_request_channel(32);

    // Create client requester for the router
    let client_requester: ClientRequesterHandle = Arc::new(ChannelClientRequester::new(request_tx));

    // Clone router and configure with client requester
    let router = session
        .router
        .clone()
        .with_client_requester(client_requester);
    let mut service = JsonRpcService::new((session.service_factory)(router.clone()))
        .with_extensions(_mcp_extensions)
        .protocol_support(protocol_support);

    // Track pending outgoing requests
    let pending_requests: Arc<Mutex<HashMap<RequestId, PendingRequest>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let (sender, mut receiver) = socket.split();
    let sender = Arc::new(Mutex::new(sender));

    let session_id_owned = session_id.to_string();

    loop {
        tokio::select! {
            // Handle incoming messages from client
            msg = receiver.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => {
                        tracing::error!(error = %e, "WebSocket receive error");
                        break;
                    }
                    None => break,
                };

                match msg {
                    Message::Text(text) => {
                        let result = handle_incoming_message(
                            &text,
                            &mut service,
                            &router,
                            pending_requests.clone(),
                            sender.clone(),
                        ).await;
                        if let Err(e) = result {
                            tracing::error!(error = %e, "Error handling incoming message");
                        }
                    }
                    Message::Binary(_) => {
                        // MCP spec (SEP-1288) requires text frames only.
                        // Binary frames MUST result in close code 1003 (Unsupported Data).
                        tracing::warn!(session_id = %session_id_owned, "Received binary frame, closing with 1003");
                        let mut s = sender.lock().await;
                        let _ = s.send(Message::Close(Some(axum::extract::ws::CloseFrame {
                            code: 1003,
                            reason: "Binary frames are not supported by MCP".into(),
                        }))).await;
                        break;
                    }
                    Message::Ping(data) => {
                        let mut sender = sender.lock().await;
                        if let Err(e) = sender.send(Message::Pong(data)).await {
                            tracing::error!(error = %e, "Failed to send pong");
                            break;
                        }
                    }
                    Message::Pong(_) => {}
                    Message::Close(_) => {
                        tracing::info!(session_id = %session_id_owned, "WebSocket close received");
                        break;
                    }
                }
            }

            // Handle outgoing requests to send to client
            Some(outgoing) = request_rx.recv() => {
                let result = send_outgoing_request(
                    outgoing,
                    pending_requests.clone(),
                    sender.clone(),
                ).await;
                if let Err(e) = result {
                    tracing::error!(error = %e, "Error sending outgoing request");
                }
            }

            // Handle cancellation (zombie prevention)
            _ = cancel_rx.changed() => {
                if *cancel_rx.borrow() {
                    tracing::info!(session_id = %session_id_owned, "Connection superseded by new connection, closing");
                    let mut s = sender.lock().await;
                    let _ = s.send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: 1000,
                        reason: "Connection replaced by newer WebSocket connection".into(),
                    }))).await;
                    break;
                }
            }
        }
    }
}

/// Handle an incoming WebSocket message (bidirectional mode)
async fn handle_incoming_message<S>(
    text: &str,
    service: &mut JsonRpcService<McpBoxService>,
    router: &McpRouter,
    pending_requests: Arc<Mutex<HashMap<RequestId, PendingRequest>>>,
    sender: Arc<Mutex<S>>,
) -> Result<()>
where
    S: futures::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let parsed: serde_json::Value = serde_json::from_str(text)?;

    // A notification cannot be answered, so a validation failure on one is
    // logged rather than sent back (#1272). Requests fall through.
    if !parsed.is_array()
        && parsed.get("id").is_none()
        && let Err(error) =
            service.inspect_incoming_value(&parsed, crate::inspection::McpDirection::ClientToServer)
    {
        tracing::debug!(
            method = parsed.get("method").and_then(|m| m.as_str()),
            %error,
            "rejected an invalid notification"
        );
        return Ok(());
    }

    if let Err(error) =
        service.inspect_incoming_value(&parsed, crate::inspection::McpDirection::ClientToServer)
    {
        let response = JsonRpcResponse::error(None, error);
        let response = serde_json::to_string(&response)
            .map_err(|error| Error::Transport(format!("Failed to serialize response: {error}")))?;
        sender
            .lock()
            .await
            .send(Message::Text(response.into()))
            .await
            .map_err(|error| Error::Transport(format!("Failed to send response: {error}")))?;
        return Ok(());
    }

    // Check if this is a response to one of our pending requests
    if parsed.get("method").is_none()
        && (parsed.get("result").is_some() || parsed.get("error").is_some())
    {
        return handle_response(&parsed, pending_requests).await;
    }

    // Check if it's a notification (no id field)
    if !parsed.is_array() && parsed.get("id").is_none() {
        if let Ok(notification) = serde_json::from_str::<JsonRpcNotification>(text) {
            let mcp_notification = McpNotification::from_jsonrpc(&notification)?;
            router.handle_notification(mcp_notification);
        }
        return Ok(());
    }

    // Process as a request
    let message: JsonRpcMessage = serde_json::from_str(text)?;
    match service.call_message(message).await {
        Ok(response) => {
            let response_json = serde_json::to_string(&response)
                .map_err(|e| Error::Transport(format!("Failed to serialize response: {}", e)))?;
            let mut sender = sender.lock().await;
            sender
                .send(Message::Text(response_json.into()))
                .await
                .map_err(|e| Error::Transport(format!("Failed to send response: {}", e)))?;
        }
        Err(e) => {
            tracing::error!(error = %e, "Error processing message");
            let error_response =
                JsonRpcResponse::error(None, JsonRpcError::internal_error(e.to_string()));
            if let Ok(json) = serde_json::to_string(&error_response) {
                let mut sender = sender.lock().await;
                let _ = sender.send(Message::Text(json.into())).await;
            }
        }
    }

    Ok(())
}

/// Handle a response to one of our pending requests
async fn handle_response(
    parsed: &serde_json::Value,
    pending_requests: Arc<Mutex<HashMap<RequestId, PendingRequest>>>,
) -> Result<()> {
    let id = match parsed.get("id") {
        Some(id) => {
            if let Some(n) = id.as_i64() {
                RequestId::Number(n)
            } else if let Some(s) = id.as_str() {
                RequestId::String(s.to_string())
            } else {
                tracing::warn!("Response has invalid id type");
                return Ok(());
            }
        }
        None => {
            tracing::warn!("Response missing id field");
            return Ok(());
        }
    };

    let pending = {
        let mut pending_requests = pending_requests.lock().await;
        pending_requests.remove(&id)
    };

    match pending {
        Some(pending) => {
            let result = if let Some(error) = parsed.get("error") {
                let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
                let message = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown error");
                Err(Error::Internal(format!(
                    "Client error ({}): {}",
                    code, message
                )))
            } else if let Some(result) = parsed.get("result") {
                Ok(result.clone())
            } else {
                Err(Error::Internal(
                    "Response has neither result nor error".to_string(),
                ))
            };

            // Send result to waiter (ignore if they've dropped the receiver)
            let _ = pending.response_tx.send(result);
        }
        None => {
            tracing::warn!(id = ?id, "Received response for unknown request");
        }
    }

    Ok(())
}

/// Send an outgoing request to the client
async fn send_outgoing_request<S>(
    outgoing: OutgoingRequest,
    pending_requests: Arc<Mutex<HashMap<RequestId, PendingRequest>>>,
    sender: Arc<Mutex<S>>,
) -> Result<()>
where
    S: futures::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    // Build JSON-RPC request
    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: outgoing.id.clone(),
        method: outgoing.method,
        params: Some(outgoing.params),
    };

    let request_json = serde_json::to_string(&request)
        .map_err(|e| Error::Transport(format!("Failed to serialize request: {}", e)))?;

    tracing::debug!(output = %request_json, "Sending request to client");

    // Store pending request
    {
        let mut pending = pending_requests.lock().await;
        pending.insert(
            outgoing.id,
            PendingRequest {
                response_tx: outgoing.response_tx,
            },
        );
    }

    // Send the request
    let mut sender = sender.lock().await;
    sender
        .send(Message::Text(request_json.into()))
        .await
        .map_err(|e| Error::Transport(format!("Failed to send request: {}", e)))?;

    Ok(())
}

/// Process a JSON-RPC message
async fn process_message(
    service: &mut JsonRpcService<McpBoxService>,
    router: &McpRouter,
    text: &str,
) -> Result<Option<crate::protocol::JsonRpcResponseMessage>> {
    let parsed: serde_json::Value = serde_json::from_str(text)?;

    // Classify before validating: a frame with no id has nowhere to put a
    // response, so answering one is a JSON-RPC violation regardless of what
    // validation says. Same fix as the stdio transport (#1272).
    if !parsed.is_array() && parsed.get("id").is_none() {
        let method = parsed.get("method").and_then(|m| m.as_str());
        if let Err(error) =
            service.inspect_incoming_value(&parsed, crate::inspection::McpDirection::ClientToServer)
        {
            tracing::debug!(method, %error, "rejected an invalid notification");
            return Ok(None);
        }
        match serde_json::from_str::<JsonRpcNotification>(text) {
            Ok(notification) => {
                let mcp_notification = McpNotification::from_jsonrpc(&notification)?;
                router.handle_notification(mcp_notification);
            }
            Err(_) => tracing::debug!(method, "unparseable notification"),
        }
        return Ok(None);
    }

    if let Err(error) =
        service.inspect_incoming_value(&parsed, crate::inspection::McpDirection::ClientToServer)
    {
        return Ok(Some(crate::protocol::JsonRpcResponseMessage::Single(
            JsonRpcResponse::error(None, error),
        )));
    }

    // Parse and process as a request. The serde error is not surfaced: for an
    // untagged enum it names the Rust type (#1272).
    let Ok(message) = serde_json::from_str::<JsonRpcMessage>(text) else {
        return Ok(Some(crate::protocol::JsonRpcResponseMessage::Single(
            crate::transport::stdio::parse_error_response("not a valid JSON-RPC request"),
        )));
    };
    let response = service.call_message(message).await?;
    Ok(Some(response))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_router() -> McpRouter {
        McpRouter::new().server_info("test-server", "1.0.0")
    }

    #[tokio::test]
    async fn test_websocket_transport_builds() {
        let transport = WebSocketTransport::new(create_test_router());
        let _router = transport.into_router();
    }

    #[tokio::test]
    async fn test_websocket_transport_at_path() {
        let transport = WebSocketTransport::new(create_test_router());
        let _router = transport.into_router_at("/mcp");
    }

    #[cfg(feature = "oauth")]
    #[tokio::test]
    async fn test_oauth_metadata_route_is_path_aware() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let metadata =
            crate::oauth::ProtectedResourceMetadata::new("https://mcp.example.com/tenant/ws")
                .authorization_server("https://auth.example.com");
        let app = WebSocketTransport::new(create_test_router())
            .oauth(metadata)
            .into_router_at("/tenant/ws");
        let request = Request::builder()
            .uri("/.well-known/oauth-protected-resource/tenant/ws")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_layer_with_identity() {
        // Verify that .layer() compiles and produces a working transport
        let transport = WebSocketTransport::new(create_test_router())
            .layer(tower::layer::util::Identity::new());
        let _router = transport.into_router();
    }

    #[tokio::test]
    async fn test_layer_with_timeout() {
        use std::time::Duration;
        use tower::timeout::TimeoutLayer;

        let transport = WebSocketTransport::new(create_test_router())
            .layer(TimeoutLayer::new(Duration::from_secs(30)));
        let _router = transport.into_router();
    }

    #[tokio::test]
    async fn test_layer_with_composed_layers() {
        use std::time::Duration;
        use tower::ServiceBuilder;
        use tower::timeout::TimeoutLayer;

        let transport = WebSocketTransport::new(create_test_router()).layer(
            ServiceBuilder::new()
                .layer(TimeoutLayer::new(Duration::from_secs(30)))
                .concurrency_limit(100)
                .into_inner(),
        );
        let _router = transport.into_router();
    }

    #[test]
    fn test_parse_mcp_subprotocols_empty() {
        let headers = axum::http::HeaderMap::new();
        let result = parse_mcp_subprotocols(&headers, &ProtocolSupport::default());
        assert!(result.auth_token.is_none());
        assert!(result.protocol_version.is_none());
        assert!(result.selected.is_empty());
    }

    #[test]
    fn test_parse_mcp_subprotocols_auth_and_version() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "sec-websocket-protocol",
            "mcp.auth.my-secret-token, mcp.version.2025-11-25"
                .parse()
                .unwrap(),
        );
        let result = parse_mcp_subprotocols(&headers, &ProtocolSupport::default());
        assert_eq!(result.auth_token.as_deref(), Some("my-secret-token"));
        assert_eq!(result.protocol_version.as_deref(), Some("2025-11-25"));
        assert_eq!(result.selected.len(), 2);
    }

    #[test]
    fn test_parse_mcp_subprotocols_unsupported_version() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "sec-websocket-protocol",
            "mcp.version.1999-01-01".parse().unwrap(),
        );
        let result = parse_mcp_subprotocols(&headers, &ProtocolSupport::default());
        assert!(result.protocol_version.is_none());
        assert!(result.selected.is_empty());
    }

    #[test]
    fn test_parse_mcp_subprotocols_older_supported_version() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "sec-websocket-protocol",
            "mcp.version.2025-03-26".parse().unwrap(),
        );
        let result = parse_mcp_subprotocols(&headers, &ProtocolSupport::default());
        assert_eq!(result.protocol_version.as_deref(), Some("2025-03-26"));
        assert_eq!(result.selected.len(), 1);
    }

    #[test]
    fn test_parse_mcp_subprotocols_auth_only() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "sec-websocket-protocol",
            "mcp.auth.bearer-xyz123".parse().unwrap(),
        );
        let result = parse_mcp_subprotocols(&headers, &ProtocolSupport::default());
        assert_eq!(result.auth_token.as_deref(), Some("bearer-xyz123"));
        assert!(result.protocol_version.is_none());
    }

    #[test]
    fn test_parse_mcp_subprotocols_ignores_unknown() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "sec-websocket-protocol",
            "graphql-ws, mcp.auth.token, mcp.version.2025-11-25, other-protocol"
                .parse()
                .unwrap(),
        );
        let result = parse_mcp_subprotocols(&headers, &ProtocolSupport::default());
        assert_eq!(result.auth_token.as_deref(), Some("token"));
        assert_eq!(result.protocol_version.as_deref(), Some("2025-11-25"));
        // Only MCP subprotocols are selected
        assert_eq!(result.selected.len(), 2);
    }

    #[cfg(feature = "stateless")]
    #[test]
    fn websocket_subprotocol_uses_exact_runtime_allow_list() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "sec-websocket-protocol",
            "mcp.version.2026-07-28, mcp.version.2025-11-25"
                .parse()
                .unwrap(),
        );

        let stable = parse_mcp_subprotocols(&headers, &ProtocolSupport::stable());
        assert_eq!(stable.protocol_version.as_deref(), Some("2025-11-25"));
        assert_eq!(stable.selected, vec!["mcp.version.2025-11-25"]);

        let final_only = ProtocolSupport::try_new(["2026-07-28"]).unwrap();
        let final_selected = parse_mcp_subprotocols(&headers, &final_only);
        assert_eq!(
            final_selected.protocol_version.as_deref(),
            Some("2026-07-28")
        );
        assert_eq!(final_selected.selected, vec!["mcp.version.2026-07-28"]);
    }

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn websocket_request_body_selects_final_lifecycle() {
        let router = create_test_router();
        let service = identity_factory()(router.clone());
        let mut service = JsonRpcService::new(service)
            .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap());
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "server/discover",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        });

        let response = process_message(&mut service, &router, &request.to_string())
            .await
            .unwrap()
            .expect("request response");
        let response = serde_json::to_value(response).unwrap();
        assert_eq!(response["result"]["resultType"], "complete");
        assert_eq!(response["result"]["ttlMs"], 0);
        assert_eq!(response["result"]["cacheScope"], "private");
        assert_eq!(response["result"]["supportedVersions"][0], "2026-07-28");
    }

    async fn websocket_batch_response(revision: &str) -> serde_json::Value {
        let router = create_test_router();
        let service = identity_factory()(router.clone());
        let mut service = JsonRpcService::new(service);
        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": revision,
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0"}
            }
        });
        process_message(&mut service, &router, &initialize.to_string())
            .await
            .unwrap();
        router.handle_notification(McpNotification::Initialized);

        let batch = serde_json::json!([
            {"jsonrpc": "2.0", "id": 1, "method": "ping"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}
        ]);
        let response = process_message(&mut service, &router, &batch.to_string())
            .await
            .unwrap()
            .expect("batch response");
        serde_json::to_value(response).unwrap()
    }

    #[tokio::test]
    async fn websocket_accepts_batch_for_2025_03() {
        let response = websocket_batch_response("2025-03-26").await;
        assert_eq!(response.as_array().map(Vec::len), Some(2));
    }

    #[tokio::test]
    async fn websocket_rejects_batch_for_2025_11() {
        let response = websocket_batch_response("2025-11-25").await;
        assert_eq!(response["error"]["code"], -32600);
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("does not permit top-level JSON-RPC batches")
        );
    }

    #[tokio::test]
    async fn test_session_cancel_receiver() {
        let router = create_test_router();
        let session = Session::new(router, identity_factory());
        let mut rx = session.cancel_receiver().await;

        // Should not be cancelled initially
        assert!(!*rx.borrow());

        // After replace_connection, old receiver should see cancellation
        let _new_rx = session.replace_connection().await;
        rx.changed().await.unwrap();
        assert!(*rx.borrow());
    }

    #[tokio::test]
    async fn test_session_replace_connection_new_rx_starts_clean() {
        let router = create_test_router();
        let session = Session::new(router, identity_factory());

        // First connection
        let _rx1 = session.cancel_receiver().await;

        // Replace: old connection cancelled, new starts clean
        let rx2 = session.replace_connection().await;
        assert!(!*rx2.borrow(), "New receiver should start as not-cancelled");
    }

    #[tokio::test]
    async fn test_session_store_reconnect() {
        let router = create_test_router();
        let store = SessionStore::new();

        let (session, mut rx1) = store
            .create(router.with_fresh_session(), identity_factory())
            .await;
        let session_id = session.id.clone();

        // Reconnect should cancel the first connection
        let result = store.reconnect(&session_id).await;
        assert!(result.is_some());
        let (_session2, rx2) = result.unwrap();

        // Old receiver should see cancellation
        rx1.changed().await.unwrap();
        assert!(*rx1.borrow());

        // New receiver should be clean
        assert!(!*rx2.borrow());
    }
}
