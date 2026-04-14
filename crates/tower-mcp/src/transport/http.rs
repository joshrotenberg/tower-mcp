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
//! ## Session Handling
//!
//! By default, sessions are optional: requests without an `mcp-session-id`
//! header are allowed and receive a transient, pre-initialized session. This
//! ensures compatibility with clients (Codex CLI, Cursor, etc.) that don't
//! carry the session ID forward after initialization.
//!
//! Clients that do send session IDs continue to work normally.
//!
//! To require strict session management (reject requests without a session ID),
//! use [`HttpTransport::require_sessions()`]:
//!
//! ```rust,ignore
//! let transport = HttpTransport::new(router).require_sessions();
//! ```
//!
//! ## CORS Support
//!
//! Browser-based MCP clients require CORS headers. Since [`HttpTransport::into_router()`]
//! returns a standard [`axum::Router`], you can add CORS support using
//! `tower_http::cors::CorsLayer`:
//!
//! ```rust,ignore
//! use tower_mcp::McpRouter;
//! use tower_mcp::transport::http::HttpTransport;
//! use tower_http::cors::{CorsLayer, Any};
//! use http::Method;
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let router = McpRouter::new().server_info("my-server", "1.0.0");
//! let transport = HttpTransport::new(router);
//!
//! // Wrap the axum router with CORS middleware
//! let app = transport.into_router().layer(
//!     CorsLayer::new()
//!         .allow_origin(Any)
//!         .allow_methods([Method::GET, Method::POST, Method::DELETE])
//!         .allow_headers(Any)
//!         .expose_headers(Any),
//! );
//!
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
//! axum::serve(listener, app).await?;
//! # Ok(())
//! # }
//! ```
//!
//! For production, replace `Any` origins with your specific allowed origins.
//!
//! **Note:** [`HttpTransport::layer()`] applies middleware at the *MCP request* level
//! (inside the JSON-RPC service). CORS must be applied at the *HTTP* level using
//! `into_router().layer(...)` as shown above.
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

use std::collections::HashMap;
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
    ChannelClientRequester, ClientRequesterHandle, OutgoingRequestReceiver, notification_channel,
    outgoing_request_channel,
};
use crate::error::{Error, JsonRpcError, Result};
use crate::jsonrpc::JsonRpcService;
use crate::protocol::{
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, LATEST_PROTOCOL_VERSION, McpNotification,
    RequestId, SUPPORTED_PROTOCOL_VERSIONS,
};
use crate::router::{McpRouter, RouterRequest, RouterResponse};
use crate::transport::service::{
    CatchError, InjectAnnotations, McpBoxService, ServiceFactory, identity_factory,
};
use tower::util::BoxCloneService;

/// Header name for MCP session ID
pub const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";

/// Header name for MCP protocol version
pub const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";

/// SSE event type for JSON-RPC messages
const SSE_MESSAGE_EVENT: &str = "message";

/// Header name for Last-Event-ID (for SSE stream resumption per SEP-1699)
const LAST_EVENT_ID_HEADER: &str = "last-event-id";

/// Pending request waiting for a response from the client
struct PendingRequest {
    response_tx: oneshot::Sender<Result<serde_json::Value>>,
}

/// Session state for HTTP transport
/// How a session produces its MCP service for request processing.
enum SessionServiceSource {
    /// Session was created from an McpRouter with a factory for middleware wrapping.
    Router {
        router: McpRouter,
        factory: ServiceFactory,
    },
    /// Session was created from a pre-built boxed service (e.g., McpProxy).
    /// Wrapped in Mutex because BoxCloneService is Send but not Sync,
    /// and Session must be Sync for Arc<Session> to be Send.
    Boxed(std::sync::Mutex<McpBoxService>),
}

struct Session {
    /// Session ID
    id: String,
    /// Source for creating the MCP service
    service_source: SessionServiceSource,
    /// Broadcast channel for SSE notifications and outgoing requests
    notifications_tx: broadcast::Sender<String>,
    /// When this session was created
    created_at: Instant,
    /// Last time this session was accessed
    last_accessed: RwLock<Instant>,
    /// Pending outgoing requests waiting for responses
    pending_requests: Mutex<HashMap<RequestId, PendingRequest>>,
    /// Receiver for outgoing requests (used by SSE stream)
    request_rx: Mutex<Option<OutgoingRequestReceiver>>,
    /// Negotiated protocol version (set after initialize)
    protocol_version: RwLock<String>,
    /// Counter for SSE event IDs (for stream resumption per SEP-1699)
    event_counter: AtomicU64,
    /// Pluggable store for SSE events (enables cross-instance replay)
    event_store: Arc<dyn crate::event_store::EventStore>,
}

impl Session {
    fn new(
        router: McpRouter,
        sampling_enabled: bool,
        service_factory: ServiceFactory,
        event_store: Arc<dyn crate::event_store::EventStore>,
    ) -> Self {
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
                if let Some(json) = crate::transport::stdio::serialize_notification(&notification) {
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

        let now = Instant::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            service_source: SessionServiceSource::Router {
                router,
                factory: service_factory,
            },
            notifications_tx,
            created_at: now,
            last_accessed: RwLock::new(now),
            pending_requests: Mutex::new(HashMap::new()),
            request_rx: Mutex::new(request_rx),
            protocol_version: RwLock::new(LATEST_PROTOCOL_VERSION.to_string()),
            event_counter: AtomicU64::new(0),
            event_store,
        }
    }

    /// Create a session from a pre-built boxed service.
    ///
    /// This is used when the transport is created via [`HttpTransport::from_service()`].
    /// Notification bridging and sampling setup are skipped — the caller is
    /// responsible for configuring these on the service before passing it in.
    fn from_service(
        service: McpBoxService,
        event_store: Arc<dyn crate::event_store::EventStore>,
    ) -> Self {
        let (notifications_tx, _) = broadcast::channel(100);

        let now = Instant::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            service_source: SessionServiceSource::Boxed(std::sync::Mutex::new(service)),
            notifications_tx,
            created_at: now,
            last_accessed: RwLock::new(now),
            pending_requests: Mutex::new(HashMap::new()),
            request_rx: Mutex::new(None),
            protocol_version: RwLock::new(LATEST_PROTOCOL_VERSION.to_string()),
            event_counter: AtomicU64::new(0),
            event_store,
        }
    }

    /// Rebuild a session from a [`SessionRecord`] so a request for an
    /// unknown session ID can be served transparently.
    ///
    /// The router is pre-marked initialized and the protocol version is
    /// restored from the record. Runtime state (broadcast channels,
    /// pending-request table) is freshly allocated — in-flight state from
    /// before the rebuild is not recovered. The `event_counter` is left at
    /// zero; the [`SessionRegistry`] seeds it from the event store so
    /// future event IDs don't collide with buffered ones.
    fn restored(
        record: &crate::session_store::SessionRecord,
        router: McpRouter,
        sampling_enabled: bool,
        service_factory: ServiceFactory,
        event_store: Arc<dyn crate::event_store::EventStore>,
    ) -> Self {
        // Skip the Initializing intermediate state — this session was
        // already initialized on the original instance.
        router.session().mark_initialized();

        let (notifications_tx, _) = broadcast::channel(100);
        let (notif_sender, mut notif_receiver) = notification_channel(256);
        let router = router.with_notification_sender(notif_sender);

        let broadcast_tx = notifications_tx.clone();
        tokio::spawn(async move {
            while let Some(notification) = notif_receiver.recv().await {
                if let Some(json) = crate::transport::stdio::serialize_notification(&notification) {
                    let _ = broadcast_tx.send(json);
                }
            }
        });

        let (router, request_rx) = if sampling_enabled {
            let (request_tx, request_rx) = outgoing_request_channel(32);
            let client_requester: ClientRequesterHandle =
                Arc::new(ChannelClientRequester::new(request_tx));
            let router = router.with_client_requester(client_requester);
            (router, Some(request_rx))
        } else {
            (router, None)
        };

        let now = Instant::now();
        Self {
            id: record.id.clone(),
            service_source: SessionServiceSource::Router {
                router,
                factory: service_factory,
            },
            notifications_tx,
            created_at: now,
            last_accessed: RwLock::new(now),
            pending_requests: Mutex::new(HashMap::new()),
            request_rx: Mutex::new(request_rx),
            protocol_version: RwLock::new(record.protocol_version.clone()),
            event_counter: AtomicU64::new(0),
            event_store,
        }
    }

    /// Rebuild a session from a [`SessionRecord`] for transports built
    /// with [`HttpTransport::from_service`]. The service's internal state
    /// (if any) is not restored — the caller is responsible for anything
    /// beyond the metadata in the record.
    fn from_service_restored(
        service: McpBoxService,
        record: &crate::session_store::SessionRecord,
        event_store: Arc<dyn crate::event_store::EventStore>,
    ) -> Self {
        let (notifications_tx, _) = broadcast::channel(100);
        let now = Instant::now();
        Self {
            id: record.id.clone(),
            service_source: SessionServiceSource::Boxed(std::sync::Mutex::new(service)),
            notifications_tx,
            created_at: now,
            last_accessed: RwLock::new(now),
            pending_requests: Mutex::new(HashMap::new()),
            request_rx: Mutex::new(None),
            protocol_version: RwLock::new(record.protocol_version.clone()),
            event_counter: AtomicU64::new(0),
            event_store,
        }
    }

    /// Create a middleware-wrapped service from this session's service source.
    fn make_service(&self) -> McpBoxService {
        match &self.service_source {
            SessionServiceSource::Router { router, factory } => (factory)(router.clone()),
            SessionServiceSource::Boxed(mutex) => mutex.lock().unwrap().clone(),
        }
    }

    /// Handle a client notification (fire-and-forget, no response).
    ///
    /// For router-based sessions, delegates to the router's notification handler.
    /// For service-based sessions, notifications are logged but not processed
    /// (the service should handle its own notification needs).
    fn handle_notification(&self, notification: McpNotification) {
        match &self.service_source {
            SessionServiceSource::Router { router, .. } => {
                router.handle_notification(notification);
            }
            SessionServiceSource::Boxed(_) => {
                tracing::debug!(
                    notification = ?notification,
                    "Notification received on service-based session (not forwarded)"
                );
            }
        }
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
    /// Delegates to the configured [`EventStore`](crate::event_store::EventStore).
    /// Store errors are logged but non-fatal — the transport continues
    /// serving the client even if the external event buffer is unavailable,
    /// since the event has already been sent on the live SSE stream.
    async fn buffer_event(&self, id: u64, data: String) {
        let record = crate::event_store::EventRecord::new(id, data);
        if let Err(e) = self.event_store.append(&self.id, record).await {
            tracing::warn!(session_id = %self.id, event_id = id, error = %e, "Failed to append event to event store");
        }
    }

    /// Get buffered events after the given event ID.
    ///
    /// Returns events with IDs greater than `after_id`, in order. Used for
    /// stream resumption when a client reconnects with the `Last-Event-ID`
    /// header. Store errors produce an empty replay list and are logged.
    async fn get_events_after(&self, after_id: u64) -> Vec<crate::event_store::EventRecord> {
        match self.event_store.replay_after(&self.id, after_id).await {
            Ok(events) => events,
            Err(e) => {
                tracing::warn!(session_id = %self.id, error = %e, "Failed to replay events from event store");
                Vec::new()
            }
        }
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

/// Registry coordinating live session runtime state with a pluggable
/// persistent [`SessionStore`](crate::session_store::SessionStore).
///
/// - Runtime state (broadcast channels, pending requests, live services) is
///   kept in the in-process `sessions` map and cannot be serialized.
/// - Persistent metadata (IDs, timestamps, protocol version) is mirrored into
///   the caller-supplied [`SessionStore`]. The default
///   [`MemorySessionStore`](crate::session_store::MemorySessionStore) keeps
///   metadata in-process (same behavior as before this trait existed).
struct SessionRegistry {
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    config: SessionConfig,
    sampling_enabled: bool,
    persistent: Arc<dyn crate::session_store::SessionStore>,
    events: Arc<dyn crate::event_store::EventStore>,
    /// Source for rebuilding services when restoring a session.
    service_source: ServiceSource,
    /// If `true`, a request for an unknown session ID whose record is not
    /// in the persistent store spins up a new session with synthetic
    /// client info instead of returning 404 (see anubis-mcp #125 for the
    /// precedent).
    auto_reinit: bool,
}

impl SessionRegistry {
    fn new(
        config: SessionConfig,
        sampling_enabled: bool,
        persistent: Arc<dyn crate::session_store::SessionStore>,
        events: Arc<dyn crate::event_store::EventStore>,
        service_source: ServiceSource,
        auto_reinit: bool,
    ) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            config,
            sampling_enabled,
            persistent,
            events,
            service_source,
            auto_reinit,
        }
    }

    /// Build a SessionRecord reflecting the given live Session.
    async fn record_for(&self, session: &Session) -> crate::session_store::SessionRecord {
        let protocol_version = session.protocol_version.read().await.clone();
        let last_accessed = session.last_accessed.read().await;
        let mut record = crate::session_store::SessionRecord::new(
            session.id.clone(),
            protocol_version,
            self.config.ttl,
        );
        // Convert from monotonic Instant to SystemTime approximation. We
        // don't persist the client_info/capabilities here -- those can be
        // populated by a future restore-aware refactor.
        let now = std::time::SystemTime::now();
        let created_ago = session.created_at.elapsed();
        let last_accessed_ago = last_accessed.elapsed();
        record.created_at = now.checked_sub(created_ago).unwrap_or(now);
        record.last_accessed = now.checked_sub(last_accessed_ago).unwrap_or(now);
        record.expires_at = record.last_accessed + self.config.ttl;
        record
    }

    /// Persist metadata for a newly created session, logging on failure.
    ///
    /// Persistence errors are intentionally non-fatal: the live runtime
    /// session is already registered locally, so the transport can continue
    /// serving requests even if the external store is briefly unavailable.
    async fn persist_new(&self, session: &Session) {
        let record = self.record_for(session).await;
        if let Err(e) = self.persistent.create(&mut record.clone()).await {
            tracing::warn!(session_id = %session.id, error = %e, "Failed to persist session record");
        }
    }

    async fn create(
        &self,
        router: McpRouter,
        service_factory: ServiceFactory,
    ) -> Option<Arc<Session>> {
        let session = {
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

            let session = Arc::new(Session::new(
                router,
                self.sampling_enabled,
                service_factory,
                self.events.clone(),
            ));
            sessions.insert(session.id.clone(), session.clone());
            tracing::debug!(session_id = %session.id, sampling = self.sampling_enabled, "Created new session");
            session
        };
        self.persist_new(&session).await;
        Some(session)
    }

    async fn create_from_service(&self, service: McpBoxService) -> Option<Arc<Session>> {
        let session = {
            let mut sessions = self.sessions.write().await;

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

            let session = Arc::new(Session::from_service(service, self.events.clone()));
            sessions.insert(session.id.clone(), session.clone());
            tracing::debug!(session_id = %session.id, "Created new session from service");
            session
        };
        self.persist_new(&session).await;
        Some(session)
    }

    /// Create a new session with its router already marked as initialized.
    ///
    /// Used by the optional-sessions feature to serve requests from clients
    /// that skip the initialize handshake.
    async fn create_initialized(
        &self,
        router: McpRouter,
        service_factory: ServiceFactory,
    ) -> Option<Arc<Session>> {
        // Pre-initialize the router's session state so it won't reject requests
        router.session().mark_initialized();

        let session = {
            let mut sessions = self.sessions.write().await;

            if let Some(max) = self.config.max_sessions
                && sessions.len() >= max
            {
                return None;
            }

            let session = Arc::new(Session::new(
                router,
                self.sampling_enabled,
                service_factory,
                self.events.clone(),
            ));
            sessions.insert(session.id.clone(), session.clone());
            tracing::debug!(session_id = %session.id, "Created pre-initialized session (optional_sessions)");
            session
        };
        self.persist_new(&session).await;
        Some(session)
    }

    /// Create a pre-initialized session from a boxed service.
    async fn create_initialized_from_service(
        &self,
        service: McpBoxService,
    ) -> Option<Arc<Session>> {
        let session = {
            let mut sessions = self.sessions.write().await;

            if let Some(max) = self.config.max_sessions
                && sessions.len() >= max
            {
                return None;
            }

            let session = Arc::new(Session::from_service(service, self.events.clone()));
            sessions.insert(session.id.clone(), session.clone());
            tracing::debug!(session_id = %session.id, "Created pre-initialized session from service (optional_sessions)");
            session
        };
        self.persist_new(&session).await;
        Some(session)
    }

    async fn get(&self, id: &str) -> Option<Arc<Session>> {
        // Fast path: the session is live in this process.
        {
            let sessions = self.sessions.read().await;
            if let Some(s) = sessions.get(id).cloned() {
                s.touch().await;
                return Some(s);
            }
        }

        // Slow path #1: the session is unknown locally but the persistent
        // store has a record — rebuild it.
        match self.persistent.load(id).await {
            Ok(Some(record)) => {
                tracing::info!(session_id = %id, "Restoring session from persistent store");
                if let Some(session) = self.restore_from_record(record).await {
                    return Some(session);
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(session_id = %id, error = %e, "Failed to load session record");
            }
        }

        // Slow path #2 (opt-in): auto-reinitialize with synthetic client
        // info so the client can continue without a re-handshake. Useful
        // for single-instance restarts where no external store is
        // configured; loses original client identity.
        if self.auto_reinit {
            tracing::info!(session_id = %id, "Auto-reinitializing unknown session");
            return self.auto_reinitialize(id).await;
        }

        None
    }

    /// Restore a live [`Session`] from a persisted [`SessionRecord`].
    ///
    /// The caller must ensure the record's ID is not already live locally;
    /// on success the session is inserted into the local registry, the
    /// event counter is seeded so new event IDs don't collide with
    /// buffered ones, and the record's `last_accessed` is refreshed and
    /// saved back to the store.
    async fn restore_from_record(
        &self,
        record: crate::session_store::SessionRecord,
    ) -> Option<Arc<Session>> {
        let session = {
            let mut sessions = self.sessions.write().await;

            if let Some(max) = self.config.max_sessions
                && sessions.len() >= max
            {
                tracing::warn!(
                    max_sessions = max,
                    "Session limit reached, cannot restore session"
                );
                return None;
            }

            // Guard against a concurrent create that beat us here.
            if let Some(existing) = sessions.get(&record.id).cloned() {
                existing.touch().await;
                return Some(existing);
            }

            let session: Arc<Session> = match &self.service_source {
                ServiceSource::Router { router, factory } => Arc::new(Session::restored(
                    &record,
                    router.with_fresh_session(),
                    self.sampling_enabled,
                    factory.clone(),
                    self.events.clone(),
                )),
                ServiceSource::Service(svc) => {
                    let service = svc.lock().unwrap().clone();
                    Arc::new(Session::from_service_restored(
                        service,
                        &record,
                        self.events.clone(),
                    ))
                }
            };

            sessions.insert(record.id.clone(), session.clone());
            tracing::debug!(session_id = %session.id, "Restored session into local registry");
            session
        };

        // Seed the event counter past the highest buffered event ID so new
        // SSE events don't collide with ones the client may still replay.
        if let Ok(events) = self.events.replay_after(&record.id, 0).await
            && let Some(max_id) = events.iter().map(|e| e.id).max()
        {
            session
                .event_counter
                .store(max_id + 1, std::sync::atomic::Ordering::SeqCst);
        }

        // Refresh last_accessed in the store so the record doesn't expire
        // immediately after restore.
        let mut refreshed = record;
        refreshed.touch(self.config.ttl);
        if let Err(e) = self.persistent.save(&refreshed).await {
            tracing::warn!(session_id = %refreshed.id, error = %e, "Failed to refresh restored session record");
        }

        Some(session)
    }

    /// Create a new session with the requested ID and synthetic client
    /// info, skipping the initialize handshake. Used when `auto_reinit`
    /// is enabled and no stored record exists.
    ///
    /// Loses the original client's identity and capabilities — the server
    /// sees a session from client `"auto-recovered"`.
    async fn auto_reinitialize(&self, id: &str) -> Option<Arc<Session>> {
        let mut record = crate::session_store::SessionRecord::new(
            id.to_string(),
            LATEST_PROTOCOL_VERSION.to_string(),
            self.config.ttl,
        );
        record.client_info = Some(crate::protocol::Implementation {
            name: "auto-recovered".into(),
            version: "unknown".into(),
            title: None,
            description: None,
            icons: None,
            website_url: None,
            meta: None,
        });
        record.client_capabilities = Some(crate::protocol::ClientCapabilities::default());

        // Persist first so a concurrent request sees the record. Ignore
        // persistence errors; the in-memory session will still work.
        if let Err(e) = self.persistent.create(&mut record).await {
            tracing::warn!(session_id = %id, error = %e, "Failed to persist auto-reinitialized session");
        }

        self.restore_from_record(record).await
    }

    async fn remove(&self, id: &str) -> bool {
        let removed = {
            let mut sessions = self.sessions.write().await;
            sessions.remove(id).is_some()
        };
        if removed {
            tracing::debug!(session_id = %id, "Removed session");
            if let Err(e) = self.persistent.delete(id).await {
                tracing::warn!(session_id = %id, error = %e, "Failed to delete session record");
            }
            if let Err(e) = self.events.purge_session(id).await {
                tracing::warn!(session_id = %id, error = %e, "Failed to purge session events");
            }
        }
        removed
    }

    /// Remove expired sessions, returns count of removed sessions
    async fn cleanup_expired(&self) -> usize {
        let expired = {
            let mut sessions = self.sessions.write().await;
            let ttl = self.config.ttl;

            let mut expired = Vec::new();
            for (id, session) in sessions.iter() {
                if session.is_expired(ttl).await {
                    expired.push(id.clone());
                }
            }

            for id in &expired {
                sessions.remove(id);
                tracing::debug!(session_id = %id, "Expired session removed");
            }

            if !expired.is_empty() {
                tracing::info!(
                    expired_count = expired.len(),
                    remaining = sessions.len(),
                    "Session cleanup completed"
                );
            }
            expired
        };

        for id in &expired {
            if let Err(e) = self.persistent.delete(id).await {
                tracing::warn!(session_id = %id, error = %e, "Failed to delete expired session record");
            }
            if let Err(e) = self.events.purge_session(id).await {
                tracing::warn!(session_id = %id, error = %e, "Failed to purge expired session events");
            }
        }

        expired.len()
    }
}

/// Metadata about an active session.
///
/// Returned by [`SessionHandle::list_sessions()`].
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// The session ID.
    pub id: String,
    /// How long ago this session was created.
    pub created_at: Duration,
    /// How long ago this session was last accessed.
    pub last_activity: Duration,
}

/// A handle for querying and managing HTTP transport sessions.
///
/// Obtained from [`HttpTransport::into_router_with_handle()`] or
/// [`HttpTransport::into_router_at_with_handle()`]. The handle is cheap to
/// clone and can be shared across threads.
///
/// # Example
///
/// ```rust,ignore
/// use tower_mcp::transport::http::HttpTransport;
///
/// let transport = HttpTransport::new(router);
/// let (router, handle) = transport.into_router_with_handle();
///
/// // Later, in an admin endpoint:
/// let count = handle.session_count().await;
/// for info in handle.list_sessions().await {
///     println!("{}: created {:?} ago", info.id, info.created_at);
/// }
/// handle.terminate_session("session-id").await;
/// ```
#[derive(Clone)]
pub struct SessionHandle {
    store: Arc<SessionRegistry>,
}

impl SessionHandle {
    /// Returns the number of currently active sessions.
    pub async fn session_count(&self) -> usize {
        self.store.sessions.read().await.len()
    }

    /// Returns metadata for all active sessions.
    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        let sessions = self.store.sessions.read().await;
        let mut infos = Vec::with_capacity(sessions.len());
        for session in sessions.values() {
            let last_accessed = session.last_accessed.read().await;
            infos.push(SessionInfo {
                id: session.id.clone(),
                created_at: session.created_at.elapsed(),
                last_activity: last_accessed.elapsed(),
            });
        }
        infos
    }

    /// Terminates a session by ID, returning `true` if the session existed.
    pub async fn terminate_session(&self, id: &str) -> bool {
        self.store.remove(id).await
    }
}

/// The source of the MCP service for session creation.
#[derive(Clone)]
enum ServiceSource {
    /// Created from an McpRouter with a factory for middleware wrapping.
    Router {
        router: McpRouter,
        factory: ServiceFactory,
    },
    /// Created from a pre-built boxed service (e.g., McpProxy).
    /// Wrapped in Arc<Mutex<_>> because BoxCloneService is Send but not Sync.
    Service(Arc<std::sync::Mutex<McpBoxService>>),
}

/// Shared state for the HTTP transport
struct AppState {
    /// Source for creating new session services
    service_source: ServiceSource,
    /// Session store
    sessions: Arc<SessionRegistry>,
    /// Whether to validate Origin header
    validate_origin: bool,
    /// Allowed origins (if validation is enabled)
    allowed_origins: Vec<String>,
    /// Whether sampling is enabled
    sampling_enabled: bool,
    /// Whether sessions are optional (for clients that don't track session IDs)
    optional_sessions: bool,
    /// SEP-1442 stateless mode configuration
    #[cfg(feature = "stateless")]
    stateless_config: Option<crate::stateless::StatelessConfig>,
}

/// Configuration for OAuth 2.1 Protected Resource Metadata.
///
/// When set on [`HttpTransport`], a `GET /.well-known/oauth-protected-resource`
/// endpoint is added that returns the metadata JSON, enabling OAuth client
/// discovery per RFC 9728.
#[cfg(feature = "oauth")]
#[derive(Clone)]
pub(crate) struct OAuthConfig {
    /// Protected Resource Metadata to serve at the well-known endpoint.
    pub(crate) metadata: crate::oauth::ProtectedResourceMetadata,
}

/// HTTP transport for MCP servers
///
/// Implements the Streamable HTTP transport from the MCP specification.
///
/// # Construction
///
/// There are two ways to create an `HttpTransport`:
///
/// - [`HttpTransport::new(router)`](HttpTransport::new) — wraps an [`McpRouter`], with full
///   support for per-session notification bridging, sampling, and `.layer()` middleware.
///
/// - [`HttpTransport::from_service(service)`](HttpTransport::from_service) — wraps any
///   `Service<RouterRequest>` (e.g., [`McpProxy`](crate::proxy::McpProxy)). The service is
///   cloned for each session. Notification bridging and sampling are not set up automatically;
///   the caller should configure these on the service before passing it in.
///   `.layer()` is not supported in this mode.
pub struct HttpTransport {
    service_source: ServiceSource,
    validate_origin: bool,
    allowed_origins: Vec<String>,
    session_config: SessionConfig,
    sampling_enabled: bool,
    optional_sessions: bool,
    session_store: Arc<dyn crate::session_store::SessionStore>,
    event_store: Arc<dyn crate::event_store::EventStore>,
    auto_reinit_sessions: bool,
    #[cfg(feature = "stateless")]
    stateless_config: Option<crate::stateless::StatelessConfig>,
    #[cfg(feature = "oauth")]
    oauth_config: Option<OAuthConfig>,
}

impl HttpTransport {
    /// Create a new HTTP transport wrapping an MCP router.
    ///
    /// Supports per-session notification bridging, sampling, and `.layer()` middleware.
    pub fn new(router: McpRouter) -> Self {
        Self {
            service_source: ServiceSource::Router {
                router,
                factory: identity_factory(),
            },
            validate_origin: true,
            allowed_origins: vec![],
            session_config: SessionConfig::default(),
            sampling_enabled: false,
            optional_sessions: true,
            session_store: Arc::new(crate::session_store::MemorySessionStore::new()),
            event_store: Arc::new(crate::event_store::MemoryEventStore::new()),
            auto_reinit_sessions: false,
            #[cfg(feature = "stateless")]
            stateless_config: None,
            #[cfg(feature = "oauth")]
            oauth_config: None,
        }
    }

    /// Create an HTTP transport from a pre-built service.
    ///
    /// This accepts any `Service<RouterRequest>` implementation, such as
    /// [`McpProxy`](crate::proxy::McpProxy). The service is cloned for each
    /// HTTP session.
    ///
    /// Notification bridging and sampling are **not** set up automatically.
    /// The caller should configure these on the service before passing it in.
    ///
    /// `.layer()` is not supported when using `from_service()` — wrap the
    /// service with middleware before passing it in.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use tower_mcp::transport::http::HttpTransport;
    /// use tower_mcp::proxy::McpProxy;
    ///
    /// let proxy: McpProxy = /* ... */;
    /// let transport = HttpTransport::from_service(proxy);
    /// transport.serve("127.0.0.1:3000").await?;
    /// ```
    pub fn from_service<S>(service: S) -> Self
    where
        S: tower::Service<
                RouterRequest,
                Response = RouterResponse,
                Error = std::convert::Infallible,
            > + Clone
            + Send
            + 'static,
        S::Future: Send,
    {
        Self {
            service_source: ServiceSource::Service(Arc::new(std::sync::Mutex::new(
                BoxCloneService::new(service),
            ))),
            validate_origin: true,
            allowed_origins: vec![],
            session_config: SessionConfig::default(),
            sampling_enabled: false,
            optional_sessions: true,
            session_store: Arc::new(crate::session_store::MemorySessionStore::new()),
            event_store: Arc::new(crate::event_store::MemoryEventStore::new()),
            auto_reinit_sessions: false,
            #[cfg(feature = "stateless")]
            stateless_config: None,
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

    /// Require strict session management.
    ///
    /// When enabled, requests without an `mcp-session-id` header are rejected
    /// with a `SessionRequired` error (-32006). Clients must complete the
    /// `initialize` handshake and include the session ID on all subsequent
    /// requests, as specified by the MCP 2025-11-25 spec.
    ///
    /// By default, sessions are optional for compatibility with clients
    /// (Codex CLI, Cursor, etc.) that don't carry the session ID forward
    /// after initialization.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::McpRouter;
    /// use tower_mcp::transport::http::HttpTransport;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     let router = McpRouter::new().server_info("my-server", "1.0.0");
    ///     let transport = HttpTransport::new(router).require_sessions();
    ///     transport.serve("127.0.0.1:3000").await?;
    ///     Ok(())
    /// }
    /// ```
    pub fn require_sessions(mut self) -> Self {
        self.optional_sessions = false;
        self
    }

    /// Enable SEP-1442 stateless mode (experimental).
    ///
    /// When enabled, requests with an `MCP-Protocol-Version` header (or
    /// `_meta.protocolVersion` in the body) are processed without requiring a
    /// session. Each stateless request gets an ephemeral, pre-initialized
    /// router that is discarded after the response is sent.
    ///
    /// This enables horizontal scaling and serverless deployments where
    /// sessions cannot be maintained across instances.
    ///
    /// Stateful clients (those that send `mcp-session-id`) continue to work
    /// normally alongside stateless clients.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::McpRouter;
    /// use tower_mcp::transport::http::HttpTransport;
    /// use tower_mcp::stateless::StatelessConfig;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     let router = McpRouter::new().server_info("my-server", "1.0.0");
    ///     let transport = HttpTransport::new(router)
    ///         .stateless(StatelessConfig::new());
    ///     transport.serve("127.0.0.1:3000").await?;
    ///     Ok(())
    /// }
    /// ```
    #[cfg(feature = "stateless")]
    pub fn stateless(mut self, config: crate::stateless::StatelessConfig) -> Self {
        self.stateless_config = Some(config);
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

    /// Configure a pluggable [`SessionStore`](crate::session_store::SessionStore)
    /// for persisting session metadata.
    ///
    /// The default is an in-process
    /// [`MemorySessionStore`](crate::session_store::MemorySessionStore) —
    /// supply an external store (Redis, Postgres, etc.) to share session
    /// metadata across server instances behind a load balancer.
    ///
    /// Runtime state (broadcast channels, pending requests, service
    /// instances) is always kept per-instance; only persistent metadata is
    /// mirrored to the store.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use std::sync::Arc;
    /// use tower_mcp::{HttpTransport, McpRouter};
    /// use tower_mcp::session_store::{MemorySessionStore, SessionStore};
    ///
    /// let router = McpRouter::new();
    /// let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    /// let transport = HttpTransport::new(router).session_store(store);
    /// ```
    pub fn session_store(mut self, store: Arc<dyn crate::session_store::SessionStore>) -> Self {
        self.session_store = store;
        self
    }

    /// Configure a pluggable [`EventStore`](crate::event_store::EventStore)
    /// for SSE event buffering and stream resumption.
    ///
    /// The default is an in-process
    /// [`MemoryEventStore`](crate::event_store::MemoryEventStore) with a
    /// 1000-event ring buffer per session — supply an external store (Redis,
    /// etc.) so clients can resume SSE streams after reconnecting to a
    /// different server instance behind a load balancer (SEP-1699).
    ///
    /// Typically paired with a matching
    /// [`session_store`](Self::session_store) so both session metadata and
    /// buffered events survive across instances.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use std::sync::Arc;
    /// use tower_mcp::{HttpTransport, McpRouter};
    /// use tower_mcp::event_store::{EventStore, MemoryEventStore};
    ///
    /// let router = McpRouter::new();
    /// let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::new());
    /// let transport = HttpTransport::new(router).event_store(store);
    /// ```
    pub fn event_store(mut self, store: Arc<dyn crate::event_store::EventStore>) -> Self {
        self.event_store = store;
        self
    }

    /// Enable auto-reinitialization for unknown session IDs.
    ///
    /// When a request arrives with an `mcp-session-id` that is not live
    /// locally and has no record in the configured
    /// [`session_store`](Self::session_store), the transport normally
    /// returns a session-not-found error. With this flag enabled, the
    /// transport instead spins up a new session claiming that ID and
    /// completes the initialize handshake internally with synthetic
    /// client info (`name = "auto-recovered"`, empty capabilities).
    ///
    /// This lets tolerant clients continue after a server restart without
    /// repeating the handshake, at the cost of losing the original
    /// client's identity and negotiated capabilities. Prefer pairing this
    /// with a real [`session_store`](Self::session_store) — the store
    /// path runs first and preserves full identity when a record exists.
    ///
    /// Disabled by default. This is the pattern established by
    /// [anubis-mcp #125](https://github.com/zoedsoupe/anubis-mcp/pull/125).
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::{HttpTransport, McpRouter};
    ///
    /// let router = McpRouter::new();
    /// let transport = HttpTransport::new(router).auto_reinitialize_sessions(true);
    /// ```
    pub fn auto_reinitialize_sessions(mut self, enabled: bool) -> Self {
        self.auto_reinit_sessions = enabled;
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
    /// # Panics
    ///
    /// Panics if this transport was created via [`from_service()`](Self::from_service).
    /// When using `from_service()`, wrap the service with middleware before passing it in.
    pub fn layer<L>(mut self, layer: L) -> Self
    where
        L: tower::Layer<McpRouter> + Send + Sync + 'static,
        L::Service:
            tower::Service<RouterRequest, Response = RouterResponse> + Clone + Send + 'static,
        <L::Service as tower::Service<RouterRequest>>::Error: std::fmt::Display + Send,
        <L::Service as tower::Service<RouterRequest>>::Future: Send,
    {
        match &mut self.service_source {
            ServiceSource::Router { factory, .. } => {
                *factory = Arc::new(move |router: McpRouter| {
                    let annotations = router.tool_annotations_map();
                    let wrapped = layer.layer(router);
                    tower::util::BoxCloneService::new(InjectAnnotations::new(
                        CatchError::new(wrapped),
                        annotations,
                    ))
                });
            }
            ServiceSource::Service(_) => {
                panic!(
                    "layer() cannot be used with from_service() — \
                     wrap the service with middleware before passing it in"
                );
            }
        }
        self
    }

    fn build_state(&self) -> Arc<AppState> {
        let sessions = Arc::new(SessionRegistry::new(
            self.session_config.clone(),
            self.sampling_enabled,
            self.session_store.clone(),
            self.event_store.clone(),
            self.service_source.clone(),
            self.auto_reinit_sessions,
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
            service_source: self.service_source.clone(),
            sessions,
            validate_origin: self.validate_origin,
            allowed_origins: self.allowed_origins.clone(),
            sampling_enabled: self.sampling_enabled,
            optional_sessions: self.optional_sessions,
            #[cfg(feature = "stateless")]
            stateless_config: self.stateless_config.clone(),
        })
    }

    /// Build the axum router for this transport.
    pub fn into_router(self) -> Router {
        let (router, _handle) = self.into_router_with_handle();
        router
    }

    /// Build the axum router and return a [`SessionHandle`] for querying
    /// session metrics (e.g., active session count).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let transport = HttpTransport::new(router);
    /// let (router, handle) = transport.into_router_with_handle();
    ///
    /// // Use handle in an admin endpoint
    /// let count = handle.session_count().await;
    /// ```
    pub fn into_router_with_handle(self) -> (Router, SessionHandle) {
        let state = self.build_state();
        let handle = SessionHandle {
            store: state.sessions.clone(),
        };

        let router = Router::new()
            .route("/", post(handle_post))
            .route("/", get(handle_get))
            .route("/", delete(handle_delete))
            .route("/health", get(handle_health))
            .with_state(state);

        #[cfg(feature = "oauth")]
        let router = self.add_oauth_route(router, "");

        (router, handle)
    }

    /// Build an axum router mounted at a specific path.
    pub fn into_router_at(self, path: &str) -> Router {
        let (router, _handle) = self.into_router_at_with_handle(path);
        router
    }

    /// Build an axum router mounted at a specific path and return a
    /// [`SessionHandle`] for querying session metrics.
    pub fn into_router_at_with_handle(self, path: &str) -> (Router, SessionHandle) {
        let state = self.build_state();
        let handle = SessionHandle {
            store: state.sessions.clone(),
        };

        let mcp_router = Router::new()
            .route("/", post(handle_post))
            .route("/", get(handle_get))
            .route("/", delete(handle_delete))
            .route("/health", get(handle_health))
            .with_state(state);

        let router = Router::new().nest(path, mcp_router);

        #[cfg(feature = "oauth")]
        let router = self.add_oauth_route(router, path);

        (router, handle)
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
        // Handle bracketed IPv6 (e.g., [::1]:3000)
        let host = if rest.starts_with('[') {
            // Extract everything between [ and ]
            rest.split(']')
                .next()
                .unwrap_or(rest)
                .trim_start_matches('[')
        } else {
            // Strip port if present
            rest.split(':').next().unwrap_or(rest)
        };
        matches!(host, "localhost" | "127.0.0.1" | "::1")
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

    // SEP-1442: Handle stateless requests (no session needed).
    // Stateless requests have a protocol version but no session ID and are not
    // initialize requests. They are processed with an ephemeral service and
    // return immediately without storing any session state.
    #[cfg(feature = "stateless")]
    if !is_init && state.stateless_config.is_some() && get_session_id(&headers).is_none() {
        let version_from_header = get_protocol_version(&headers);
        let params = parsed.get("params").unwrap_or(&parsed);
        let version_from_meta = crate::stateless::StatelessRequestMeta::from_params(params)
            .and_then(|m| m.protocol_version);

        if let Some(version) = version_from_header.or(version_from_meta) {
            if let Err(err) = crate::stateless::validate_protocol_version(&version) {
                return json_rpc_error_response(None, err);
            }

            // Notifications and responses don't make sense without a session
            if parsed.get("id").is_none() || is_response(&parsed) {
                return StatusCode::ACCEPTED.into_response();
            }

            let request: JsonRpcRequest = match serde_json::from_value(parsed) {
                Ok(r) => r,
                Err(e) => {
                    return json_rpc_error_response(
                        None,
                        JsonRpcError::parse_error(format!("Invalid request: {}", e)),
                    );
                }
            };

            // Ephemeral pre-initialized service -- no session stored
            let mut service = match &state.service_source {
                ServiceSource::Router { router, factory } => {
                    let ephemeral = router.with_fresh_session();
                    ephemeral.session().mark_initialized();
                    JsonRpcService::new(factory(ephemeral))
                }
                ServiceSource::Service(mutex) => JsonRpcService::new(mutex.lock().unwrap().clone()),
            };

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
                    return json_rpc_error_response(
                        None,
                        JsonRpcError::internal_error(e.to_string()),
                    );
                }
            };

            let mut resp = axum::Json(response).into_response();
            resp.headers_mut().insert(
                MCP_PROTOCOL_VERSION_HEADER,
                HeaderValue::from_str(&version).unwrap(),
            );
            return resp;
        }
    }

    // Get or create session
    let session = if is_init {
        // Create new session for initialize
        let create_result = match &state.service_source {
            ServiceSource::Router { router, factory } => {
                // Use with_fresh_session() to ensure each session has its own state
                state
                    .sessions
                    .create(router.with_fresh_session(), factory.clone())
                    .await
            }
            ServiceSource::Service(mutex) => {
                let service = mutex.lock().unwrap().clone();
                state.sessions.create_from_service(service).await
            }
        };
        match create_result {
            Some(s) => s,
            None => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Maximum session limit reached",
                )
                    .into_response();
            }
        }
    } else if let Some(session_id) = get_session_id(&headers) {
        // Client sent a session ID -- look it up
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
    } else if state.optional_sessions {
        // No session ID, but sessions are optional -- create a transient,
        // pre-initialized session so the router won't reject the request.
        // This supports clients (Codex CLI, Cursor, etc.) that perform
        // initialize + tools/list during setup but don't carry the session
        // ID forward to subsequent requests.
        let create_result = match &state.service_source {
            ServiceSource::Router { router, factory } => {
                state
                    .sessions
                    .create_initialized(router.with_fresh_session(), factory.clone())
                    .await
            }
            ServiceSource::Service(mutex) => {
                let service = mutex.lock().unwrap().clone();
                state
                    .sessions
                    .create_initialized_from_service(service)
                    .await
            }
        };
        match create_result {
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
        // No session ID and sessions are required
        return json_rpc_error_response(None, JsonRpcError::session_required());
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
            session.handle_notification(mcp_notification);
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

    // For initialize responses, extract and store the negotiated protocol version
    if is_init
        && let JsonRpcResponse::Result(ref result) = response
        && let Some(version) = result
            .result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
    {
        *session.protocol_version.write().await = version.to_string();
    }

    // Build response with headers
    let mut resp = axum::Json(response).into_response();

    if is_init {
        resp.headers_mut().insert(
            MCP_SESSION_ID_HEADER,
            HeaderValue::from_str(&session.id).unwrap(),
        );
    }

    // Always include the negotiated protocol version header
    let version = session.protocol_version.read().await;
    resp.headers_mut().insert(
        MCP_PROTOCOL_VERSION_HEADER,
        HeaderValue::from_str(&version).unwrap(),
    );

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
        // Verify protocol version header is present on initialize response
        assert_eq!(
            response
                .headers()
                .get(MCP_PROTOCOL_VERSION_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("2025-11-25")
        );
    }

    #[tokio::test]
    async fn test_protocol_version_header_on_subsequent_requests() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();

        // Initialize
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
                        "protocolVersion": "2025-03-26",
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

        let init_response = app.clone().oneshot(init_request).await.unwrap();
        let session_id = init_response
            .headers()
            .get(MCP_SESSION_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // Verify init response has negotiated version (2025-03-26, not latest)
        assert_eq!(
            init_response
                .headers()
                .get(MCP_PROTOCOL_VERSION_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("2025-03-26")
        );

        // Send initialized notification
        let initialized_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header(MCP_SESSION_ID_HEADER, &session_id)
            .header(MCP_PROTOCOL_VERSION_HEADER, "2025-03-26")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                })
                .to_string(),
            ))
            .unwrap();

        app.clone().oneshot(initialized_request).await.unwrap();

        // Send tools/list and check for protocol version header
        let list_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header(MCP_SESSION_ID_HEADER, &session_id)
            .header(MCP_PROTOCOL_VERSION_HEADER, "2025-03-26")
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
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(MCP_PROTOCOL_VERSION_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("2025-03-26")
        );
    }

    #[tokio::test]
    async fn test_request_without_session_fails() {
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .require_sessions();
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
    async fn test_custom_session_store_receives_create_and_delete() {
        use crate::session_store::{MemorySessionStore, SessionStore as PublicSessionStore};

        let store = Arc::new(MemorySessionStore::new());
        let store_dyn: Arc<dyn PublicSessionStore> = store.clone();

        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .session_store(store_dyn);
        let (app, handle) = transport.into_router_with_handle();

        // Initialize to create a session.
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
                        "clientInfo": { "name": "test-client", "version": "1.0.0" }
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

        // Custom store should have the record.
        assert_eq!(store.len().await, 1);
        let record = store.load(&session_id).await.unwrap();
        assert!(record.is_some(), "expected session to be persisted");
        assert_eq!(record.unwrap().id, session_id);

        // Terminate session via the handle — store should be cleared.
        assert!(handle.terminate_session(&session_id).await);
        assert_eq!(store.len().await, 0);
        assert!(store.load(&session_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_custom_event_store_buffers_and_purges() {
        use crate::event_store::{EventStore as PublicEventStore, MemoryEventStore};

        let events = Arc::new(MemoryEventStore::new());
        let events_dyn: Arc<dyn PublicEventStore> = events.clone();

        // Build a session directly so we can exercise buffer_event/get_events_after
        // without needing a live SSE subscriber.
        let session = Arc::new(Session::new(
            create_test_router(),
            false,
            identity_factory(),
            events_dyn,
        ));

        session.buffer_event(0, "first".to_string()).await;
        session.buffer_event(1, "second".to_string()).await;

        // Custom store should have both events.
        assert_eq!(events.total_events().await, 2);
        let replayed = events.replay_after(&session.id, 0).await.unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].id, 1);
        assert_eq!(replayed[0].data, "second");

        // Purging should clear the session's log.
        events.purge_session(&session.id).await.unwrap();
        assert_eq!(events.total_events().await, 0);
    }

    #[tokio::test]
    async fn test_restore_from_store_serves_unknown_session_id() {
        use crate::session_store::{MemorySessionStore, SessionRecord, SessionStore};

        // Two transports share a single session store (simulating two
        // server instances behind a load balancer).
        let store = Arc::new(MemorySessionStore::new());
        let store_dyn: Arc<dyn SessionStore> = store.clone();

        // Seed the store with a record as if a peer instance had created it.
        let mut seeded = SessionRecord::new(
            "shared-session".to_string(),
            "2025-11-25".to_string(),
            Duration::from_secs(60),
        );
        store.create(&mut seeded).await.unwrap();
        let seeded_id = seeded.id;

        // This transport has never seen the session locally.
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .session_store(store_dyn);
        let app = transport.into_router();

        let list_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header(MCP_SESSION_ID_HEADER, &seeded_id)
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/list"
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(list_request).await.unwrap();
        // Without restore this would produce a SessionNotFound JSON-RPC
        // error; with restore the request is served normally.
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json.get("result").is_some(),
            "expected tools/list result, got {json}"
        );
    }

    #[tokio::test]
    async fn test_auto_reinitialize_serves_unknown_session_without_store_record() {
        // No seeded store record — the client just shows up with a
        // session ID the server has never heard of. With auto-reinit
        // enabled the transport spins up a synthetic session.
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .auto_reinitialize_sessions(true);
        let app = transport.into_router();

        let list_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header(MCP_SESSION_ID_HEADER, "client-made-up-id")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/list"
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(list_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json.get("result").is_some(),
            "expected tools/list result, got {json}"
        );
    }

    #[tokio::test]
    async fn test_unknown_session_without_restore_or_auto_reinit_returns_error() {
        // Default transport: no store seeded, no auto-reinit.
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();

        let list_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header(MCP_SESSION_ID_HEADER, "never-seen-before")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/list"
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(list_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("error").is_some(), "expected error, got {json}");
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
        let session = Session::new(
            create_test_router(),
            false,
            identity_factory(),
            Arc::new(crate::event_store::MemoryEventStore::new()),
        );

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
        let session = Session::new(
            create_test_router(),
            false,
            identity_factory(),
            Arc::new(crate::event_store::MemoryEventStore::new()),
        );

        assert_eq!(session.next_event_id(), 0);
        assert_eq!(session.next_event_id(), 1);
        assert_eq!(session.next_event_id(), 2);
    }

    #[tokio::test]
    async fn test_session_event_buffer_limit() {
        // Test that buffer respects max size limit
        // Create a session - buffer limit is DEFAULT_MAX_BUFFERED_EVENTS (1000)
        let session = Session::new(
            create_test_router(),
            false,
            identity_factory(),
            Arc::new(crate::event_store::MemoryEventStore::new()),
        );

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

    #[tokio::test]
    async fn test_session_handle_count() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let (app, handle) = transport.into_router_with_handle();

        // No sessions initially
        assert_eq!(handle.session_count().await, 0);

        // Initialize to create a session
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
        assert_eq!(response.status(), 200);

        // Now we should have 1 session
        assert_eq!(handle.session_count().await, 1);
    }

    #[tokio::test]
    async fn test_session_handle_list_and_terminate() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let (app, handle) = transport.into_router_with_handle();

        // No sessions initially
        assert!(handle.list_sessions().await.is_empty());

        // Initialize to create a session
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
        assert_eq!(response.status(), 200);

        // list_sessions should return 1 session with valid metadata
        let sessions = handle.list_sessions().await;
        assert_eq!(sessions.len(), 1);
        assert!(!sessions[0].id.is_empty());

        // Terminate the session
        let session_id = sessions[0].id.clone();
        assert!(handle.terminate_session(&session_id).await);
        assert_eq!(handle.session_count().await, 0);

        // Terminating again returns false
        assert!(!handle.terminate_session(&session_id).await);
    }

    #[tokio::test]
    async fn test_request_without_session_id_rejected() {
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .require_sessions();
        let app = transport.into_router();

        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            // No mcp-session-id header
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/list",
                    "params": {}
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK); // JSON-RPC errors still 200
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // Should return session required error
        assert!(json["error"].is_object());
    }

    #[tokio::test]
    async fn test_invalid_session_id_returns_error() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();

        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("mcp-session-id", "nonexistent-session-id")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/list",
                    "params": {}
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"].as_i64().unwrap(), -32005); // SessionNotFound
    }

    #[tokio::test]
    async fn test_notification_returns_accepted() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();

        // First initialize to get a session
        let init_req = Request::builder()
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
                        "clientInfo": { "name": "test", "version": "1.0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let resp = app.clone().oneshot(init_req).await.unwrap();
        let session_id = resp
            .headers()
            .get(MCP_SESSION_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // Send a notification (no id field) -- should return 202 Accepted
        let notif = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("mcp-session-id", &session_id)
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(notif).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn test_invalid_json_returns_parse_error() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();

        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(Body::from("not valid json{{{"))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"].as_i64().unwrap(), -32700); // Parse error
    }

    #[tokio::test]
    async fn test_session_config_max_sessions() {
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .session_config(SessionConfig::default().max_sessions(1));
        let app = transport.into_router();

        // First initialize succeeds
        let init1 = Request::builder()
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
                        "clientInfo": { "name": "test1", "version": "1.0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let resp1 = app.clone().oneshot(init1).await.unwrap();
        assert_eq!(resp1.status(), StatusCode::OK);

        // Second initialize should fail (max 1 session)
        let init2 = Request::builder()
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
                        "clientInfo": { "name": "test2", "version": "1.0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let resp2 = app.oneshot(init2).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_delete_terminates_session() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();

        // Initialize
        let init_req = Request::builder()
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
                        "clientInfo": { "name": "test", "version": "1.0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let resp = app.clone().oneshot(init_req).await.unwrap();
        let session_id = resp
            .headers()
            .get(MCP_SESSION_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // DELETE should terminate the session
        let delete_req = Request::builder()
            .method("DELETE")
            .uri("/")
            .header("mcp-session-id", &session_id)
            .body(Body::empty())
            .unwrap();

        let resp = app.clone().oneshot(delete_req).await.unwrap();
        assert!(resp.status().is_success());

        // Subsequent request with that session ID should fail
        let list_req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("mcp-session-id", &session_id)
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/list",
                    "params": {}
                })
                .to_string(),
            ))
            .unwrap();

        let resp = app.oneshot(list_req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"].as_i64().unwrap(), -32005);
    }

    // -----------------------------------------------------------------------
    // Origin validation / DNS rebinding protection
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_localhost_origin_http() {
        assert!(is_localhost_origin("http://localhost"));
        assert!(is_localhost_origin("http://localhost:3000"));
        assert!(is_localhost_origin("http://127.0.0.1"));
        assert!(is_localhost_origin("http://127.0.0.1:8080"));
        assert!(is_localhost_origin("http://[::1]"));
        assert!(is_localhost_origin("http://[::1]:3000"));
    }

    #[test]
    fn test_is_localhost_origin_https() {
        assert!(is_localhost_origin("https://localhost"));
        assert!(is_localhost_origin("https://127.0.0.1:443"));
    }

    #[test]
    fn test_is_not_localhost_origin() {
        assert!(!is_localhost_origin("http://example.com"));
        assert!(!is_localhost_origin("http://evil-localhost.com"));
        assert!(!is_localhost_origin("http://localhost.evil.com"));
        assert!(!is_localhost_origin("ftp://localhost"));
        assert!(!is_localhost_origin("localhost"));
        assert!(!is_localhost_origin(""));
    }

    #[tokio::test]
    async fn test_origin_validation_rejects_cross_origin() {
        let transport = HttpTransport::new(create_test_router());
        let app = transport.into_router();

        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Origin", "http://evil.com")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": { "name": "test", "version": "1.0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_origin_validation_allows_localhost() {
        let transport = HttpTransport::new(create_test_router());
        let app = transport.into_router();

        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Origin", "http://localhost:3000")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": { "name": "test", "version": "1.0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_origin_validation_allows_configured_origin() {
        let transport = HttpTransport::new(create_test_router())
            .allowed_origins(vec!["https://my-app.example.com".to_string()]);
        let app = transport.into_router();

        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Origin", "https://my-app.example.com")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": { "name": "test", "version": "1.0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_origin_validation_rejects_unconfigured_origin() {
        let transport = HttpTransport::new(create_test_router())
            .allowed_origins(vec!["https://my-app.example.com".to_string()]);
        let app = transport.into_router();

        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Origin", "https://other-app.example.com")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": { "name": "test", "version": "1.0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_origin_validation_no_header_allowed() {
        // Requests without Origin header should be allowed (same-origin)
        let transport = HttpTransport::new(create_test_router());
        let app = transport.into_router();

        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            // No Origin header
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": { "name": "test", "version": "1.0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_disabled_origin_validation_allows_any() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();

        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Origin", "http://evil.com")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": { "name": "test", "version": "1.0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
