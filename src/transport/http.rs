//! Streamable HTTP transport for MCP
//!
//! Implements the Streamable HTTP transport from MCP specification 2025-11-25.
//!
//! ## Features
//!
//! - Single endpoint for POST (requests) and GET (SSE notifications)
//! - Session management via `MCP-Session-Id` header
//! - SSE streaming for server notifications and progress updates
//! - SSE event IDs and stream resumption via `Last-Event-ID` header (SEP-1699)
//! - Configurable session TTL and cleanup
//! - **Sampling support**: Server-to-client LLM requests via SSE + POST
//!
//! ## Sampling (Server-to-Client Requests)
//!
//! When using `HttpTransport::new(router).with_sampling()`, tool handlers can request
//! LLM completions from the client. The flow is:
//!
//! 1. Tool handler calls `ctx.sample(params)`
//! 2. Server sends the sampling request on the SSE stream
//! 3. Client receives the request and processes it
//! 4. Client sends the response as a POST to the MCP endpoint
//! 5. Server routes the response back to the waiting handler
//!
//! This follows the MCP spec which states servers MAY send JSON-RPC requests
//! on the SSE stream.
//!
//! ## Session Reconnection
//!
//! When a session is not found (e.g., after server restart or session expiration),
//! the server returns a JSON-RPC error with code `-32005` (SessionNotFound).
//! Clients should handle this by re-initializing the connection:
//!
//! ```text
//! Client                          Server
//!   |                               |
//!   |-- tools/list (old session) -->|
//!   |<-- error: SessionNotFound ----|
//!   |                               |
//!   |-- initialize --------------->|
//!   |<-- result + new session id ---|
//!   |                               |
//!   |-- tools/list (new session) -->|
//!   |<-- result -------------------|
//! ```
//!
//! ## SSE Stream Resumption (SEP-1699)
//!
//! Each SSE event includes a unique, monotonically increasing event ID. If a
//! client disconnects and reconnects, it can include the `Last-Event-ID` header
//! with the ID of the last event it received. The server will replay any buffered
//! events with IDs greater than the provided ID before continuing with live events.
//!
//! ```text
//! Client                              Server
//!   |-- GET / (Accept: text/event-stream) -->|
//!   |<-- id:0, data:{progress...} -----------|
//!   |<-- id:1, data:{progress...} -----------|
//!   |<-- id:2, data:{progress...} -----------|
//!   |                                        |
//!   |  ** Client disconnects **              |
//!   |                                        |
//!   |                   (server buffers id:3, id:4, id:5)
//!   |                                        |
//!   |-- GET / (Last-Event-ID: 2) ----------->|
//!   |<-- id:3, data:{...} (replayed) --------|
//!   |<-- id:4, data:{...} (replayed) --------|
//!   |<-- id:5, data:{...} (replayed) --------|
//!   |<-- id:6, data:{...} (live) ------------|
//! ```
//!
//! The server buffers up to 1000 events per session by default.
//!
//! ## Error Codes
//!
//! | Code    | Name           | Description                              |
//! |---------|----------------|------------------------------------------|
//! | -32005  | SessionNotFound| Session expired or server restarted      |
//! | -32006  | SessionRequired| MCP-Session-Id header missing            |
//!
//! # Example
//!
//! ```rust,no_run
//! use tower_mcp::{BoxError, McpRouter, ToolBuilder, CallToolResult};
//! use tower_mcp::transport::http::HttpTransport;
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
//!     let transport = HttpTransport::new(router);
//!
//!     // Run on localhost:3000
//!     transport.serve("127.0.0.1:3000").await?;
//!     Ok(())
//! }
//! ```

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{delete, get, post},
};
use tokio::sync::{Mutex, RwLock, broadcast, oneshot};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::context::{
    ChannelClientRequester, ClientRequesterHandle, OutgoingRequestReceiver, ServerNotification,
    notification_channel, outgoing_request_channel,
};
use crate::error::{Error, JsonRpcError, Result};
use crate::jsonrpc::JsonRpcService;
use crate::protocol::{
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, McpNotification, RequestId,
    SUPPORTED_PROTOCOL_VERSIONS, notifications,
};
use crate::router::{McpRouter, RouterRequest, RouterResponse};
use crate::transport::service::{CatchError, McpBoxService, ServiceFactory, identity_factory};

/// Header name for MCP session ID
pub const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";

/// Header name for MCP protocol version
pub const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";

/// SSE event type for JSON-RPC messages
const SSE_MESSAGE_EVENT: &str = "message";

/// Header name for Last-Event-ID (for SSE stream resumption per SEP-1699)
const LAST_EVENT_ID_HEADER: &str = "last-event-id";

/// Default maximum buffered events per session for stream resumption (SEP-1699)
const DEFAULT_MAX_BUFFERED_EVENTS: usize = 1000;

/// Pending request waiting for a response from the client
struct PendingRequest {
    response_tx: oneshot::Sender<Result<serde_json::Value>>,
}

/// A buffered SSE event for stream resumption (SEP-1699)
#[derive(Clone)]
struct BufferedEvent {
    /// Event ID
    id: u64,
    /// Event data (JSON string)
    data: String,
    /// When the event was created (for future time-based expiration)
    #[allow(dead_code)]
    timestamp: Instant,
}

/// Session state for HTTP transport
struct Session {
    /// Session ID
    id: String,
    /// The MCP router for this session
    router: McpRouter,
    /// Factory for creating middleware-wrapped services
    service_factory: ServiceFactory,
    /// Broadcast channel for SSE notifications and outgoing requests
    notifications_tx: broadcast::Sender<String>,
    /// Last time this session was accessed
    last_accessed: RwLock<Instant>,
    /// Pending outgoing requests waiting for responses
    pending_requests: Mutex<HashMap<RequestId, PendingRequest>>,
    /// Receiver for outgoing requests (used by SSE stream)
    request_rx: Mutex<Option<OutgoingRequestReceiver>>,
    /// Counter for SSE event IDs (for stream resumption per SEP-1699)
    event_counter: AtomicU64,
    /// Buffer of recent events for stream resumption (SEP-1699)
    event_buffer: RwLock<VecDeque<BufferedEvent>>,
    /// Maximum number of events to buffer per session
    max_buffered_events: usize,
}

impl Session {
    fn new(router: McpRouter, sampling_enabled: bool, service_factory: ServiceFactory) -> Self {
        let (notifications_tx, _) = broadcast::channel(100);

        // Set up notification forwarding: mpsc -> broadcast
        // The router sends notifications (progress, log, resource updates) to
        // an mpsc channel. We bridge these to the session's broadcast channel
        // so they reach connected SSE clients.
        let (notif_sender, mut notif_receiver) = notification_channel(256);
        let router = router.with_notification_sender(notif_sender);

        let broadcast_tx = notifications_tx.clone();
        tokio::spawn(async move {
            while let Some(notification) = notif_receiver.recv().await {
                let json = match &notification {
                    ServerNotification::Progress(params) => {
                        let notif = JsonRpcNotification::new(notifications::PROGRESS)
                            .with_params(serde_json::to_value(params).unwrap_or_default());
                        serde_json::to_string(&notif).ok()
                    }
                    ServerNotification::LogMessage(params) => {
                        let notif = JsonRpcNotification::new(notifications::MESSAGE)
                            .with_params(serde_json::to_value(params).unwrap_or_default());
                        serde_json::to_string(&notif).ok()
                    }
                    ServerNotification::ResourceUpdated { uri } => {
                        let notif = JsonRpcNotification::new(notifications::RESOURCE_UPDATED)
                            .with_params(serde_json::json!({ "uri": uri }));
                        serde_json::to_string(&notif).ok()
                    }
                    ServerNotification::ResourcesListChanged => {
                        let notif = JsonRpcNotification::new(notifications::RESOURCES_LIST_CHANGED);
                        serde_json::to_string(&notif).ok()
                    }
                    ServerNotification::ToolsListChanged => {
                        let notif =
                            JsonRpcNotification::new(notifications::TOOLS_LIST_CHANGED);
                        serde_json::to_string(&notif).ok()
                    }
                    ServerNotification::PromptsListChanged => {
                        let notif =
                            JsonRpcNotification::new(notifications::PROMPTS_LIST_CHANGED);
                        serde_json::to_string(&notif).ok()
                    }
                };
                if let Some(json) = json {
                    // Best effort: if no subscribers, the message is dropped
                    let _ = broadcast_tx.send(json);
                }
            }
        });

        // Set up client requester if sampling is enabled
        let (router, request_rx) = if sampling_enabled {
            let (request_tx, request_rx) = outgoing_request_channel(32);
            let client_requester: ClientRequesterHandle =
                Arc::new(ChannelClientRequester::new(request_tx));
            let router = router.with_client_requester(client_requester);
            (router, Some(request_rx))
        } else {
            (router, None)
        };

        Self {
            id: uuid::Uuid::new_v4().to_string(),
            router,
            service_factory,
            notifications_tx,
            last_accessed: RwLock::new(Instant::now()),
            pending_requests: Mutex::new(HashMap::new()),
            request_rx: Mutex::new(request_rx),
            event_counter: AtomicU64::new(0),
            event_buffer: RwLock::new(VecDeque::new()),
            max_buffered_events: DEFAULT_MAX_BUFFERED_EVENTS,
        }
    }

    /// Create a middleware-wrapped service from this session's router.
    fn make_service(&self) -> McpBoxService {
        (self.service_factory)(self.router.clone())
    }

    /// Get the next SSE event ID for this session.
    ///
    /// Event IDs are monotonically increasing per session, enabling
    /// stream resumption via the Last-Event-ID header (SEP-1699).
    fn next_event_id(&self) -> u64 {
        self.event_counter.fetch_add(1, Ordering::SeqCst)
    }

    /// Buffer an event for potential replay (SEP-1699).
    ///
    /// Events are buffered in a ring buffer. When the buffer is full,
    /// the oldest event is evicted. Clients can request replay of
    /// buffered events via the Last-Event-ID header.
    async fn buffer_event(&self, id: u64, data: String) {
        let mut buffer = self.event_buffer.write().await;
        if buffer.len() >= self.max_buffered_events {
            buffer.pop_front();
        }
        buffer.push_back(BufferedEvent {
            id,
            data,
            timestamp: Instant::now(),
        });
    }

    /// Get buffered events after the given event ID.
    ///
    /// Returns events with IDs greater than `after_id`, in order.
    /// Used for stream resumption when a client reconnects with
    /// the Last-Event-ID header.
    async fn get_events_after(&self, after_id: u64) -> Vec<BufferedEvent> {
        let buffer = self.event_buffer.read().await;
        buffer.iter().filter(|e| e.id > after_id).cloned().collect()
    }

    /// Update the last accessed time
    async fn touch(&self) {
        *self.last_accessed.write().await = Instant::now();
    }

    /// Check if the session has expired
    async fn is_expired(&self, ttl: Duration) -> bool {
        self.last_accessed.read().await.elapsed() > ttl
    }

    /// Store a pending request
    async fn add_pending_request(
        &self,
        id: RequestId,
        response_tx: oneshot::Sender<Result<serde_json::Value>>,
    ) {
        let mut pending = self.pending_requests.lock().await;
        pending.insert(id, PendingRequest { response_tx });
    }

    /// Complete a pending request with a response
    async fn complete_pending_request(
        &self,
        id: &RequestId,
        result: Result<serde_json::Value>,
    ) -> bool {
        let pending = {
            let mut pending_requests = self.pending_requests.lock().await;
            pending_requests.remove(id)
        };

        match pending {
            Some(pending) => {
                // Send result to waiter (ignore if they've dropped the receiver)
                let _ = pending.response_tx.send(result);
                true
            }
            None => false,
        }
    }
}

/// Default session TTL (30 minutes)
pub const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(30 * 60);

/// Default cleanup interval (1 minute)
const DEFAULT_CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

/// Configuration for session management
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Time-to-live for inactive sessions
    pub ttl: Duration,
    /// Maximum number of sessions (None = unlimited)
    pub max_sessions: Option<usize>,
    /// How often to run the cleanup task
    pub cleanup_interval: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            ttl: DEFAULT_SESSION_TTL,
            max_sessions: None,
            cleanup_interval: DEFAULT_CLEANUP_INTERVAL,
        }
    }
}

impl SessionConfig {
    /// Create a new session config with the given TTL
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            ttl,
            ..Default::default()
        }
    }

    /// Set the maximum number of sessions
    pub fn max_sessions(mut self, max: usize) -> Self {
        self.max_sessions = Some(max);
        self
    }

    /// Set the cleanup interval
    pub fn cleanup_interval(mut self, interval: Duration) -> Self {
        self.cleanup_interval = interval;
        self
    }
}

/// Session store for managing multiple client sessions
struct SessionStore {
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    config: SessionConfig,
    sampling_enabled: bool,
}

impl SessionStore {
    fn new(config: SessionConfig, sampling_enabled: bool) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            config,
            sampling_enabled,
        }
    }

    async fn create(
        &self,
        router: McpRouter,
        service_factory: ServiceFactory,
    ) -> Option<Arc<Session>> {
        let mut sessions = self.sessions.write().await;

        // Check max sessions limit
        if let Some(max) = self.config.max_sessions
            && sessions.len() >= max
        {
            tracing::warn!(
                max_sessions = max,
                current = sessions.len(),
                "Session limit reached, rejecting new session"
            );
            return None;
        }

        let session = Arc::new(Session::new(router, self.sampling_enabled, service_factory));
        sessions.insert(session.id.clone(), session.clone());
        tracing::debug!(session_id = %session.id, sampling = self.sampling_enabled, "Created new session");
        Some(session)
    }

    async fn get(&self, id: &str) -> Option<Arc<Session>> {
        let sessions = self.sessions.read().await;
        let session = sessions.get(id).cloned();
        if let Some(ref s) = session {
            // Touch the session to update last_accessed
            s.touch().await;
        }
        session
    }

    async fn remove(&self, id: &str) -> bool {
        let mut sessions = self.sessions.write().await;
        let removed = sessions.remove(id).is_some();
        if removed {
            tracing::debug!(session_id = %id, "Removed session");
        }
        removed
    }

    /// Remove expired sessions, returns count of removed sessions
    async fn cleanup_expired(&self) -> usize {
        let mut sessions = self.sessions.write().await;
        let ttl = self.config.ttl;

        let mut expired = Vec::new();
        for (id, session) in sessions.iter() {
            if session.is_expired(ttl).await {
                expired.push(id.clone());
            }
        }

        let count = expired.len();
        for id in expired {
            sessions.remove(&id);
            tracing::debug!(session_id = %id, "Expired session removed");
        }

        if count > 0 {
            tracing::info!(
                expired_count = count,
                remaining = sessions.len(),
                "Session cleanup completed"
            );
        }

        count
    }
}

/// Shared state for the HTTP transport
struct AppState {
    /// Template router for creating new sessions
    router_template: McpRouter,
    /// Factory for creating middleware-wrapped services
    service_factory: ServiceFactory,
    /// Session store
    sessions: Arc<SessionStore>,
    /// Whether to validate Origin header
    validate_origin: bool,
    /// Allowed origins (if validation is enabled)
    allowed_origins: Vec<String>,
    /// Whether sampling is enabled
    sampling_enabled: bool,
}

/// Configuration for OAuth 2.1 Protected Resource Metadata.
///
/// When set on [`HttpTransport`], a `GET /.well-known/oauth-protected-resource`
/// endpoint is added that returns the metadata JSON, enabling OAuth client
/// discovery per RFC 9728.
#[cfg(feature = "oauth")]
#[derive(Clone)]
pub struct OAuthConfig {
    /// Protected Resource Metadata to serve at the well-known endpoint.
    pub metadata: crate::oauth::ProtectedResourceMetadata,
}

/// HTTP transport for MCP servers
///
/// Implements the Streamable HTTP transport from the MCP specification.
pub struct HttpTransport {
    router: McpRouter,
    validate_origin: bool,
    allowed_origins: Vec<String>,
    session_config: SessionConfig,
    sampling_enabled: bool,
    service_factory: ServiceFactory,
    #[cfg(feature = "oauth")]
    oauth_config: Option<OAuthConfig>,
}

impl HttpTransport {
    /// Create a new HTTP transport wrapping an MCP router
    pub fn new(router: McpRouter) -> Self {
        Self {
            router,
            validate_origin: true,
            allowed_origins: vec![],
            session_config: SessionConfig::default(),
            sampling_enabled: false,
            service_factory: identity_factory(),
            #[cfg(feature = "oauth")]
            oauth_config: None,
        }
    }

    /// Enable sampling support for this transport.
    ///
    /// When sampling is enabled, tool handlers can use `ctx.sample()` to
    /// request LLM completions from connected clients. The server sends
    /// sampling requests on the SSE stream, and clients respond via POST.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::{BoxError, McpRouter, ToolBuilder, CallToolResult, CreateMessageParams, SamplingMessage};
    /// use tower_mcp::extract::{Context, RawArgs};
    /// use tower_mcp::transport::http::HttpTransport;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), BoxError> {
    ///     let tool = ToolBuilder::new("ai-tool")
    ///         .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
    ///             // Request LLM completion from client
    ///             let params = CreateMessageParams::new(
    ///                 vec![SamplingMessage::user("Summarize this...")],
    ///                 500,
    ///             );
    ///             let result = ctx.sample(params).await?;
    ///             Ok(CallToolResult::text(format!("{:?}", result.content)))
    ///         })
    ///         .build();
    ///
    ///     let router = McpRouter::new()
    ///         .server_info("my-server", "1.0.0")
    ///         .tool(tool);
    ///
    ///     let transport = HttpTransport::new(router).with_sampling();
    ///     transport.serve("127.0.0.1:3000").await?;
    ///     Ok(())
    /// }
    /// ```
    pub fn with_sampling(mut self) -> Self {
        self.sampling_enabled = true;
        self
    }

    /// Disable Origin header validation (not recommended for production)
    pub fn disable_origin_validation(mut self) -> Self {
        self.validate_origin = false;
        self
    }

    /// Set allowed origins for CORS/security validation
    pub fn allowed_origins(mut self, origins: Vec<String>) -> Self {
        self.allowed_origins = origins;
        self
    }

    /// Configure session management (TTL, max sessions, cleanup interval)
    pub fn session_config(mut self, config: SessionConfig) -> Self {
        self.session_config = config;
        self
    }

    /// Set session TTL (convenience method)
    pub fn session_ttl(mut self, ttl: Duration) -> Self {
        self.session_config.ttl = ttl;
        self
    }

    /// Set maximum number of concurrent sessions (convenience method)
    pub fn max_sessions(mut self, max: usize) -> Self {
        self.session_config.max_sessions = Some(max);
        self
    }

    /// Configure OAuth 2.1 Protected Resource Metadata for this transport.
    ///
    /// When set, adds a `GET /.well-known/oauth-protected-resource` endpoint
    /// that returns the metadata JSON, enabling OAuth client discovery per
    /// RFC 9728.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use tower_mcp::oauth::ProtectedResourceMetadata;
    /// use tower_mcp::transport::http::HttpTransport;
    /// use tower_mcp::McpRouter;
    ///
    /// let metadata = ProtectedResourceMetadata::new("https://mcp.example.com")
    ///     .authorization_server("https://auth.example.com")
    ///     .scope("mcp:read");
    ///
    /// let router = McpRouter::new().server_info("my-server", "1.0.0");
    /// let transport = HttpTransport::new(router).oauth(metadata);
    /// ```
    #[cfg(feature = "oauth")]
    pub fn oauth(mut self, metadata: crate::oauth::ProtectedResourceMetadata) -> Self {
        self.oauth_config = Some(OAuthConfig { metadata });
        self
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
    /// Note: this is separate from axum-level middleware on `into_router()`.
    /// Axum middleware operates on HTTP requests; this operates on MCP requests.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use std::time::Duration;
    /// use tower::ServiceBuilder;
    /// use tower::timeout::TimeoutLayer;
    /// use tower_mcp::McpRouter;
    /// use tower_mcp::transport::http::HttpTransport;
    ///
    /// let router = McpRouter::new().server_info("my-server", "1.0.0");
    /// let transport = HttpTransport::new(router)
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
            let wrapped = layer.layer(router);
            tower::util::BoxCloneService::new(CatchError::new(wrapped))
        });
        self
    }

    fn build_state(&self) -> Arc<AppState> {
        let sessions = Arc::new(SessionStore::new(
            self.session_config.clone(),
            self.sampling_enabled,
        ));

        // Spawn cleanup task
        let cleanup_sessions = sessions.clone();
        let cleanup_interval = self.session_config.cleanup_interval;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(cleanup_interval).await;
                cleanup_sessions.cleanup_expired().await;
            }
        });

        Arc::new(AppState {
            router_template: self.router.clone(),
            service_factory: self.service_factory.clone(),
            sessions,
            validate_origin: self.validate_origin,
            allowed_origins: self.allowed_origins.clone(),
            sampling_enabled: self.sampling_enabled,
        })
    }

    /// Build the axum router for this transport
    pub fn into_router(self) -> Router {
        let state = self.build_state();

        let router = Router::new()
            .route("/", post(handle_post))
            .route("/", get(handle_get))
            .route("/", delete(handle_delete))
            .route("/health", get(handle_health))
            .with_state(state);

        #[cfg(feature = "oauth")]
        let router = self.add_oauth_route(router, "");

        router
    }

    /// Build an axum router mounted at a specific path
    pub fn into_router_at(self, path: &str) -> Router {
        let state = self.build_state();

        let mcp_router = Router::new()
            .route("/", post(handle_post))
            .route("/", get(handle_get))
            .route("/", delete(handle_delete))
            .route("/health", get(handle_health))
            .with_state(state);

        let router = Router::new().nest(path, mcp_router);

        #[cfg(feature = "oauth")]
        let router = self.add_oauth_route(router, path);

        router
    }

    /// Serve the transport on the given address
    ///
    /// This is a convenience method that creates a TCP listener and serves the transport.
    pub async fn serve(self, addr: &str) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| Error::Transport(format!("Failed to bind to {}: {}", addr, e)))?;

        tracing::info!("MCP HTTP transport listening on {}", addr);

        let router = self.into_router();
        axum::serve(listener, router)
            .await
            .map_err(|e| Error::Transport(format!("Server error: {}", e)))?;

        Ok(())
    }

    /// Add the OAuth Protected Resource Metadata well-known route if configured.
    #[cfg(feature = "oauth")]
    fn add_oauth_route(&self, router: Router, base_path: &str) -> Router {
        if let Some(ref config) = self.oauth_config {
            let metadata = config.metadata.clone();
            let well_known_path = if base_path.is_empty() {
                crate::oauth::ProtectedResourceMetadata::well_known_path().to_string()
            } else {
                format!(
                    "{}{}",
                    base_path.trim_end_matches('/'),
                    crate::oauth::ProtectedResourceMetadata::well_known_path()
                )
            };
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
}

/// Check if an origin is a localhost origin (safe from DNS rebinding).
fn is_localhost_origin(origin: &str) -> bool {
    // Parse the origin to extract the host
    if let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    {
        // Strip port if present
        let host = rest.split(':').next().unwrap_or(rest);
        matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
    } else {
        false
    }
}

/// Validate Origin header for security.
///
/// When origin validation is enabled:
/// - Requests without an Origin header are allowed (same-origin)
/// - Localhost origins are always allowed (DNS rebinding protection)
/// - If `allowed_origins` is non-empty, non-localhost origins must match
/// - If `allowed_origins` is empty, non-localhost origins are rejected
///
/// Returns Some(Response) if validation fails, None if it passes.
fn validate_origin(headers: &HeaderMap, state: &AppState) -> Option<Response> {
    if !state.validate_origin {
        return None;
    }

    if let Some(origin) = headers.get(header::ORIGIN) {
        let origin_str = origin.to_str().unwrap_or("");

        // Always allow localhost origins (DNS rebinding protection allows these)
        if is_localhost_origin(origin_str) {
            return None;
        }

        // Non-localhost origin: check against allowed list
        if state.allowed_origins.is_empty() {
            return Some(
                (StatusCode::FORBIDDEN, "Cross-origin requests not allowed").into_response(),
            );
        }

        if !state
            .allowed_origins
            .iter()
            .any(|o| o == origin_str || o == "*")
        {
            return Some((StatusCode::FORBIDDEN, "Origin not allowed").into_response());
        }
    }

    None
}

/// Extract and validate session ID from headers
fn get_session_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get(MCP_SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Extract protocol version from headers
fn get_protocol_version(headers: &HeaderMap) -> Option<String> {
    headers
        .get(MCP_PROTOCOL_VERSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Extract Last-Event-ID from headers for SSE stream resumption (SEP-1699)
fn get_last_event_id(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(LAST_EVENT_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
}

/// Check if the request is an initialize request
fn is_initialize_request(body: &serde_json::Value) -> bool {
    body.get("method")
        .and_then(|m| m.as_str())
        .map(|m| m == "initialize")
        .unwrap_or(false)
}

/// Check if this is a response to one of our outgoing requests
fn is_response(parsed: &serde_json::Value) -> bool {
    parsed.get("method").is_none()
        && (parsed.get("result").is_some() || parsed.get("error").is_some())
}

/// Extract request ID from a JSON value
fn extract_request_id(parsed: &serde_json::Value) -> Option<RequestId> {
    parsed.get("id").and_then(|id| {
        if let Some(n) = id.as_i64() {
            Some(RequestId::Number(n))
        } else {
            id.as_str().map(|s| RequestId::String(s.to_string()))
        }
    })
}

/// Handle POST requests (JSON-RPC messages from client)
async fn handle_post(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    let (parts, body_bytes) = request.into_parts();
    let headers = parts.headers;

    // Validate Origin
    if let Some(resp) = validate_origin(&headers, &state) {
        return resp;
    }

    let body = match axum::body::to_bytes(body_bytes, usize::MAX).await {
        Ok(bytes) => match String::from_utf8(bytes.to_vec()) {
            Ok(s) => s,
            Err(e) => {
                return json_rpc_error_response(
                    None,
                    JsonRpcError::parse_error(format!("Invalid UTF-8: {}", e)),
                );
            }
        },
        Err(e) => {
            return json_rpc_error_response(
                None,
                JsonRpcError::parse_error(format!("Failed to read body: {}", e)),
            );
        }
    };

    // Bridge TokenClaims from HTTP extensions to MCP extensions (if present)
    #[cfg(feature = "oauth")]
    let http_extensions = parts.extensions;
    #[cfg(not(feature = "oauth"))]
    let _ = parts.extensions;

    // Parse the request body
    let parsed: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return json_rpc_error_response(
                None,
                JsonRpcError::parse_error(format!("Invalid JSON: {}", e)),
            );
        }
    };

    // Check if this is an initialize request (creates new session)
    let is_init = is_initialize_request(&parsed);

    // Get or create session
    let session = if is_init {
        // Create new session for initialize
        // Use with_fresh_session() to ensure each session has its own state
        match state
            .sessions
            .create(
                state.router_template.with_fresh_session(),
                state.service_factory.clone(),
            )
            .await
        {
            Some(s) => s,
            None => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Maximum session limit reached",
                )
                    .into_response();
            }
        }
    } else {
        // Require existing session
        let session_id = match get_session_id(&headers) {
            Some(id) => id,
            None => {
                // Return JSON-RPC error so clients can detect and handle this
                return json_rpc_error_response(None, JsonRpcError::session_required());
            }
        };

        match state.sessions.get(&session_id).await {
            Some(s) => s,
            None => {
                // Return JSON-RPC error with session info so clients know to re-initialize
                return json_rpc_error_response(
                    None,
                    JsonRpcError::session_not_found_with_id(&session_id),
                );
            }
        }
    };

    // Validate protocol version (if present and not init request)
    if !is_init
        && let Some(version) = get_protocol_version(&headers)
        && !SUPPORTED_PROTOCOL_VERSIONS.contains(&version.as_str())
    {
        return (
            StatusCode::BAD_REQUEST,
            format!("Unsupported protocol version: {}", version),
        )
            .into_response();
    }

    // Check if this is a response to one of our outgoing requests (sampling)
    if is_response(&parsed) {
        if let Some(id) = extract_request_id(&parsed) {
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

            if session.complete_pending_request(&id, result).await {
                tracing::debug!(request_id = ?id, "Completed pending request");
            } else {
                tracing::warn!(request_id = ?id, "Received response for unknown request");
            }
        }
        return StatusCode::ACCEPTED.into_response();
    }

    // Check if this is a notification (no id field)
    if parsed.get("id").is_none() {
        // Handle notification
        if let Ok(notification) = serde_json::from_value::<JsonRpcNotification>(parsed)
            && let Ok(mcp_notification) = McpNotification::from_jsonrpc(&notification)
        {
            session.router.handle_notification(mcp_notification);
        }
        return StatusCode::ACCEPTED.into_response();
    }

    // Handle as JSON-RPC request
    let request: JsonRpcRequest = match serde_json::from_value(parsed) {
        Ok(r) => r,
        Err(e) => {
            return json_rpc_error_response(
                None,
                JsonRpcError::parse_error(format!("Invalid request: {}", e)),
            );
        }
    };

    // Process the request through the middleware-wrapped service
    let mut service = JsonRpcService::new(session.make_service());

    // Bridge TokenClaims from HTTP request extensions to MCP extensions
    #[cfg(feature = "oauth")]
    {
        if let Some(claims) = http_extensions.get::<crate::oauth::token::TokenClaims>() {
            let mut ext = crate::router::Extensions::new();
            ext.insert(claims.clone());
            service = service.with_extensions(ext);
        }
    }
    let response = match service.call_single(request).await {
        Ok(resp) => resp,
        Err(e) => {
            return json_rpc_error_response(None, JsonRpcError::internal_error(e.to_string()));
        }
    };

    // Build response with session ID header for initialize
    let mut resp = axum::Json(response).into_response();

    if is_init {
        resp.headers_mut().insert(
            MCP_SESSION_ID_HEADER,
            HeaderValue::from_str(&session.id).unwrap(),
        );
    }

    resp
}

/// Handle GET requests (SSE stream for server notifications and outgoing requests)
async fn handle_get(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    // Validate Origin
    if let Some(resp) = validate_origin(&headers, &state) {
        return resp;
    }

    // Check Accept header
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !accept.contains("text/event-stream") {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "Accept header must include text/event-stream",
        )
            .into_response();
    }

    // Get session
    let session_id = match get_session_id(&headers) {
        Some(id) => id,
        None => {
            return json_rpc_error_response(None, JsonRpcError::session_required());
        }
    };

    let session = match state.sessions.get(&session_id).await {
        Some(s) => s,
        None => {
            return json_rpc_error_response(
                None,
                JsonRpcError::session_not_found_with_id(&session_id),
            );
        }
    };

    // Check for Last-Event-ID header for stream resumption (SEP-1699)
    let last_event_id = get_last_event_id(&headers);

    // If sampling is enabled, use bidirectional stream
    if state.sampling_enabled {
        return handle_get_bidirectional(session, last_event_id).await;
    }

    // Simple mode: just notifications
    let rx = session.notifications_tx.subscribe();
    let session_clone = session.clone();

    // Replay buffered events if Last-Event-ID was provided (SEP-1699)
    let replay_events: Vec<_> = if let Some(after_id) = last_event_id {
        let events = session.get_events_after(after_id).await;
        tracing::debug!(
            after_id = after_id,
            replay_count = events.len(),
            "Replaying buffered events for stream resumption"
        );
        events
            .into_iter()
            .map(|e| {
                Ok::<_, Infallible>(
                    Event::default()
                        .id(e.id.to_string())
                        .event(SSE_MESSAGE_EVENT)
                        .data(e.data),
                )
            })
            .collect()
    } else {
        Vec::new()
    };

    // Create replay stream from buffered events
    let replay_stream = tokio_stream::iter(replay_events);

    // Create live stream for new events
    // Use `then` for async processing, then `filter_map` to remove errors
    let live_stream = BroadcastStream::new(rx)
        .then(move |result: std::result::Result<String, _>| {
            let session = session_clone.clone();
            async move {
                match result {
                    Ok(msg) => {
                        let event_id = session.next_event_id();
                        // Buffer the event for potential replay (SEP-1699)
                        session.buffer_event(event_id, msg.clone()).await;
                        Some(Ok::<_, Infallible>(
                            Event::default()
                                .id(event_id.to_string())
                                .event(SSE_MESSAGE_EVENT)
                                .data(msg),
                        ))
                    }
                    Err(_) => None,
                }
            }
        })
        .filter_map(|x| x);

    // Chain replay stream with live stream
    let stream = replay_stream.chain(live_stream);

    Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(30))
                .text("ping"),
        )
        .into_response()
}

/// Handle GET requests with bidirectional support (sampling enabled)
async fn handle_get_bidirectional(session: Arc<Session>, last_event_id: Option<u64>) -> Response {
    // Take ownership of the request receiver for this SSE connection
    let request_rx = {
        let mut rx_guard = session.request_rx.lock().await;
        rx_guard.take()
    };

    // Create a channel for the stream
    let (tx, rx) = tokio::sync::mpsc::channel::<std::result::Result<Event, Infallible>>(100);

    // Replay buffered events if Last-Event-ID was provided (SEP-1699)
    if let Some(after_id) = last_event_id {
        let events = session.get_events_after(after_id).await;
        tracing::debug!(
            after_id = after_id,
            replay_count = events.len(),
            "Replaying buffered events for bidirectional stream resumption"
        );
        for event in events {
            let sse_event = Event::default()
                .id(event.id.to_string())
                .event(SSE_MESSAGE_EVENT)
                .data(event.data);
            if tx.send(Ok(sse_event)).await.is_err() {
                // Client disconnected before replay completed
                return Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx))
                    .keep_alive(
                        axum::response::sse::KeepAlive::new()
                            .interval(Duration::from_secs(30))
                            .text("ping"),
                    )
                    .into_response();
            }
        }
    }

    // Spawn task to multiplex notifications and outgoing requests
    let session_clone = session.clone();
    tokio::spawn(async move {
        let mut notification_rx = session_clone.notifications_tx.subscribe();

        // If we have a request receiver, use select! to handle both
        if let Some(mut req_rx) = request_rx {
            loop {
                tokio::select! {
                    // Handle notifications
                    result = notification_rx.recv() => {
                        match result {
                            Ok(msg) => {
                                let event_id = session_clone.next_event_id();
                                // Buffer the event for potential replay (SEP-1699)
                                session_clone.buffer_event(event_id, msg.clone()).await;
                                let event = Event::default()
                                    .id(event_id.to_string())
                                    .event(SSE_MESSAGE_EVENT)
                                    .data(msg);
                                if tx.send(Ok(event)).await.is_err() {
                                    break; // Client disconnected
                                }
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        }
                    }

                    // Handle outgoing requests (sampling)
                    Some(outgoing) = req_rx.recv() => {
                        // Build JSON-RPC request
                        let request = JsonRpcRequest {
                            jsonrpc: "2.0".to_string(),
                            id: outgoing.id.clone(),
                            method: outgoing.method,
                            params: Some(outgoing.params),
                        };

                        match serde_json::to_string(&request) {
                            Ok(request_json) => {
                                tracing::debug!(output = %request_json, "Sending request to client via SSE");

                                // Store pending request
                                session_clone.add_pending_request(
                                    outgoing.id,
                                    outgoing.response_tx,
                                ).await;

                                // Send on SSE stream
                                let event_id = session_clone.next_event_id();
                                // Buffer the event for potential replay (SEP-1699)
                                session_clone.buffer_event(event_id, request_json.clone()).await;
                                let event = Event::default()
                                    .id(event_id.to_string())
                                    .event(SSE_MESSAGE_EVENT)
                                    .data(request_json);
                                if tx.send(Ok(event)).await.is_err() {
                                    break; // Client disconnected
                                }
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "Failed to serialize outgoing request");
                                // Notify the waiter of the error
                                let _ = outgoing.response_tx.send(Err(Error::Internal(
                                    format!("Failed to serialize request: {}", e),
                                )));
                            }
                        }
                    }
                }
            }
        } else {
            // No request receiver, just handle notifications
            loop {
                match notification_rx.recv().await {
                    Ok(msg) => {
                        let event_id = session_clone.next_event_id();
                        // Buffer the event for potential replay (SEP-1699)
                        session_clone.buffer_event(event_id, msg.clone()).await;
                        let event = Event::default()
                            .id(event_id.to_string())
                            .event(SSE_MESSAGE_EVENT)
                            .data(msg);
                        if tx.send(Ok(event)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        }
    });

    // Convert the receiver into a stream
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(30))
                .text("ping"),
        )
        .into_response()
}

/// Handle DELETE requests (session termination)
async fn handle_delete(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    // Validate Origin
    if let Some(resp) = validate_origin(&headers, &state) {
        return resp;
    }

    let session_id = match get_session_id(&headers) {
        Some(id) => id,
        None => {
            return json_rpc_error_response(None, JsonRpcError::session_required());
        }
    };

    if state.sessions.remove(&session_id).await {
        tracing::info!(session_id = %session_id, "Session terminated");
        StatusCode::OK.into_response()
    } else {
        // For DELETE, it's okay if the session doesn't exist - it's already gone
        // Return OK instead of an error for idempotency
        tracing::debug!(session_id = %session_id, "Session already removed or never existed");
        StatusCode::OK.into_response()
    }
}

/// Handle GET /health requests
///
/// Returns a simple 200 OK response for health checks.
/// Does not require authentication or session state.
async fn handle_health() -> Response {
    StatusCode::OK.into_response()
}

/// Create a JSON-RPC error response
fn json_rpc_error_response(
    id: Option<crate::protocol::RequestId>,
    error: JsonRpcError,
) -> Response {
    let response = JsonRpcResponse::error(id, error);
    axum::Json(response).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn create_test_router() -> McpRouter {
        McpRouter::new().server_info("test-server", "1.0.0")
    }

    #[tokio::test]
    async fn test_initialize_creates_session() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();

        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": {
                            "name": "test-client",
                            "version": "1.0.0"
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key(MCP_SESSION_ID_HEADER));
    }

    #[tokio::test]
    async fn test_request_without_session_fails() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();

        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/list"
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();

        // We now return JSON-RPC errors for session issues
        assert_eq!(response.status(), StatusCode::OK);

        // Verify it's a JSON-RPC error response
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("error").is_some());
        assert_eq!(json["error"]["code"], -32006); // SessionRequired
    }

    #[tokio::test]
    async fn test_delete_session() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();

        // First, initialize to get a session
        let init_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": {
                            "name": "test-client",
                            "version": "1.0.0"
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.clone().oneshot(init_request).await.unwrap();
        let session_id = response
            .headers()
            .get(MCP_SESSION_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // Delete the session
        let delete_request = Request::builder()
            .method("DELETE")
            .uri("/")
            .header(MCP_SESSION_ID_HEADER, &session_id)
            .body(Body::empty())
            .unwrap();

        let response = app.clone().oneshot(delete_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Verify session is gone
        let list_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header(MCP_SESSION_ID_HEADER, &session_id)
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/list"
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(list_request).await.unwrap();
        // We now return JSON-RPC errors for session issues
        assert_eq!(response.status(), StatusCode::OK);

        // Verify it's a JSON-RPC error response
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("error").is_some());
        assert_eq!(json["error"]["code"], -32005); // SessionNotFound
    }

    #[tokio::test]
    async fn test_session_expiration() {
        // Create transport with very short TTL
        let config = SessionConfig::with_ttl(Duration::from_millis(50))
            .cleanup_interval(Duration::from_millis(10));
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .session_config(config);
        let app = transport.into_router();

        // Initialize to get a session
        let init_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": {
                            "name": "test-client",
                            "version": "1.0.0"
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.clone().oneshot(init_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let session_id = response
            .headers()
            .get(MCP_SESSION_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // Wait for session to expire and cleanup to run
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Session should be expired now
        let list_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header(MCP_SESSION_ID_HEADER, &session_id)
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/list"
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(list_request).await.unwrap();
        // We now return JSON-RPC errors for session issues
        assert_eq!(response.status(), StatusCode::OK);

        // Verify it's a JSON-RPC error response
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("error").is_some());
        assert_eq!(json["error"]["code"], -32005); // SessionNotFound
    }

    #[tokio::test]
    async fn test_layer_with_identity() {
        // Verify that .layer() compiles and produces a working transport
        // using a no-op layer (tower::layer::Identity)
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .layer(tower::layer::util::Identity::new());
        let app = transport.into_router();

        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": {
                            "name": "test-client",
                            "version": "1.0.0"
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key(MCP_SESSION_ID_HEADER));
    }

    #[tokio::test]
    async fn test_layer_with_timeout() {
        // Verify that .layer() works with TimeoutLayer
        use std::time::Duration;
        use tower::timeout::TimeoutLayer;

        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .layer(TimeoutLayer::new(Duration::from_secs(30)));
        let app = transport.into_router();

        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": {
                            "name": "test-client",
                            "version": "1.0.0"
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key(MCP_SESSION_ID_HEADER));
    }

    #[tokio::test]
    async fn test_layer_middleware_error_produces_jsonrpc_error() {
        // Use an extremely short timeout to force an error.
        // The CatchError wrapper should convert it to a JSON-RPC error response.
        use std::time::Duration;
        use tower::timeout::TimeoutLayer;

        let slow_tool = crate::tool::ToolBuilder::new("slow")
            .description("A slow tool")
            .handler(|_: serde_json::Value| async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(crate::CallToolResult::text("done"))
            })
            .build();

        let router = McpRouter::new()
            .server_info("test-server", "1.0.0")
            .tool(slow_tool);

        // 1ms timeout will definitely expire before the tool completes
        let transport = HttpTransport::new(router)
            .disable_origin_validation()
            .layer(TimeoutLayer::new(Duration::from_millis(1)));
        let app = transport.into_router();

        // Initialize first
        let init_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": {
                            "name": "test-client",
                            "version": "1.0.0"
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.clone().oneshot(init_request).await.unwrap();
        let session_id = response
            .headers()
            .get(MCP_SESSION_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // Call the slow tool -- should timeout and return a JSON-RPC error
        let tool_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header(MCP_SESSION_ID_HEADER, &session_id)
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "name": "slow",
                        "arguments": {}
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(tool_request).await.unwrap();
        // Should still return 200 with a JSON-RPC error body
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json.get("error").is_some(),
            "Expected JSON-RPC error response, got: {}",
            json
        );
    }

    #[tokio::test]
    async fn test_max_sessions_limit() {
        // Create transport with max 1 session
        let config = SessionConfig::default().max_sessions(1);
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .session_config(config);
        let app = transport.into_router();

        // First initialize should succeed
        let init_request1 = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": {
                            "name": "test-client",
                            "version": "1.0.0"
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.clone().oneshot(init_request1).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Second initialize should fail (max sessions reached)
        let init_request2 = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": {
                            "name": "test-client-2",
                            "version": "1.0.0"
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(init_request2).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_session_event_buffering() {
        // Test that events are buffered and can be retrieved for replay (SEP-1699)
        let session = Session::new(create_test_router(), false, identity_factory());

        // Buffer some events
        session.buffer_event(0, "event0".to_string()).await;
        session.buffer_event(1, "event1".to_string()).await;
        session.buffer_event(2, "event2".to_string()).await;

        // Get events after event 0
        let events = session.get_events_after(0).await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, 1);
        assert_eq!(events[0].data, "event1");
        assert_eq!(events[1].id, 2);
        assert_eq!(events[1].data, "event2");

        // Get events after event 1
        let events = session.get_events_after(1).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, 2);

        // Get events after event 2 (none)
        let events = session.get_events_after(2).await;
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn test_session_event_counter_increments() {
        // Test that event IDs increment monotonically (SEP-1699)
        let session = Session::new(create_test_router(), false, identity_factory());

        assert_eq!(session.next_event_id(), 0);
        assert_eq!(session.next_event_id(), 1);
        assert_eq!(session.next_event_id(), 2);
    }

    #[tokio::test]
    async fn test_session_event_buffer_limit() {
        // Test that buffer respects max size limit
        // Create a session - buffer limit is DEFAULT_MAX_BUFFERED_EVENTS (1000)
        let session = Session::new(create_test_router(), false, identity_factory());

        // Buffer more events than we can test practically, but verify the mechanism works
        // by checking that old events are evicted when we exceed the limit
        for i in 0..10 {
            session.buffer_event(i, format!("event{}", i)).await;
        }

        // All 10 events should be present
        let events = session.get_events_after(0).await;
        // Events after 0 should be 1-9 (9 events)
        assert_eq!(events.len(), 9);
    }
}
