//! Streamable HTTP transport for MCP
//!
//! Implements the Streamable HTTP transport from MCP specification 2025-11-25,
//! with version-gated support for the 2026-07-28 stateless protocol (SEP-2575 /
//! SEP-2567) when the `protocol-2026-07-28` feature is compiled in.
//!
//! ## Features
//!
//! - Single endpoint for POST (requests) and GET (SSE notifications)
//! - Session management via `MCP-Session-Id` header
//! - SSE streaming for server notifications and progress updates
//! - SSE event IDs and stream resumption via `Last-Event-ID` header (SEP-1699)
//! - Configurable session TTL and cleanup
//! - **Sampling support**: Server-to-client LLM requests via SSE + POST
//! - **2026 protocol mode** (`protocol-2026-07-28` feature): version-gated
//!   dispatch with per-request `_meta` and no session handshake
//!
//! ## Stateless mode (2026-07-28 protocol)
//!
//! When the `protocol-2026-07-28` feature (or its former `stateless` alias) is
//! compiled in, the transport handles two distinct stateless paths:
//!
//! ### Automatic version-gated path (2026-07-28)
//!
//! Any request that arrives with `MCP-Protocol-Version: 2026-07-28` and no
//! `mcp-session-id` header is dispatched statelessly, regardless
//! of whether [`HttpTransport::stateless()`] was called. The client is fully
//! self-identifying: every request carries its protocol version, client info,
//! and client capabilities in the `_meta` object; no initialize handshake is
//! needed. Handlers access this data via
//! [`RequestContext::per_request_meta()`](crate::context::RequestContext::per_request_meta).
//!
//! This path runs before the legacy SEP-1442 opt-in path, so 2026-07-28
//! clients are always handled correctly even on transports that never call
//! `HttpTransport::stateless()`.
//!
//! ### Legacy SEP-1442 opt-in path
//!
//! Calling [`HttpTransport::stateless()`] with a [`crate::stateless::StatelessConfig`]
//! activates the older SEP-1442-style opt-in stateless behavior for clients that
//! do not carry `MCP-Protocol-Version: 2026-07-28`. See
//! [`crate::stateless::StatelessConfig`] for details on what this path controls.
//!
//! Stateful clients (those sending an `mcp-session-id`) continue to work
//! normally on the same transport alongside both stateless paths.
//!
//! ## `subscriptions/listen` SSE stream
//!
//! Clients using the 2026-07-28 protocol open a server-to-client notification
//! stream by POSTing a `subscriptions/listen` JSON-RPC request. The server responds
//! with `Content-Type: text/event-stream` and streams zero or more
//! `notifications/*` events until the client disconnects.
//!
//! This replaces the `GET /` SSE endpoint used by the 2025-11-25 protocol. The
//! `GET /` endpoint is still supported for 2025-11-25 sessions; `subscriptions/listen`
//! is only available for 2026-07-28 clients.
//!
//! ```text
//! Client (2026-07-28)                          Server
//!   |                                            |
//!   |-- POST / {method: "subscriptions/listen",  |
//!   |           MCP-Protocol-Version: 2026-07-28} -->|
//!   |<-- 200 Content-Type: text/event-stream ----|
//!   |<-- event: message (notification) ----------|
//!   |<-- event: message (notification) ----------|
//!   |   (client disconnects)                     |
//! ```
//!
//! ## SEP-2243 HTTP headers
//!
//! SEP-2243 defines HTTP headers that let load balancers, proxies, and
//! observability tools inspect MCP traffic without parsing the JSON-RPC body:
//!
//! | Header | Required when | Description |
//! |--------|---------------|-------------|
//! | `Mcp-Method` | All POST requests (strict mode) | Mirrors the JSON-RPC `method` field |
//! | `Mcp-Name` | `tools/call`, `prompts/get`, `resources/read` (strict mode) | Mirrors `params.name` or `params.uri` |
//! | `MCP-Protocol-Version` | All requests (strict mode) | The protocol version in use |
//!
//! Validation is **lenient** for 2025-11-25 clients: headers present in the
//! request are validated for consistency with the body, but missing headers are
//! not an error. Validation is **strict** for 2026-07-28 clients: `Mcp-Method`
//! must be present on every POST and `Mcp-Name` must be present for the three
//! named methods. Violations return `-32020` (HeaderMismatch).
//!
//! The public constants [`MCP_METHOD_HEADER`], [`MCP_NAME_HEADER`], and
//! [`MCP_PARAM_HEADER_PREFIX`] hold the canonical lowercase header names.
//!
//! ## Sampling (Server-to-Client Requests)
//!
//! When using `HttpTransport::new(router).with_sampling()`, tool handlers can request
//! LLM completions from the client. The flow is:
//!
//! 1. Tool handler calls `ctx.sample(params)`
//! 2. Server upgrades that originating POST response to SSE and sends the
//!    sampling request on it
//! 3. Client receives the request and processes it
//! 4. Client sends the response as a POST to the MCP endpoint
//! 5. Server routes the response back to the waiting handler and finishes the
//!    original POST SSE stream with the tool result
//!
//! Restricted server-to-client requests are never sent on the standalone GET
//! notification stream. Associated POST streams are process-local and are not
//! resumable; deployments using sampling, elicitation, or roots requests need
//! session affinity for the duration of the exchange.
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
//! | Code    | Name                      | Description                                        |
//! |---------|---------------------------|----------------------------------------------------|
//! | -32020  | HeaderMismatch            | Required HTTP header missing or inconsistent with body (SEP-2243, strict mode) |
//! | -32022  | UnsupportedProtocolVersion| Server does not support the requested protocol version (SEP-2575) |
//! | -32005  | SessionNotFound           | Session expired or server restarted                |
//! | -32006  | SessionRequired           | MCP-Session-Id header missing                      |
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
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{delete, get, post},
};
#[cfg(feature = "stateless")]
use tokio::sync::mpsc;
use tokio::sync::{Mutex, RwLock, broadcast, oneshot};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

#[cfg(feature = "stateless")]
use crate::context::ServerNotification;
use crate::context::{
    ChannelClientRequester, ClientRequesterHandle, NotificationReceiver, OutgoingRequest,
    OutgoingRequestReceiver, notification_channel, outgoing_request_channel,
};
use crate::error::{Error, JsonRpcError, Result};
#[cfg(feature = "stateless")]
use crate::error::{ErrorCode, McpErrorCode};
use crate::inspection::{McpDirection, McpProtocolRevision};
use crate::jsonrpc::{JsonRpcService, apply_protocol_result_fields, inspect_runtime_value};
#[cfg(feature = "stateless")]
use crate::protocol::SubscriptionFilter;
use crate::protocol::{
    ClientCapabilities, Implementation, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse, LATEST_PROTOCOL_VERSION, McpNotification, PROTOCOL_VERSION_2026_07_28,
    RequestId,
};
use crate::router::{McpRouter, RouterRequest, RouterResponse};
use crate::transport::service::{
    CatchError, InjectAnnotations, McpBoxService, ServiceFactory, identity_factory,
};
#[cfg(feature = "stateless")]
use crate::transport::subscriptions::{
    subscription_complete_response, subscription_matches, tagged_subscription_notification,
};
use crate::{ProtocolSupport, ProtocolSupportError};
use tower::util::BoxCloneService;

/// SEP-2575 per-request `_meta` extraction. Pulls `StatelessRequestMeta` from
/// the parsed request params and inserts it into the per-request `Extensions`
/// so handlers can read it via `ctx.per_request_meta()`. No-op if the request
/// has no `_meta`, params aren't an object, or the meta can't deserialize.
#[cfg(feature = "stateless")]
fn stash_per_request_meta(req: &JsonRpcRequest, ext: &mut crate::router::Extensions) {
    if let Some(params) = req.params.as_ref()
        && let Some(meta) = crate::stateless::StatelessRequestMeta::from_params(params)
    {
        ext.insert(meta);
    }
}

/// Header name for MCP session ID
pub const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";

/// Header name for MCP protocol version
pub const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";

/// SEP-2243: header that mirrors the JSON-RPC `method` field for HTTP
/// intermediaries (load balancers, observability) so they can route or
/// classify MCP traffic without parsing the body. Required on all POST
/// requests when the negotiated protocol version implements SEP-2243.
pub const MCP_METHOD_HEADER: &str = "mcp-method";

/// SEP-2243: header that mirrors `params.name` (for `tools/call` and
/// `prompts/get`) or `params.uri` (for `resources/read`). Required for
/// those three methods when the negotiated protocol version implements
/// SEP-2243.
pub const MCP_NAME_HEADER: &str = "mcp-name";

/// SEP-2243: prefix for custom headers derived from tool parameters
/// marked with the `x-mcp-header` JSON Schema extension. The full header
/// name is `Mcp-Param-{Name}`.
pub const MCP_PARAM_HEADER_PREFIX: &str = "mcp-param-";

/// Default maximum POST body size in bytes (4 MiB, matching rmcp).
///
/// See [`HttpTransport::max_body_size`].
pub const DEFAULT_MAX_BODY_SIZE: usize = 4 * 1024 * 1024;

/// SSE event type for JSON-RPC messages
const SSE_MESSAGE_EVENT: &str = "message";

/// Header name for Last-Event-ID (for SSE stream resumption per SEP-1699)
const LAST_EVENT_ID_HEADER: &str = "last-event-id";

/// Pending request waiting for a response from the client
struct PendingRequest {
    response_tx: oneshot::Sender<Result<serde_json::Value>>,
}

type AssociatedCall = Pin<Box<dyn Future<Output = Result<JsonRpcResponse>> + Send + 'static>>;

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
    /// Session-wide allocator for request-scoped server-to-client request IDs.
    ///
    /// Each originating POST owns a separate channel, but IDs must remain
    /// unique across concurrent POSTs in the same session.
    request_id_allocator: Option<Arc<AtomicI64>>,
    /// Negotiated protocol version (set after initialize)
    protocol_version: RwLock<String>,
    /// Client implementation info advertised in the `initialize` request.
    ///
    /// Populated by `handle_post` after a successful initialize response,
    /// and restored from a [`SessionRecord`](crate::session_store::SessionRecord)
    /// when a session is rebuilt from the persistent store. `None` until the
    /// first initialize completes.
    client_info: RwLock<Option<Implementation>>,
    /// Client capabilities advertised in the `initialize` request.
    ///
    /// Populated by `handle_post` after a successful initialize response,
    /// and restored from a [`SessionRecord`](crate::session_store::SessionRecord)
    /// when a session is rebuilt from the persistent store. `None` until the
    /// first initialize completes.
    client_capabilities: RwLock<Option<ClientCapabilities>>,
    /// Counter for SSE event IDs (for stream resumption per SEP-1699)
    event_counter: AtomicU64,
    /// Pluggable store for SSE events (enables cross-instance replay)
    event_store: Arc<dyn crate::event_store::EventStore>,
    /// Whether `notifications/initialized` has been received from the client.
    ///
    /// Per the MCP 2025-11-25 spec, clients MUST send this notification after
    /// receiving the `initialize` response and before sending any other requests.
    /// Checked by `handle_post` when `strict_initialization` is enabled on
    /// [`SessionConfig`]. Pre-initialized sessions (optional_sessions path) and
    /// restored sessions start with this set to `true`.
    initialized_notification_received: std::sync::atomic::AtomicBool,
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

        let request_id_allocator = if sampling_enabled {
            Some(Arc::new(AtomicI64::new(1)))
        } else {
            None
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
            request_id_allocator,
            protocol_version: RwLock::new(LATEST_PROTOCOL_VERSION.to_string()),
            client_info: RwLock::new(None),
            client_capabilities: RwLock::new(None),
            event_counter: AtomicU64::new(0),
            event_store,
            initialized_notification_received: std::sync::atomic::AtomicBool::new(false),
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
            request_id_allocator: None,
            protocol_version: RwLock::new(LATEST_PROTOCOL_VERSION.to_string()),
            client_info: RwLock::new(None),
            client_capabilities: RwLock::new(None),
            event_counter: AtomicU64::new(0),
            event_store,
            initialized_notification_received: std::sync::atomic::AtomicBool::new(false),
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

        let request_id_allocator = if sampling_enabled {
            Some(Arc::new(AtomicI64::new(1)))
        } else {
            None
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
            request_id_allocator,
            protocol_version: RwLock::new(record.protocol_version.clone()),
            client_info: RwLock::new(record.client_info.clone()),
            client_capabilities: RwLock::new(record.client_capabilities.clone()),
            event_counter: AtomicU64::new(0),
            event_store,
            // Restored sessions already completed the handshake on a previous
            // instance; treat `notifications/initialized` as already received.
            initialized_notification_received: std::sync::atomic::AtomicBool::new(true),
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
            request_id_allocator: None,
            protocol_version: RwLock::new(record.protocol_version.clone()),
            client_info: RwLock::new(record.client_info.clone()),
            client_capabilities: RwLock::new(record.client_capabilities.clone()),
            event_counter: AtomicU64::new(0),
            event_store,
            // Restored sessions already completed the handshake on a previous
            // instance; treat `notifications/initialized` as already received.
            initialized_notification_received: std::sync::atomic::AtomicBool::new(true),
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

    /// Fail request-scoped client requests whose originating POST is gone.
    async fn fail_pending_requests(&self, ids: &[RequestId], message: &str) {
        let removed = {
            let mut pending = self.pending_requests.lock().await;
            ids.iter()
                .filter_map(|id| pending.remove(id))
                .collect::<Vec<_>>()
        };

        for pending in removed {
            let _ = pending
                .response_tx
                .send(Err(Error::Transport(message.to_string())));
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
    /// Whether to enforce that clients send `notifications/initialized` before
    /// making any non-initialize requests, per the MCP 2025-11-25 spec.
    ///
    /// When `true` (the default), the transport returns a JSON-RPC
    /// `InvalidRequest` error (-32600) to any request received before
    /// `notifications/initialized` on a 2025-11-25 session-based connection.
    ///
    /// Set to `false` to restore the previous lenient behavior, e.g. in
    /// dev/test scenarios where the full MCP handshake is inconvenient.
    pub strict_initialization: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            ttl: DEFAULT_SESSION_TTL,
            max_sessions: None,
            cleanup_interval: DEFAULT_CLEANUP_INTERVAL,
            strict_initialization: true,
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

    /// Enable or disable strict initialization enforcement.
    ///
    /// When enabled (default), the transport enforces that clients send
    /// `notifications/initialized` before any other requests on a
    /// 2025-11-25 session-based connection, per the MCP spec. Requests
    /// that arrive before this notification receive a JSON-RPC
    /// `InvalidRequest` error (-32600).
    ///
    /// Disable this for dev/test scenarios where the full MCP handshake
    /// is inconvenient.
    pub fn strict_initialization(mut self, enabled: bool) -> Self {
        self.strict_initialization = enabled;
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
        // Populate the client identity / capabilities advertised at
        // initialize time so persisted records faithfully describe the
        // session. These remain `None` until a successful initialize.
        record.client_info = session.client_info.read().await.clone();
        record.client_capabilities = session.client_capabilities.read().await.clone();
        // Convert from monotonic Instant to SystemTime approximation.
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

    /// Persist an update to an existing session's record (upsert).
    ///
    /// Called after the session's state changes in a way that should be
    /// reflected in the persistent store -- notably after a successful
    /// `initialize` so the stored record carries the client's advertised
    /// `client_info` and `capabilities` (rather than the defaults captured
    /// at create time). Failures are logged but non-fatal.
    async fn save_record(&self, session: &Session) {
        let record = self.record_for(session).await;
        if let Err(e) = self.persistent.save(&record).await {
            tracing::warn!(session_id = %session.id, error = %e, "Failed to save session record");
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
            // Pre-initialized sessions bypass the full MCP handshake (they
            // exist for clients that don't track session IDs). Mark the
            // notification as already received so strict_initialization checks
            // don't reject their requests.
            session
                .initialized_notification_received
                .store(true, Ordering::Release);
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
            // Pre-initialized sessions bypass the full MCP handshake; mark the
            // notification as already received.
            session
                .initialized_notification_received
                .store(true, Ordering::Release);
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

    /// Send a pre-serialized JSON notification to every live session's SSE
    /// broadcast channel.
    ///
    /// Used by the external-notification fan-out task. Failures to send
    /// (no SSE subscribers attached to a session yet) are silent — the
    /// broadcast channel drops the message naturally.
    async fn broadcast_to_all(&self, json: &str) {
        let sessions = self.sessions.read().await;
        for session in sessions.values() {
            let _ = session.notifications_tx.send(json.to_string());
        }
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

/// A handle for managing HTTP transport sessions and final subscription streams.
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
///
/// // During graceful server shutdown (with the `stateless` feature):
/// handle.close_subscriptions();
/// ```
#[derive(Clone)]
pub struct SessionHandle {
    store: Arc<SessionRegistry>,
    #[cfg(feature = "stateless")]
    modern_subscriptions: Arc<ModernSubscriptionRegistry>,
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

    /// Returns the number of active final-protocol subscription streams.
    #[cfg(feature = "stateless")]
    pub fn subscription_count(&self) -> usize {
        self.modern_subscriptions.len()
    }

    /// Gracefully finish every active final-protocol subscription stream.
    ///
    /// Each stream receives its terminal `SubscriptionsListenResult` before
    /// closing. Returns the number of streams that were drained.
    #[cfg(feature = "stateless")]
    pub fn close_subscriptions(&self) -> usize {
        self.modern_subscriptions.close_all()
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
    /// Types copied from each HTTP request's extensions into the per-request
    /// MCP extensions (#1242). Empty by default.
    extension_bridges: Vec<crate::transport::extension_bridge::ExtensionBridge>,
    /// Exact protocol versions accepted and advertised by this transport.
    protocol_support: ProtocolSupport,
    /// Session store
    sessions: Arc<SessionRegistry>,
    /// Whether to validate Origin header
    validate_origin: bool,
    /// Allowed origins (if validation is enabled)
    allowed_origins: Vec<String>,
    /// Whether to validate Host header (defense against direct DNS rebinding)
    validate_host: bool,
    /// Allowed hosts (host:port). Localhost variants are always allowed.
    allowed_hosts: Vec<String>,
    /// Whether sessions are optional (for clients that don't track session IDs)
    optional_sessions: bool,
    /// Whether to enforce `notifications/initialized` before tool dispatch
    /// (see [`SessionConfig::strict_initialization`]).
    strict_initialization: bool,
    /// SEP-1442 stateless mode configuration
    #[cfg(feature = "stateless")]
    stateless_config: Option<crate::stateless::StatelessConfig>,
    /// Whether to stamp server identity into `_meta` on 2026-07-28 stateless
    /// responses (see [`HttpTransport::stamp_server_info()`]).
    #[cfg(feature = "stateless")]
    stamp_server_info: bool,
    /// Active final-protocol `subscriptions/listen` streams.
    #[cfg(feature = "stateless")]
    modern_subscriptions: Arc<ModernSubscriptionRegistry>,
    /// Whether to wrap synchronous responses in SSE format (rmcp compat)
    sse_responses: bool,
    /// Maximum accepted POST body size in bytes
    max_body_size: usize,
}

#[cfg(feature = "stateless")]
struct ModernSubscription {
    subscription_id: RequestId,
    filter: SubscriptionFilter,
    tx: mpsc::UnboundedSender<String>,
    started: std::time::Instant,
}

/// Process-local registry for sessionless final-protocol subscriptions.
///
/// The 2026-07-28 transport deliberately has no session or replay state.
/// Each listen POST owns one sender, removed when its response stream drops.
#[cfg(feature = "stateless")]
struct ModernSubscriptionRegistry {
    next_key: AtomicU64,
    subscriptions: std::sync::Mutex<HashMap<u64, ModernSubscription>>,
    server_info: Option<Implementation>,
    observer: Option<Arc<dyn crate::transport::subscriptions::SubscriptionObserver>>,
}

#[cfg(feature = "stateless")]
impl ModernSubscriptionRegistry {
    fn new(
        server_info: Option<Implementation>,
        observer: Option<Arc<dyn crate::transport::subscriptions::SubscriptionObserver>>,
    ) -> Self {
        Self {
            next_key: AtomicU64::new(0),
            subscriptions: std::sync::Mutex::new(HashMap::new()),
            server_info,
            observer,
        }
    }

    fn observe_close(
        &self,
        subscription: &ModernSubscription,
        reason: crate::transport::subscriptions::SubscriptionCloseReason,
    ) {
        if let Some(observer) = &self.observer {
            observer.on_close(crate::transport::subscriptions::SubscriptionClose {
                subscription_id: subscription.subscription_id.clone(),
                reason,
                duration: subscription.started.elapsed(),
            });
        }
    }

    fn register(
        self: &Arc<Self>,
        subscription_id: RequestId,
        filter: SubscriptionFilter,
    ) -> (mpsc::UnboundedReceiver<String>, ModernSubscriptionGuard) {
        let key = self.next_key.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscriptions.lock().unwrap().insert(
            key,
            ModernSubscription {
                subscription_id,
                filter,
                tx,
                started: std::time::Instant::now(),
            },
        );
        (
            rx,
            ModernSubscriptionGuard {
                key,
                registry: self.clone(),
            },
        )
    }

    /// Route subscription-scoped notifications and return whether the
    /// notification belongs exclusively on listen streams.
    fn publish(&self, notification: &ServerNotification) -> bool {
        let subscription_scoped = matches!(
            notification,
            ServerNotification::ResourceUpdated { .. }
                | ServerNotification::ResourcesListChanged
                | ServerNotification::ToolsListChanged
                | ServerNotification::PromptsListChanged
                | ServerNotification::FinalTaskStatusChanged(_)
        );
        if !subscription_scoped {
            return false;
        }

        let mut subscriptions = self.subscriptions.lock().unwrap();
        tracing::trace!(
            active_subscriptions = subscriptions.len(),
            notification = ?notification,
            "Routing final-protocol subscription notification"
        );
        subscriptions.retain(|_, subscription| {
            let keep = if subscription_matches(notification, &subscription.filter)
                && let Some(json) =
                    tagged_subscription_notification(notification, &subscription.subscription_id)
            {
                subscription.tx.send(json).is_ok()
            } else {
                !subscription.tx.is_closed()
            };
            if !keep {
                // Detected here rather than at guard drop: the entry is
                // removed exactly once, so the close is reported exactly once.
                self.observe_close(
                    subscription,
                    crate::transport::subscriptions::SubscriptionCloseReason::Disconnected,
                );
            }
            keep
        });
        true
    }

    fn len(&self) -> usize {
        self.subscriptions.lock().unwrap().len()
    }

    /// Gracefully finish every active HTTP listen stream.
    fn close_all(&self) -> usize {
        let subscriptions = {
            let mut active = self.subscriptions.lock().unwrap();
            active
                .drain()
                .map(|(_, subscription)| subscription)
                .collect::<Vec<_>>()
        };
        let count = subscriptions.len();
        for subscription in subscriptions {
            self.observe_close(
                &subscription,
                crate::transport::subscriptions::SubscriptionCloseReason::Drained,
            );
            let response = subscription_complete_response(
                subscription.subscription_id,
                self.server_info.clone(),
            );
            if let Ok(json) = serde_json::to_string(&response) {
                let _ = subscription.tx.send(json);
            }
        }
        count
    }
}

#[cfg(feature = "stateless")]
impl Default for ModernSubscriptionRegistry {
    fn default() -> Self {
        Self::new(None, None)
    }
}

#[cfg(feature = "stateless")]
struct ModernSubscriptionGuard {
    key: u64,
    registry: Arc<ModernSubscriptionRegistry>,
}

#[cfg(feature = "stateless")]
impl Drop for ModernSubscriptionGuard {
    fn drop(&mut self) {
        let removed = self
            .registry
            .subscriptions
            .lock()
            .unwrap()
            .remove(&self.key);
        if let Some(subscription) = removed {
            self.registry.observe_close(
                &subscription,
                crate::transport::subscriptions::SubscriptionCloseReason::Disconnected,
            );
        }
    }
}

/// Configuration for OAuth 2.1 Protected Resource Metadata.
///
/// When set on [`HttpTransport`], a `GET` endpoint is added at the resource's
/// path-aware RFC 9728 well-known location.
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
    /// Types copied from each HTTP request's extensions into the per-request
    /// MCP extensions (#1242). Empty by default.
    extension_bridges: Vec<crate::transport::extension_bridge::ExtensionBridge>,
    protocol_support: ProtocolSupport,
    validate_origin: bool,
    allowed_origins: Vec<String>,
    validate_host: bool,
    allowed_hosts: Vec<String>,
    session_config: SessionConfig,
    sampling_enabled: bool,
    optional_sessions: bool,
    session_store: Arc<dyn crate::session_store::SessionStore>,
    event_store: Arc<dyn crate::event_store::EventStore>,
    auto_reinit_sessions: bool,
    /// Caller-owned receiver for notifications pushed from outside any
    /// request handler. Drained by a background task and fanned out to
    /// every live session's SSE stream.
    external_notifications: Option<NotificationReceiver>,
    #[cfg(feature = "stateless")]
    stateless_config: Option<crate::stateless::StatelessConfig>,
    /// When true, 2026-07-28 stateless responses carry server identity in
    /// `_meta["io.modelcontextprotocol/serverInfo"]`.
    ///
    /// See [`HttpTransport::stamp_server_info()`] for details.
    #[cfg(feature = "stateless")]
    stamp_server_info: bool,
    #[cfg(feature = "oauth")]
    oauth_config: Option<OAuthConfig>,
    /// When true, synchronous JSON-RPC responses are wrapped in SSE format.
    ///
    /// See [`HttpTransport::sse_responses()`] for details.
    sse_responses: bool,
    /// Maximum accepted POST body size in bytes.
    ///
    /// See [`HttpTransport::max_body_size()`] for details.
    max_body_size: usize,
    /// How long [`HttpTransport::serve_with_shutdown()`] waits for open
    /// connections once the shutdown signal fires. `None` waits for all of
    /// them.
    drain_timeout: Option<Duration>,
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
            protocol_support: ProtocolSupport::default(),
            validate_origin: true,
            allowed_origins: vec![],
            validate_host: true,
            allowed_hosts: vec![],
            session_config: SessionConfig::default(),
            sampling_enabled: false,
            optional_sessions: true,
            session_store: Arc::new(crate::session_store::MemorySessionStore::new()),
            event_store: Arc::new(crate::event_store::MemoryEventStore::new()),
            auto_reinit_sessions: false,
            external_notifications: None,
            #[cfg(feature = "stateless")]
            stateless_config: None,
            #[cfg(feature = "stateless")]
            stamp_server_info: true,
            #[cfg(feature = "oauth")]
            oauth_config: None,
            sse_responses: false,
            extension_bridges: Vec::new(),
            max_body_size: DEFAULT_MAX_BODY_SIZE,
            drain_timeout: None,
        }
    }

    /// Copy `T` out of each HTTP request's extensions into the per-request
    /// MCP extensions.
    ///
    /// A tower layer in front of this transport can attach anything it likes
    /// to the request: a resolved identity, a tenant, a tracing id, the peer
    /// address. Registering the type here is what makes it visible to
    /// handlers through
    /// [`RequestContext::extension`](crate::RequestContext::extension)
    /// (#1242).
    ///
    /// Requests that do not carry a `T` are left alone, so a layer that only
    /// attaches its value on some routes is fine.
    ///
    /// Bridging is per type rather than wholesale, so nothing crosses into
    /// handler code that the server did not choose to expose.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::{McpRouter, HttpTransport};
    ///
    /// #[derive(Clone)]
    /// struct AgentIdentity(String);
    ///
    /// let router = McpRouter::new().server_info("my-server", "1.0.0");
    /// let app = HttpTransport::new(router)
    ///     .bridge_extension::<AgentIdentity>()
    ///     .into_router_at("/mcp");
    /// // A layer inserts AgentIdentity; a handler reads it with
    /// // ctx.extension::<AgentIdentity>().
    /// ```
    pub fn bridge_extension<T>(mut self) -> Self
    where
        T: Clone + Send + Sync + 'static,
    {
        self.extension_bridges
            .push(crate::transport::extension_bridge::extension_bridge::<T>());
        self
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
            protocol_support: ProtocolSupport::default(),
            validate_origin: true,
            allowed_origins: vec![],
            validate_host: true,
            allowed_hosts: vec![],
            session_config: SessionConfig::default(),
            sampling_enabled: false,
            optional_sessions: true,
            session_store: Arc::new(crate::session_store::MemorySessionStore::new()),
            event_store: Arc::new(crate::event_store::MemoryEventStore::new()),
            auto_reinit_sessions: false,
            external_notifications: None,
            #[cfg(feature = "stateless")]
            stateless_config: None,
            #[cfg(feature = "stateless")]
            stamp_server_info: true,
            #[cfg(feature = "oauth")]
            oauth_config: None,
            sse_responses: false,
            extension_bridges: Vec::new(),
            max_body_size: DEFAULT_MAX_BODY_SIZE,
            drain_timeout: None,
        }
    }

    /// Create an HTTP transport that drains a caller-owned notification
    /// channel and fans the items out to every live session's SSE stream.
    ///
    /// This mirrors [`GenericStdioTransport::with_notifications`](crate::transport::stdio::GenericStdioTransport::with_notifications)
    /// and is the supported way to push server-originated notifications
    /// (e.g. `notifications/resources/updated`) from outside any request
    /// handler — background tasks, lifecycle hooks, anything async that
    /// needs to notify subscribed clients.
    ///
    /// Per-session notification channels (in-handler `ctx.send_log()`,
    /// progress updates) are unaffected. The external channel runs in
    /// parallel and broadcasts to every active session; MCP clients are
    /// expected to ignore notifications they didn't subscribe to.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::{BoxError, McpRouter};
    /// use tower_mcp::context::{ServerNotification, notification_channel};
    /// use tower_mcp::transport::http::HttpTransport;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), BoxError> {
    ///     let (notif_tx, notif_rx) = notification_channel(256);
    ///
    ///     let router = McpRouter::new().server_info("my-server", "1.0.0");
    ///
    ///     // Hold onto notif_tx in your application state so background tasks
    ///     // can push notifications. tx is `Clone`.
    ///     let pusher = notif_tx.clone();
    ///     tokio::spawn(async move {
    ///         let _ = pusher.send(ServerNotification::ResourceUpdated {
    ///             uri: "claude://chats/123".to_string(),
    ///         }).await;
    ///     });
    ///
    ///     let transport = HttpTransport::with_notifications(router, notif_rx);
    ///     transport.serve("127.0.0.1:3000").await?;
    ///     Ok(())
    /// }
    /// ```
    pub fn with_notifications(router: McpRouter, notification_rx: NotificationReceiver) -> Self {
        Self {
            external_notifications: Some(notification_rx),
            ..Self::new(router)
        }
    }

    /// Attach a caller-owned notification receiver after construction.
    ///
    /// Useful when wrapping a pre-built service via
    /// [`from_service`](Self::from_service), where setting a sender on the
    /// router isn't part of the flow. See [`with_notifications`](Self::with_notifications)
    /// for the typical router-based path.
    pub fn external_notifications(mut self, notification_rx: NotificationReceiver) -> Self {
        self.external_notifications = Some(notification_rx);
        self
    }

    /// Enable sampling support for this transport.
    ///
    /// When sampling is enabled, tool handlers can use `ctx.sample()` to
    /// request LLM completions from connected clients. The server sends each
    /// request on the SSE response stream of the POST that caused it, and the
    /// client responds via a separate POST. These associated streams are not
    /// replayed; use session affinity while a request is in flight.
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

    /// Set the exact protocol versions this transport accepts and advertises.
    ///
    /// By default, every protocol implementation compiled into tower-mcp is
    /// enabled. This setting can narrow that set per server instance. Versions
    /// are advertised by `server/discover` in the order supplied.
    pub fn protocol_support(mut self, support: ProtocolSupport) -> Self {
        self.protocol_support = support;
        self
    }

    /// Construct and set an exact runtime protocol-version allow-list.
    ///
    /// Returns an error when the list is empty, duplicated, or names a version
    /// whose Cargo feature was not compiled.
    pub fn protocol_versions<I, S>(
        mut self,
        versions: I,
    ) -> std::result::Result<Self, ProtocolSupportError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.protocol_support = ProtocolSupport::try_new(versions)?;
        Ok(self)
    }

    /// Enable SSE-wrapping for synchronous JSON-RPC responses.
    ///
    /// When enabled, synchronous responses (initialize, tools/list, tools/call, etc.)
    /// are returned with `Content-Type: text/event-stream` and formatted as an SSE
    /// message event:
    ///
    /// ```text
    /// event: message
    /// data: {"jsonrpc":"2.0","id":1,"result":{...}}
    ///
    /// ```
    ///
    /// This matches the behavior of rmcp's `StreamableHttpService`, which always uses
    /// SSE format for all responses. The MCP Streamable HTTP spec allows both bare
    /// JSON and SSE for synchronous responses; this option is provided for
    /// compatibility with clients that expect rmcp's SSE-always behavior.
    ///
    /// **Known divergence from rmcp:** rmcp's `StreamableHttpService` always uses SSE
    /// for synchronous responses by default. tower-mcp defaults to bare JSON (the
    /// spec-correct choice, matching the SHOULD in the 2025-11-25 spec). Use
    /// `.sse_responses(true)` to match rmcp's behavior when targeting clients
    /// written against rmcp.
    ///
    /// The existing SSE notification stream (GET `/`) and `subscriptions/listen` stream
    /// (2026-07-28+) are unaffected by this flag.
    ///
    /// Default: `false` (bare JSON, `Content-Type: application/json`).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let transport = HttpTransport::new(router).sse_responses(true);
    /// ```
    pub fn sse_responses(mut self, enabled: bool) -> Self {
        self.sse_responses = enabled;
        self
    }

    /// Whether 2026-07-28 stateless responses carry server identity in
    /// `_meta["io.modelcontextprotocol/serverInfo"]`.
    ///
    /// Per SEP-2575, servers SHOULD identify themselves in each result's
    /// `_meta` "unless specifically configured not to do so" -- this is that
    /// configuration. Only applies to the version-gated 2026-07-28 stateless
    /// dispatch path (`stateless` feature); other protocol versions and
    /// transports are unaffected, and identity there is carried by
    /// `initialize`'s top-level `serverInfo` instead.
    ///
    /// Only takes effect when the transport was built from an [`McpRouter`]
    /// (`HttpTransport::new`); a transport built from a pre-built service
    /// (`HttpTransport::from_service`) has no router to read identity from
    /// and never stamps, regardless of this setting.
    ///
    /// Default: `true`.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let transport = HttpTransport::new(router).stamp_server_info(false);
    /// ```
    #[cfg(feature = "stateless")]
    pub fn stamp_server_info(mut self, enabled: bool) -> Self {
        self.stamp_server_info = enabled;
        self
    }

    /// Set the maximum accepted POST body size in bytes.
    ///
    /// Requests whose body exceeds the limit are rejected with HTTP 413
    /// (Payload Too Large) before any JSON parsing or dispatch happens.
    /// A `Content-Length` header above the limit short-circuits without
    /// reading the body; chunked bodies are capped while streaming.
    ///
    /// Default: 4 MiB ([`DEFAULT_MAX_BODY_SIZE`]), matching rmcp.
    ///
    /// # Interplay with axum's `DefaultBodyLimit`
    ///
    /// axum's built-in [`DefaultBodyLimit`](axum::extract::DefaultBodyLimit)
    /// (2 MB by default) only applies to body-consuming extractors such as
    /// `Bytes`, `String`, and `Json`. The MCP endpoint consumes the raw
    /// [`Request`](axum::extract::Request) and reads the body itself, so
    /// `DefaultBodyLimit` never applies to it; this transport-level limit
    /// is the only bound on the MCP POST body. Layering
    /// `DefaultBodyLimit` onto the router returned by
    /// [`into_router`](Self::into_router) does not change the MCP
    /// endpoint's behavior.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::McpRouter;
    /// use tower_mcp::transport::http::HttpTransport;
    ///
    /// let router = McpRouter::new().server_info("my-server", "1.0.0");
    /// // Accept request bodies up to 1 MiB.
    /// let transport = HttpTransport::new(router).max_body_size(1024 * 1024);
    /// ```
    pub fn max_body_size(mut self, bytes: usize) -> Self {
        self.max_body_size = bytes;
        self
    }

    /// Enable the legacy SEP-1442 stateless opt-in path.
    ///
    /// This activates the SEP-1442-style stateless behavior for clients that
    /// do NOT send `MCP-Protocol-Version: 2026-07-28`. Specifically, when a
    /// [`crate::stateless::StatelessConfig`] is set:
    ///
    /// - Requests without a session ID can be served without an initialize
    ///   handshake (if [`crate::stateless::StatelessConfig::optional_sessions`]
    ///   is `true`).
    /// - The `server/discover` RPC is enabled (if
    ///   [`crate::stateless::StatelessConfig::enable_discover`] is `true`).
    /// - Protocol version may be required in every request body (if
    ///   [`crate::stateless::StatelessConfig::require_protocol_version`] is `true`).
    ///
    /// **Note:** this method does NOT control the automatic version-gated
    /// stateless path for 2026-07-28+ clients. When the `stateless` feature
    /// is compiled in, any request with `MCP-Protocol-Version: 2026-07-28`
    /// and no `mcp-session-id` is dispatched statelessly regardless of
    /// whether this method is called. See the [`crate::stateless`] module
    /// documentation for the full two-path explanation.
    ///
    /// Stateful clients (those that send `mcp-session-id`) continue to work
    /// normally on the same transport.
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
    ///     // Enables the SEP-1442 opt-in path. 2026-07-28 clients are
    ///     // handled statelessly regardless of this call.
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

    /// Disable Host header validation (not recommended when binding to a
    /// non-loopback interface).
    ///
    /// Host validation is the defense-in-depth pair to Origin validation: it
    /// rejects requests whose `Host` header doesn't match the server's
    /// expected hostname, blocking direct DNS-rebinding attacks where a
    /// malicious site resolves its own domain to `127.0.0.1`.
    pub fn disable_host_validation(mut self) -> Self {
        self.validate_host = false;
        self
    }

    /// Set allowed hosts for the `Host` header allowlist.
    ///
    /// Each entry should be a `host:port` pair (e.g. `"api.example.com"`,
    /// `"api.example.com:8443"`). Localhost variants (`localhost`,
    /// `127.0.0.1`, `::1`, with any port) are always accepted regardless
    /// of this list.
    ///
    /// When the `Host` header is missing, the validator falls back to the
    /// HTTP/2 `:authority` pseudo-header from `request.uri().authority()`,
    /// since middleware like `axum::Router::nest` can strip the synthesized
    /// `Host` header before it reaches our handler.
    pub fn allowed_hosts(mut self, hosts: Vec<String>) -> Self {
        self.allowed_hosts = hosts;
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
    /// This lower-level method only serves metadata; it does not install token
    /// or scope enforcement. Prefer [`Self::into_oauth_router`] for a complete,
    /// fail-closed MCP resource-server setup.
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

    /// Build a fully protected OAuth resource-server router.
    ///
    /// This validates the Protected Resource Metadata, serves it at the
    /// path-aware RFC 9728 endpoint, validates bearer tokens, independently
    /// enforces the token audience against `metadata.resource`, and installs
    /// fail-closed per-operation scope enforcement.
    ///
    /// # Errors
    ///
    /// Returns an error when the resource metadata is not suitable for an MCP
    /// resource server.
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
        let (router, _) = self.into_oauth_router_with_handle(validator, metadata, policy)?;
        Ok(router)
    }

    /// Build a fully protected OAuth router and return its session handle.
    ///
    /// This is the session-management variant of [`Self::into_oauth_router`].
    #[cfg(feature = "oauth")]
    pub fn into_oauth_router_with_handle<V>(
        self,
        validator: V,
        metadata: crate::oauth::ProtectedResourceMetadata,
        policy: crate::oauth::ScopePolicy,
    ) -> std::result::Result<(Router, SessionHandle), crate::oauth::ProtectedResourceMetadataError>
    where
        V: crate::oauth::TokenValidator,
    {
        metadata.validate()?;
        let oauth_layer =
            crate::oauth::OAuthLayer::new(validator, metadata.clone()).scope_policy(policy.clone());
        let transport = self
            .layer(crate::oauth::ScopeEnforcementLayer::new(policy))
            .oauth(metadata);
        let (router, handle) = transport.into_router_with_handle();
        Ok((router.layer(oauth_layer), handle))
    }

    /// Build a fully protected OAuth router mounted at `path`.
    ///
    /// The metadata route is derived from `metadata.resource`, not from the
    /// local mount path, so it remains correct for path-based resource URLs.
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
        let (router, _) =
            self.into_oauth_router_at_with_handle(path, validator, metadata, policy)?;
        Ok(router)
    }

    /// Build a path-mounted protected OAuth router and return its session handle.
    #[cfg(feature = "oauth")]
    pub fn into_oauth_router_at_with_handle<V>(
        self,
        path: &str,
        validator: V,
        metadata: crate::oauth::ProtectedResourceMetadata,
        policy: crate::oauth::ScopePolicy,
    ) -> std::result::Result<(Router, SessionHandle), crate::oauth::ProtectedResourceMetadataError>
    where
        V: crate::oauth::TokenValidator,
    {
        metadata.validate()?;
        let oauth_layer =
            crate::oauth::OAuthLayer::new(validator, metadata.clone()).scope_policy(policy.clone());
        let transport = self
            .layer(crate::oauth::ScopeEnforcementLayer::new(policy))
            .oauth(metadata);
        let (router, handle) = transport.into_router_at_with_handle(path);
        Ok((router.layer(oauth_layer), handle))
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
        #[cfg(feature = "stateless")]
        let modern_subscriptions = Arc::new(ModernSubscriptionRegistry::new(
            match &self.service_source {
                ServiceSource::Router { router, .. } if self.stamp_server_info => {
                    Some(router.implementation())
                }
                _ => None,
            },
            match &self.service_source {
                ServiceSource::Router { router, .. } => router.subscription_observer(),
                ServiceSource::Service(_) => None,
            },
        ));

        // Keep one transport-lifetime notification sender registered with
        // dynamic registries. Per-request senders intentionally are not
        // registered, but dynamic mutations still need a stable path to all
        // active final-protocol listen streams.
        #[cfg(feature = "stateless")]
        let service_source = match &self.service_source {
            ServiceSource::Router { router, factory } => {
                let (tx, mut rx) = notification_channel(256);
                let direct_subscriptions = modern_subscriptions.clone();
                router.attach_modern_notification_sink(Arc::new(move |notification| {
                    direct_subscriptions.publish(notification)
                }));
                let subscriptions = modern_subscriptions.clone();
                tokio::spawn(async move {
                    while let Some(notification) = rx.recv().await {
                        subscriptions.publish(&notification);
                    }
                });
                ServiceSource::Router {
                    router: router.clone().with_notification_sender(tx),
                    factory: factory.clone(),
                }
            }
            ServiceSource::Service(service) => ServiceSource::Service(service.clone()),
        };
        #[cfg(not(feature = "stateless"))]
        let service_source = self.service_source.clone();

        let sessions = Arc::new(SessionRegistry::new(
            self.session_config.clone(),
            self.sampling_enabled,
            self.session_store.clone(),
            self.event_store.clone(),
            service_source.clone(),
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
            service_source,
            protocol_support: self.protocol_support.clone(),
            sessions,
            validate_origin: self.validate_origin,
            allowed_origins: self.allowed_origins.clone(),
            validate_host: self.validate_host,
            allowed_hosts: self.allowed_hosts.clone(),
            optional_sessions: self.optional_sessions,
            strict_initialization: self.session_config.strict_initialization,
            #[cfg(feature = "stateless")]
            stateless_config: self.stateless_config.clone(),
            #[cfg(feature = "stateless")]
            stamp_server_info: self.stamp_server_info,
            #[cfg(feature = "stateless")]
            modern_subscriptions,
            sse_responses: self.sse_responses,
            extension_bridges: self.extension_bridges.clone(),
            max_body_size: self.max_body_size,
        })
    }

    /// Build the axum router for this transport.
    pub fn into_router(self) -> Router {
        let (router, _handle) = self.into_router_with_handle();
        router
    }

    /// Build the axum router and return a [`SessionHandle`] for managing
    /// sessions and final subscription streams.
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
    pub fn into_router_with_handle(mut self) -> (Router, SessionHandle) {
        let external_rx = self.external_notifications.take();
        let state = self.build_state();
        let handle = SessionHandle {
            store: state.sessions.clone(),
            #[cfg(feature = "stateless")]
            modern_subscriptions: state.modern_subscriptions.clone(),
        };

        spawn_external_notification_fanout(
            external_rx,
            state.sessions.clone(),
            #[cfg(feature = "stateless")]
            state.modern_subscriptions.clone(),
        );

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
    /// [`SessionHandle`] for managing sessions and final subscription streams.
    pub fn into_router_at_with_handle(mut self, path: &str) -> (Router, SessionHandle) {
        let external_rx = self.external_notifications.take();
        let state = self.build_state();
        let handle = SessionHandle {
            store: state.sessions.clone(),
            #[cfg(feature = "stateless")]
            modern_subscriptions: state.modern_subscriptions.clone(),
        };

        spawn_external_notification_fanout(
            external_rx,
            state.sessions.clone(),
            #[cfg(feature = "stateless")]
            state.modern_subscriptions.clone(),
        );

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

    /// Bound how long [`serve_with_shutdown`](Self::serve_with_shutdown)
    /// waits for open connections after the shutdown signal fires.
    ///
    /// By default it waits for all of them, so a request in flight when the
    /// signal arrives is still answered. That is the right default and it
    /// stays the default.
    ///
    /// Set a bound when a connection might not close on its own. An SSE
    /// notification stream is the case that matters here: it is a live
    /// connection until its client hangs up, so an unbounded drain can
    /// outlast the shutdown that started it. Reaching the bound returns
    /// rather than waiting further, which is preferable to never returning.
    /// This mirrors
    /// [`StdioTransport::drain_timeout`](crate::transport::stdio::StdioTransport::drain_timeout).
    ///
    /// Returning does not close the connections that are still open; axum
    /// serves each on its own task. The listener is closed either way, as
    /// soon as the signal fires, so nothing new is accepted while the drain
    /// runs.
    pub fn drain_timeout(mut self, timeout: Duration) -> Self {
        self.drain_timeout = Some(timeout);
        self
    }

    /// Serve the transport on the given address, forever.
    ///
    /// This is a convenience method that creates a TCP listener and serves the transport.
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
    /// use tower_mcp::{HttpTransport, McpRouter};
    ///
    /// # async fn example() -> Result<(), tower_mcp::BoxError> {
    /// HttpTransport::new(McpRouter::new())
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

        tracing::info!("MCP HTTP transport listening on {}", addr);

        let drain_timeout = self.drain_timeout;
        let router = self.into_router();
        crate::transport::graceful::serve_with_shutdown(listener, router, signal, drain_timeout)
            .await
    }

    /// Add the OAuth Protected Resource Metadata well-known route if configured.
    #[cfg(feature = "oauth")]
    fn add_oauth_route(&self, router: Router, _base_path: &str) -> Router {
        if let Some(ref config) = self.oauth_config {
            let metadata = config.metadata.clone();
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
}

/// Check if an origin is a localhost origin (safe from DNS rebinding).
/// Drain a caller-supplied notification channel and fan items out to every
/// live session's SSE broadcast.
///
/// No-op when `rx` is `None`. When present, spawns a long-running task that
/// runs for the lifetime of the transport (until the channel closes).
fn spawn_external_notification_fanout(
    rx: Option<NotificationReceiver>,
    sessions: Arc<SessionRegistry>,
    #[cfg(feature = "stateless")] modern_subscriptions: Arc<ModernSubscriptionRegistry>,
) {
    let Some(mut rx) = rx else {
        return;
    };
    tokio::spawn(async move {
        while let Some(notification) = rx.recv().await {
            #[cfg(feature = "stateless")]
            modern_subscriptions.publish(&notification);
            if let Some(json) = crate::transport::stdio::serialize_notification(&notification) {
                sessions.broadcast_to_all(&json).await;
            }
        }
        tracing::debug!("External notification channel closed; fan-out task exiting");
    });
}

fn is_localhost_origin(origin: &str) -> bool {
    // Parse the origin to extract the host
    if let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    {
        is_localhost_host(rest)
    } else {
        false
    }
}

/// Check if a `host:port` (or `[ipv6]:port`) value refers to localhost.
///
/// Used by both Origin validation (after stripping the `http(s)://` scheme)
/// and Host validation (where there's no scheme to begin with).
fn is_localhost_host(host: &str) -> bool {
    let host_only = if host.starts_with('[') {
        // Bracketed IPv6: [::1]:3000 -> ::1
        host.split(']')
            .next()
            .unwrap_or(host)
            .trim_start_matches('[')
    } else {
        // Strip port if present
        host.split(':').next().unwrap_or(host)
    };
    matches!(host_only, "localhost" | "127.0.0.1" | "::1")
}

/// Resolve the effective host for validation.
///
/// Prefers the `Host` header, falling back to the HTTP/2 `:authority`
/// pseudo-header (`request.uri().authority()`) when the header is missing.
/// This matters behind middleware like `axum::Router::nest`, which can
/// strip Hyper's synthesized `Host` before our handler sees it.
fn effective_host<'a>(headers: &'a HeaderMap, uri: &'a axum::http::Uri) -> Option<&'a str> {
    if let Some(value) = headers.get(header::HOST)
        && let Ok(s) = value.to_str()
    {
        return Some(s);
    }
    uri.authority().map(|a| a.as_str())
}

/// Validate the `Host` header (defense-in-depth alongside Origin).
///
/// Returns Some(Response) if validation fails, None if it passes.
fn validate_host(headers: &HeaderMap, uri: &axum::http::Uri, state: &AppState) -> Option<Response> {
    if !state.validate_host {
        return None;
    }

    let Some(host) = effective_host(headers, uri) else {
        if state.allowed_hosts.is_empty() {
            // No Host header and no allowlist: fall back to permissive
            // behavior matching pre-validation defaults so we don't break
            // existing deployments. (Origin already protects browsers.)
            return None;
        }
        tracing::warn!("Rejecting request: missing Host header and no :authority fallback");
        return Some((StatusCode::BAD_REQUEST, "Missing Host header").into_response());
    };

    if is_localhost_host(host) {
        return None;
    }

    if state.allowed_hosts.is_empty() {
        // Non-localhost host with no explicit allowlist: keep accepting it.
        // Operators who want strict Host validation must opt in via
        // `.allowed_hosts(...)`. This preserves the historical behavior of
        // not enforcing Host on non-loopback deployments by default.
        return None;
    }

    if state.allowed_hosts.iter().any(|h| h == host) {
        return None;
    }

    tracing::warn!(host = %host, "Rejecting request: Host not in allowlist");
    Some((StatusCode::BAD_REQUEST, "Host not allowed").into_response())
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
            tracing::warn!(
                origin = %origin_str,
                "Rejecting request: cross-origin not allowed (no allowlist configured)"
            );
            return Some(
                (StatusCode::FORBIDDEN, "Cross-origin requests not allowed").into_response(),
            );
        }

        if !state
            .allowed_origins
            .iter()
            .any(|o| o == origin_str || o == "*")
        {
            tracing::warn!(origin = %origin_str, "Rejecting request: Origin not in allowlist");
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

/// Resolve the selected tool's input schema when the transport owns an
/// [`McpRouter`]. Pre-built services do not expose their tool registry, so
/// supplied custom headers can still be checked there but missing headers
/// cannot be inferred before dispatch.
fn request_tool_input_schema(
    service_source: &ServiceSource,
    parsed: &serde_json::Value,
) -> Option<serde_json::Value> {
    if parsed.get("method").and_then(serde_json::Value::as_str) != Some("tools/call") {
        return None;
    }
    let name = parsed
        .get("params")
        .and_then(serde_json::Value::as_object)
        .and_then(|params| params.get("name"))
        .and_then(serde_json::Value::as_str)?;
    match service_source {
        ServiceSource::Router { router, .. } => router.tool_input_schema(name),
        ServiceSource::Service(_) => None,
    }
}

/// Return whether an HTTP request claims the modern, per-request-metadata
/// protocol era.
///
/// The body envelope is authoritative for era detection. The final-version
/// header is also treated as a modern claim so a missing or malformed
/// envelope receives the specified modern error instead of drifting into the
/// legacy session path.
fn claims_modern_protocol(headers: &HeaderMap, parsed: &serde_json::Value) -> bool {
    get_protocol_version(headers).as_deref() == Some(PROTOCOL_VERSION_2026_07_28)
        || parsed
            .get("params")
            .and_then(serde_json::Value::as_object)
            .and_then(|params| params.get("_meta"))
            .and_then(serde_json::Value::as_object)
            .is_some_and(|meta| meta.contains_key("io.modelcontextprotocol/protocolVersion"))
}

/// Validate the required modern per-request metadata and return its declared
/// protocol version.
///
/// `clientInfo` is deliberately optional in the final specification.
fn validate_modern_request_meta(
    parsed: &serde_json::Value,
) -> std::result::Result<String, JsonRpcError> {
    let params = parsed
        .get("params")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            JsonRpcError::invalid_params("Modern requests require a params object containing _meta")
        })?;
    let meta_value = params
        .get("_meta")
        .ok_or_else(|| JsonRpcError::invalid_params("Modern requests require a _meta object"))?;
    crate::protocol::validate_meta_object(meta_value)
        .map_err(|error| JsonRpcError::invalid_params(error.to_string()))?;
    let meta = meta_value
        .as_object()
        .expect("validate_meta_object accepted a JSON object");
    let protocol_version = meta
        .get("io.modelcontextprotocol/protocolVersion")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            JsonRpcError::invalid_params(
                "Missing or invalid _meta.io.modelcontextprotocol/protocolVersion",
            )
        })?;
    let client_capabilities = meta
        .get("io.modelcontextprotocol/clientCapabilities")
        .ok_or_else(|| {
            JsonRpcError::invalid_params("Missing _meta.io.modelcontextprotocol/clientCapabilities")
        })?;
    if !client_capabilities.is_object()
        || serde_json::from_value::<ClientCapabilities>(client_capabilities.clone()).is_err()
    {
        return Err(JsonRpcError::invalid_params(
            "Invalid _meta.io.modelcontextprotocol/clientCapabilities",
        ));
    }

    Ok(protocol_version.to_string())
}

/// Methods present in legacy protocol unions but removed from the modern core.
fn is_removed_modern_method(method: &str) -> bool {
    matches!(
        method,
        "initialize"
            | "notifications/initialized"
            | "ping"
            | "logging/setLevel"
            | "resources/subscribe"
            | "resources/unsubscribe"
            | "notifications/roots/list_changed"
    )
}

/// Map protocol errors whose final Streamable HTTP binding assigns a
/// non-success status. Errors emitted after an SSE stream has opened remain
/// in-band because the HTTP status is already committed.
#[cfg(feature = "stateless")]
fn modern_response_status(response: &JsonRpcResponse) -> StatusCode {
    let JsonRpcResponse::Error(error) = response else {
        return StatusCode::OK;
    };
    if error.error.code == ErrorCode::MethodNotFound as i32 {
        StatusCode::NOT_FOUND
    } else if error.error.code == McpErrorCode::MissingRequiredClientCapability.code() {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::OK
    }
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
    let uri = parts.uri.clone();

    // Validate Host (DNS rebinding defense, complement to Origin)
    if let Some(resp) = validate_host(&headers, &uri, &state) {
        return resp;
    }

    // Validate Origin
    if let Some(resp) = validate_origin(&headers, &state) {
        return resp;
    }

    // Bound the body size (rmcp #970 analog). axum's `DefaultBodyLimit`
    // doesn't apply here because this handler consumes the raw `Request`
    // instead of a body-consuming extractor, so this is the only limit on
    // the MCP POST body. A declared Content-Length above the limit is
    // rejected without reading; chunked bodies are capped while streaming.
    if let Some(declared) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        && declared > state.max_body_size
    {
        return body_too_large_response(state.max_body_size);
    }

    let body = match axum::body::to_bytes(body_bytes, state.max_body_size).await {
        Ok(bytes) => match String::from_utf8(bytes.to_vec()) {
            Ok(s) => s,
            Err(e) => {
                return json_rpc_error_response(
                    None,
                    JsonRpcError::parse_error(format!("Invalid UTF-8: {}", e)),
                );
            }
        },
        Err(e) if is_length_limit_error(&e) => {
            return body_too_large_response(state.max_body_size);
        }
        Err(e) => {
            return json_rpc_error_response(
                None,
                JsonRpcError::parse_error(format!("Failed to read body: {}", e)),
            );
        }
    };

    // Per-request data bridged from HTTP into MCP extensions: OAuth claims
    // when that feature is compiled in, plus whatever types the server
    // registered with `bridge_extension`, which is independent of OAuth
    // (#1242). Always bound, since the bridges run in every build.
    let http_extensions = parts.extensions;

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

    // A version header supplies enough exact context to reject a batch before
    // any object-only HTTP classification runs. Legacy batches without a
    // header are validated against their session revision after lookup below.
    if parsed.is_array()
        && let Some(version) = get_protocol_version(&headers)
    {
        let revision = match version.parse::<McpProtocolRevision>() {
            Ok(revision) => revision,
            Err(_) => {
                return json_rpc_error_response(
                    None,
                    JsonRpcError::unsupported_protocol_version(
                        version,
                        state.protocol_support.versions().iter().map(String::as_str),
                    ),
                );
            }
        };
        if let Err(error) = inspect_runtime_value(
            &parsed,
            revision,
            &state.protocol_support,
            McpDirection::ClientToServer,
        ) {
            let status = if revision == McpProtocolRevision::V2026_07_28 {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::OK
            };
            return json_rpc_error_response_with_status(None, error, status);
        }
    }

    // Check if this is an initialize request (creates new session)
    let is_init = is_initialize_request(&parsed);
    let request_method = parsed
        .get("method")
        .and_then(|method| method.as_str())
        .unwrap_or_default()
        .to_string();
    let tool_input_schema = request_tool_input_schema(&state.service_source, &parsed);
    let modern_request = claims_modern_protocol(&headers, &parsed);

    // The modern protocol is selected by its per-request `_meta` envelope,
    // with the final-version HTTP header also acting as a signal for malformed
    // requests whose envelope is missing. Resolve that era before consulting
    // any legacy session state so modern traffic cannot accidentally fall
    // through to the initialize/session lifecycle.
    if modern_request {
        let id = extract_request_id(&parsed);
        let body_version = match validate_modern_request_meta(&parsed) {
            Ok(version) => version,
            Err(error) => {
                return json_rpc_error_response_with_status(id, error, StatusCode::BAD_REQUEST);
            }
        };

        let Some(header_version) = get_protocol_version(&headers) else {
            return json_rpc_error_response_with_status(
                id,
                JsonRpcError::header_mismatch("MCP-Protocol-Version header is required"),
                StatusCode::BAD_REQUEST,
            );
        };
        if header_version != body_version {
            return json_rpc_error_response_with_status(
                id,
                JsonRpcError::header_mismatch(format!(
                    "MCP-Protocol-Version header value {header_version:?} does not match \
                     request _meta protocol version {body_version:?}"
                )),
                StatusCode::BAD_REQUEST,
            );
        }

        if !state.protocol_support.contains(&body_version) {
            return json_rpc_error_response_with_status(
                id,
                JsonRpcError::unsupported_protocol_version(
                    body_version,
                    state.protocol_support.versions().iter().map(String::as_str),
                ),
                StatusCode::BAD_REQUEST,
            );
        }

        let revision = match body_version.parse::<McpProtocolRevision>() {
            Ok(revision) => revision,
            Err(_) => {
                return json_rpc_error_response_with_status(
                    id,
                    JsonRpcError::unsupported_protocol_version(
                        body_version,
                        state.protocol_support.versions().iter().map(String::as_str),
                    ),
                    StatusCode::BAD_REQUEST,
                );
            }
        };
        if let Err(error) = inspect_runtime_value(
            &parsed,
            revision,
            &state.protocol_support,
            McpDirection::ClientToServer,
        ) {
            return json_rpc_error_response_with_status(id, error, StatusCode::BAD_REQUEST);
        }

        let sep_2243_mode = super::http_headers::mode_for_version(&body_version);
        if let Err(error) = super::http_headers::validate_with_tool_schema(
            &headers,
            &parsed,
            sep_2243_mode,
            tool_input_schema.as_ref(),
        ) {
            tracing::warn!(
                mode = ?sep_2243_mode,
                version = %body_version,
                error = %error.message,
                "Rejecting modern request: HTTP header validation failed",
            );
            return json_rpc_error_response_with_status(id, error, StatusCode::BAD_REQUEST);
        }

        if is_removed_modern_method(&request_method) {
            return json_rpc_error_response_with_status(
                id,
                JsonRpcError::method_not_found(&request_method),
                StatusCode::NOT_FOUND,
            );
        }
    }

    // SEP-2575 / SEP-2567: version-gated stateless mode for 2026-07-28+ clients.
    //
    // When the requested (or carried) protocol version is >= 2026-07-28 and the
    // request has no mcp-session-id, every request -- including `initialize` --
    // is served without creating or looking up a session. Each request is fully
    // self-contained; client identity and capabilities flow through per-request
    // `_meta` rather than a session handshake.
    //
    // This block runs before the legacy SEP-1442 stateless path so that
    // 2026-07-28 requests are handled here regardless of whether
    // `stateless_config` is set on the transport.
    #[cfg(feature = "stateless")]
    {
        let version_in_play: Option<String> = if is_init && !modern_request {
            // For `initialize`, read the version the client is requesting from
            // the params object.
            parsed
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        } else {
            // For non-init requests, only the HTTP-level `MCP-Protocol-Version`
            // header gates stateless mode. Body-level `_meta.protocolVersion` is
            // plumbed to handlers via `stash_per_request_meta` in both paths.
            get_protocol_version(&headers)
        };

        if let Some(ref version) = version_in_play
            && is_stateless_protocol_version(version)
            && state.protocol_support.contains(version)
            // `subscriptions/listen` opens an SSE stream; let it fall through to the
            // dedicated intercept below rather than handling it as a plain RPC call.
            && parsed.get("method").and_then(|m| m.as_str()) != Some("subscriptions/listen")
        {
            // Notifications and responses are fire-and-forget; no dispatch needed.
            if !is_init && (parsed.get("id").is_none() || is_response(&parsed)) {
                return StatusCode::ACCEPTED.into_response();
            }

            // SEP-2243 validation before `parsed` is consumed by deserialization.
            // 2026-07-28 falls into strict mode, so missing Mcp-Method is an error.
            let sep_2243_mode = super::http_headers::mode_for_version(version);
            if let Err(err) = super::http_headers::validate_with_tool_schema(
                &headers,
                &parsed,
                sep_2243_mode,
                tool_input_schema.as_ref(),
            ) {
                tracing::warn!(
                    mode = ?sep_2243_mode,
                    version = %version,
                    error = %err.message,
                    "Rejecting stateless request: SEP-2243 header validation failed",
                );
                let id = extract_request_id(&parsed);
                let mut resp = json_rpc_error_response(id, err);
                *resp.status_mut() = StatusCode::BAD_REQUEST;
                return resp;
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

            // Ephemeral pre-initialized service -- no session is stored or created.
            //
            // A per-request notification channel captures anything the handler
            // emits during the call (progress, logging). With no session and no
            // GET stream on this path, those messages can only reach the client
            // on the POST response itself: per the draft Streamable HTTP rules,
            // a plain JSON body is only correct when the first outbound message
            // is the terminal response; otherwise the response falls back to
            // SSE with the notifications delivered ahead of the terminal
            // response.
            // Captured before the match below borrows `router` into the
            // ephemeral session; used to stamp `_meta.serverInfo` on the
            // outgoing response (SEP-2575). `None` for a transport built
            // from a pre-built service (no router to read identity from).
            let server_identity = match &state.service_source {
                ServiceSource::Router { router, .. } if state.stamp_server_info => {
                    Some(router.implementation())
                }
                _ => None,
            };

            let (notif_tx, mut notif_rx) = crate::context::notification_channel(64);
            let mut service = match &state.service_source {
                ServiceSource::Router { router, factory } => {
                    let ephemeral = router
                        .with_fresh_session()
                        .with_request_notification_sender(notif_tx);
                    ephemeral.session().mark_initialized();
                    JsonRpcService::new(factory(ephemeral))
                }
                ServiceSource::Service(mutex) => JsonRpcService::new(mutex.lock().unwrap().clone()),
            };

            let mut ext = crate::router::Extensions::new();
            ext.insert(state.protocol_support.clone());
            #[cfg(feature = "oauth")]
            if let Some(claims) = http_extensions.get::<crate::oauth::token::TokenClaims>() {
                ext.insert(claims.clone());
            }
            stash_per_request_meta(&request, &mut ext);
            crate::transport::extension_bridge::apply_extension_bridges(
                &state.extension_bridges,
                &http_extensions,
                &mut ext,
            );

            // rmcp #967 analog: give the request a cancellation token that
            // fires if the client disconnects before the response is
            // delivered. The router adopts the token as the
            // `RequestContext`'s cancellation source, so handlers observe
            // the disconnect via `ctx.is_cancelled()` / `ctx.cancelled()`,
            // and spawned work holding a token clone is signalled even
            // after the request future itself is dropped. Session-based
            // requests are exempt: with stream resumption, a disconnect is
            // not a cancellation.
            let cancel_token = crate::context::CancellationToken::new();
            let mut cancel_guard = CancelOnDisconnect::arm(cancel_token.clone());
            ext.insert(cancel_token);

            service = service.with_extensions(ext);

            let mut call: std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::error::Result<JsonRpcResponse>> + Send>,
            > = Box::pin(async move {
                let mut service = service;
                service.call_single(request).await
            });

            enum FirstOutbound {
                Response(crate::error::Result<JsonRpcResponse>),
                Notification(crate::context::ServerNotification),
            }

            // Race the handler against its first notification. A closed
            // channel (no sender attached, or all senders dropped) simply
            // awaits the handler.
            let first = loop {
                let outbound = tokio::select! {
                    // A handler may enqueue a notification and complete in
                    // the same poll. Observe the queued notification first so
                    // it is neither dropped nor raced behind the response.
                    biased;
                    maybe = notif_rx.recv() => match maybe {
                        Some(n) => FirstOutbound::Notification(n),
                        None => FirstOutbound::Response((&mut call).await),
                    },
                    result = &mut call => FirstOutbound::Response(result),
                };
                match outbound {
                    FirstOutbound::Notification(notification)
                        if state.modern_subscriptions.publish(&notification) =>
                    {
                        continue;
                    }
                    outbound => break outbound,
                }
            };

            match first {
                FirstOutbound::Response(result) => {
                    // `select!` may observe a handler's ready response in the
                    // same poll that the handler enqueued notifications.
                    // Drain that queue before committing a JSON response.
                    while let Ok(notification) = notif_rx.try_recv() {
                        if state.modern_subscriptions.publish(&notification) {
                            continue;
                        }
                        let ready_call: std::pin::Pin<
                            Box<
                                dyn std::future::Future<
                                        Output = crate::error::Result<JsonRpcResponse>,
                                    > + Send,
                            >,
                        > = Box::pin(async move { result });
                        let mut resp = stateless_sse_with_notifications(
                            notification,
                            ready_call,
                            notif_rx,
                            StatelessSseContext {
                                version: version.clone(),
                                method: request_method.clone(),
                                cancel_guard,
                                server_identity,
                                subscriptions: state.modern_subscriptions.clone(),
                            },
                        );
                        resp.headers_mut().insert(
                            MCP_PROTOCOL_VERSION_HEADER,
                            HeaderValue::from_str(version).unwrap(),
                        );
                        return resp;
                    }

                    // Handler finished; the response is about to be
                    // produced, so dropping the connection from here on is
                    // no longer a cancellation.
                    cancel_guard.disarm();
                    let mut response = match result {
                        Ok(resp) => resp,
                        Err(e) => {
                            return json_rpc_error_response(
                                None,
                                JsonRpcError::internal_error(e.to_string()),
                            );
                        }
                    };

                    // Keep the response aligned with the version selected for
                    // this sessionless request. The router also receives the
                    // runtime allow-list through Extensions.
                    if is_init
                        && let JsonRpcResponse::Result(ref mut result) = response
                        && let Some(pv) = result.result.get_mut("protocolVersion")
                    {
                        *pv = serde_json::Value::String(version.clone());
                    }
                    apply_protocol_result_fields(&mut response, &request_method, version);
                    if let Some(ref identity) = server_identity {
                        stamp_server_info(&mut response, identity);
                    }

                    let status = modern_response_status(&response);
                    let mut resp = if state.sse_responses {
                        sse_json_response(&response)
                    } else {
                        axum::Json(response).into_response()
                    };
                    *resp.status_mut() = status;
                    resp.headers_mut().insert(
                        MCP_PROTOCOL_VERSION_HEADER,
                        HeaderValue::from_str(version).unwrap(),
                    );
                    // Intentionally NO `mcp-session-id` header for 2026-07-28+ clients.
                    return resp;
                }
                FirstOutbound::Notification(first_notif) => {
                    let mut resp = stateless_sse_with_notifications(
                        first_notif,
                        call,
                        notif_rx,
                        StatelessSseContext {
                            version: version.clone(),
                            method: request_method.clone(),
                            cancel_guard,
                            server_identity,
                            subscriptions: state.modern_subscriptions.clone(),
                        },
                    );
                    resp.headers_mut().insert(
                        MCP_PROTOCOL_VERSION_HEADER,
                        HeaderValue::from_str(version).unwrap(),
                    );
                    // Intentionally NO `mcp-session-id` header for 2026-07-28+ clients.
                    return resp;
                }
            }
        }
    }

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

            let mut ext = crate::router::Extensions::new();
            ext.insert(state.protocol_support.clone());
            #[cfg(feature = "oauth")]
            if let Some(claims) = http_extensions.get::<crate::oauth::token::TokenClaims>() {
                ext.insert(claims.clone());
            }
            #[cfg(feature = "stateless")]
            stash_per_request_meta(&request, &mut ext);
            crate::transport::extension_bridge::apply_extension_bridges(
                &state.extension_bridges,
                &http_extensions,
                &mut ext,
            );
            if !ext.is_empty() {
                service = service.with_extensions(ext);
            }

            let mut response = match service.call_single(request).await {
                Ok(resp) => resp,
                Err(e) => {
                    return json_rpc_error_response(
                        None,
                        JsonRpcError::internal_error(e.to_string()),
                    );
                }
            };
            apply_protocol_result_fields(&mut response, &request_method, &version);

            let mut resp = if state.sse_responses {
                sse_json_response(&response)
            } else {
                axum::Json(response).into_response()
            };
            resp.headers_mut().insert(
                MCP_PROTOCOL_VERSION_HEADER,
                HeaderValue::from_str(&version).unwrap(),
            );
            return resp;
        }
    }

    // Final-protocol subscriptions are sessionless long-lived POSTs. They
    // must be established before consulting any legacy session state.
    #[cfg(feature = "stateless")]
    if modern_request && request_method == "subscriptions/listen" {
        return handle_modern_subscriptions_listen_sse(state, &parsed, &http_extensions).await;
    }

    // Runtime allowlist enforcement precedes semantic profile validation.
    // This is especially important for optional-session traffic: an unknown
    // header must not be interpreted under a fallback revision.
    if !is_init
        && let Some(version) = get_protocol_version(&headers)
        && !state.protocol_support.contains(&version)
    {
        return json_rpc_error_response(
            extract_request_id(&parsed),
            JsonRpcError::unsupported_protocol_version(
                version,
                state.protocol_support.versions().iter().map(String::as_str),
            ),
        );
    }

    let uses_transient_session = !is_init
        && !modern_request
        && get_session_id(&headers).is_none()
        && state.optional_sessions;

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
    } else if !modern_request && let Some(session_id) = get_session_id(&headers) {
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

    // Session lookup establishes the exact legacy revision. Validate the raw
    // envelope before object-only notification/response routing, then let the
    // existing request dispatcher consume the typed shape.
    let session_protocol_version = if uses_transient_session {
        let version = state
            .protocol_support
            .versions()
            .iter()
            .find(|version| {
                crate::protocol::SUPPORTED_PROTOCOL_VERSIONS.contains(&version.as_str())
            })
            .map_or_else(
                || state.protocol_support.preferred().to_string(),
                Clone::clone,
            );
        *session.protocol_version.write().await = version.clone();
        version
    } else {
        session.protocol_version.read().await.clone()
    };
    let session_revision = match session_protocol_version.parse::<McpProtocolRevision>() {
        Ok(revision) => revision,
        Err(_) => {
            return json_rpc_error_response(
                extract_request_id(&parsed),
                JsonRpcError::unsupported_protocol_version(
                    session_protocol_version,
                    state.protocol_support.versions().iter().map(String::as_str),
                ),
            );
        }
    };
    if !is_init
        && let Err(error) = inspect_runtime_value(
            &parsed,
            session_revision,
            &state.protocol_support,
            McpDirection::ClientToServer,
        )
    {
        return json_rpc_error_response(extract_request_id(&parsed), error);
    }

    if parsed.is_array() {
        if state.strict_initialization
            && !session
                .initialized_notification_received
                .load(Ordering::Acquire)
        {
            return json_rpc_error_response(
                None,
                JsonRpcError::invalid_request(
                    "Client must send notifications/initialized before making requests",
                ),
            );
        }

        let message: JsonRpcMessage = match serde_json::from_value(parsed) {
            Ok(message) => message,
            Err(error) => {
                return json_rpc_error_response(
                    None,
                    JsonRpcError::invalid_request(format!("Invalid request batch: {error}")),
                );
            }
        };

        let mut extensions = crate::router::Extensions::new();
        extensions.insert(state.protocol_support.clone());
        extensions.insert(session_revision);
        #[cfg(feature = "oauth")]
        if let Some(claims) = http_extensions.get::<crate::oauth::token::TokenClaims>() {
            extensions.insert(claims.clone());
        }
        crate::transport::extension_bridge::apply_extension_bridges(
            &state.extension_bridges,
            &http_extensions,
            &mut extensions,
        );

        let mut service = JsonRpcService::new(session.make_service())
            .with_extensions(extensions)
            .protocol_support(state.protocol_support.clone())
            .with_negotiated_protocol_version(&session_protocol_version);
        let response = match service.call_message(message).await {
            Ok(response) => response,
            Err(error) => {
                return json_rpc_error_response(
                    None,
                    JsonRpcError::internal_error(error.to_string()),
                );
            }
        };
        let mut response = axum::Json(response).into_response();
        response.headers_mut().insert(
            MCP_PROTOCOL_VERSION_HEADER,
            HeaderValue::from_str(&session_protocol_version).unwrap(),
        );
        return response;
    }

    // SEP-2575 / SEP-2567: intercept `subscriptions/listen` before the standard
    // version validation. `subscriptions/listen` is only available when the
    // effective protocol version is >= 2026-07-28; otherwise we return a
    // proper JSON-RPC error rather than silently falling through to the
    // router (which would return `MethodNotFound` anyway, but without the
    // protocol-version context).
    //
    // We check the Mcp-Protocol-Version header first (per-request override),
    // falling back to the session-negotiated version. Intercepting here
    // also prevents the version-validation guard below from rejecting the
    // 2026-07-28 header before we can inspect it.
    {
        let method_str = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");
        if method_str == "subscriptions/listen" {
            let req_id = extract_request_id(&parsed);
            let effective_version = if let Some(v) = get_protocol_version(&headers) {
                v
            } else {
                session.protocol_version.read().await.clone()
            };
            if version_supports_subscriptions_listen(&effective_version, &state.protocol_support) {
                return handle_subscriptions_listen_sse(session).await;
            } else {
                return json_rpc_error_response(
                    req_id,
                    JsonRpcError::method_not_found("subscriptions/listen"),
                );
            }
        }
    }

    // SEP-2243: validate the standardized HTTP headers (Mcp-Method,
    // Mcp-Name, Mcp-Param-*) against the body. Mode is "strict" only
    // when the negotiated protocol version is at or beyond the
    // SEP-2243-inclusion version; otherwise present headers are still
    // checked for body consistency but missing headers are allowed.
    //
    // For `initialize` requests the session's protocol version hasn't
    // been negotiated yet, so we fall back to the version the client
    // requested in the body. For all other requests we use the session's
    // negotiated version (which is also reflected back in the response
    // `Mcp-Protocol-Version` header).
    let sep_2243_version = if is_init {
        match parsed
            .get("params")
            .and_then(|p| p.get("protocolVersion"))
            .and_then(|v| v.as_str())
        {
            Some(v) => v.to_string(),
            None => session.protocol_version.read().await.clone(),
        }
    } else {
        session.protocol_version.read().await.clone()
    };
    let sep_2243_mode = super::http_headers::mode_for_version(&sep_2243_version);
    if let Err(err) = super::http_headers::validate_with_tool_schema(
        &headers,
        &parsed,
        sep_2243_mode,
        tool_input_schema.as_ref(),
    ) {
        tracing::warn!(
            mode = ?sep_2243_mode,
            version = %sep_2243_version,
            error = %err.message,
            "Rejecting request: SEP-2243 header validation failed",
        );
        let id = extract_request_id(&parsed);
        let mut resp = json_rpc_error_response(id, err);
        // Per SEP-2243 §"Error Code" the HTTP status MUST be 400.
        *resp.status_mut() = StatusCode::BAD_REQUEST;
        return resp;
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
            // Per the MCP 2025-11-25 spec, clients MUST send
            // `notifications/initialized` after receiving the `initialize`
            // response and before sending any other requests. Record the
            // receipt so the strict_initialization check below can allow
            // subsequent tool/resource/prompt requests.
            if matches!(&mcp_notification, McpNotification::Initialized) {
                session
                    .initialized_notification_received
                    .store(true, Ordering::Release);
                tracing::debug!(session_id = %session.id, "Received notifications/initialized");
            }
            session.handle_notification(mcp_notification);
        }
        return StatusCode::ACCEPTED.into_response();
    }

    // Enforce `notifications/initialized` before any non-initialize request
    // (MCP 2025-11-25 spec requirement). This only applies to the session-based
    // path; stateless requests (2026-07-28) are handled above and never reach here.
    if !is_init
        && state.strict_initialization
        && !session
            .initialized_notification_received
            .load(Ordering::Acquire)
    {
        let id = extract_request_id(&parsed);
        tracing::warn!(
            session_id = %session.id,
            "Rejecting request: notifications/initialized not yet received"
        );
        return json_rpc_error_response(
            id,
            JsonRpcError::invalid_request(
                "Client must send notifications/initialized before making requests",
            ),
        );
    }

    // For initialize requests, capture the advertised client info /
    // capabilities from the raw params before `parsed` is consumed by
    // deserialization. These are stashed onto the live `Session` after a
    // successful initialize so the persisted SessionRecord faithfully
    // describes the client (rather than carrying the defaults set at
    // session-create time).
    let init_client_metadata: Option<(Option<Implementation>, Option<ClientCapabilities>)> =
        if is_init {
            let params = parsed.get("params");
            let client_info = params
                .and_then(|p| p.get("clientInfo"))
                .and_then(|v| serde_json::from_value::<Implementation>(v.clone()).ok());
            let client_capabilities = params
                .and_then(|p| p.get("capabilities"))
                .and_then(|v| serde_json::from_value::<ClientCapabilities>(v.clone()).ok());
            Some((client_info, client_capabilities))
        } else {
            None
        };

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

    // Bridge per-request data from HTTP into MCP Extensions: OAuth claims,
    // SEP-2575 `_meta` (clientInfo, clientCapabilities, etc.). Empty ext is
    // skipped to avoid pointless allocation.
    #[allow(unused_mut)]
    let mut ext = crate::router::Extensions::new();
    ext.insert(state.protocol_support.clone());
    ext.insert(session_revision);
    #[cfg(feature = "oauth")]
    if let Some(claims) = http_extensions.get::<crate::oauth::token::TokenClaims>() {
        ext.insert(claims.clone());
    }
    crate::transport::extension_bridge::apply_extension_bridges(
        &state.extension_bridges,
        &http_extensions,
        &mut ext,
    );
    #[cfg(feature = "stateless")]
    stash_per_request_meta(&request, &mut ext);

    // SEP-2260: legacy server-to-client requests are associated with the
    // client POST that caused them. Give this request its own channel while
    // drawing IDs from the session-wide allocator so concurrent POSTs cannot
    // collide or leak requests onto one another's response streams.
    let mut associated_request_rx = if !is_init {
        session.request_id_allocator.as_ref().map(|next_id| {
            let (request_tx, request_rx) = outgoing_request_channel(32);
            let requester: ClientRequesterHandle = Arc::new(
                ChannelClientRequester::with_id_allocator(request_tx, next_id.clone()),
            );
            ext.insert(requester);
            request_rx
        })
    } else {
        None
    };

    if !ext.is_empty() {
        service = service.with_extensions(ext);
    }

    let request_id = request.id.clone();
    let mut call: AssociatedCall = Box::pin(async move { service.call_single(request).await });
    let mut response = if let Some(mut request_rx) = associated_request_rx.take() {
        tokio::select! {
            result = &mut call => match result {
                Ok(response) => response,
                Err(error) => {
                    return json_rpc_error_response(
                        Some(request_id),
                        JsonRpcError::internal_error(error.to_string()),
                    );
                }
            },
            outgoing = request_rx.recv() => {
                match outgoing {
                    Some(outgoing) => {
                        let negotiated_version = session.protocol_version.read().await.clone();
                        return associated_request_sse_response(
                            session,
                            call,
                            request_rx,
                            outgoing,
                            request_id,
                            request_method,
                            negotiated_version,
                        );
                    }
                    None => match call.await {
                        Ok(response) => response,
                        Err(error) => {
                            return json_rpc_error_response(
                                Some(request_id),
                                JsonRpcError::internal_error(error.to_string()),
                            );
                        }
                    },
                }
            }
        }
    } else {
        match call.await {
            Ok(response) => response,
            Err(error) => {
                return json_rpc_error_response(
                    Some(request_id),
                    JsonRpcError::internal_error(error.to_string()),
                );
            }
        }
    };

    // For successful initialize responses, extract and store the negotiated
    // protocol version, stash the client's advertised identity / capabilities
    // on the live session, and persist the now-complete record to the session
    // store so a restore from a peer instance sees the original client info
    // instead of defaults.
    if is_init && let JsonRpcResponse::Result(ref result) = response {
        if let Some(version) = result
            .result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
        {
            *session.protocol_version.write().await = version.to_string();
        }
        if let Some((client_info, client_capabilities)) = init_client_metadata {
            *session.client_info.write().await = client_info;
            *session.client_capabilities.write().await = client_capabilities;
        }
        state.sessions.save_record(&session).await;
    }

    let negotiated_version = session.protocol_version.read().await.clone();
    let response_version = if request_method == "server/discover"
        && state.protocol_support.contains(PROTOCOL_VERSION_2026_07_28)
    {
        PROTOCOL_VERSION_2026_07_28
    } else {
        &negotiated_version
    };
    apply_protocol_result_fields(&mut response, &request_method, response_version);

    // Build response with headers
    let mut resp = if state.sse_responses {
        sse_json_response(&response)
    } else {
        axum::Json(response).into_response()
    };

    if is_init {
        resp.headers_mut().insert(
            MCP_SESSION_ID_HEADER,
            HeaderValue::from_str(&session.id).unwrap(),
        );
    }

    // Always include the negotiated protocol version header
    resp.headers_mut().insert(
        MCP_PROTOCOL_VERSION_HEADER,
        HeaderValue::from_str(&negotiated_version).unwrap(),
    );

    resp
}

/// Keep legacy server-to-client requests on the POST response stream that
/// caused them. These events deliberately have no SSE IDs and are not written
/// to the session event store: their response channels only exist on this
/// process and replaying them on another connection would break association.
fn associated_request_sse_response(
    session: Arc<Session>,
    mut call: AssociatedCall,
    mut request_rx: OutgoingRequestReceiver,
    first_outgoing: OutgoingRequest,
    original_request_id: RequestId,
    request_method: String,
    negotiated_version: String,
) -> Response {
    let (event_tx, event_rx) =
        tokio::sync::mpsc::channel::<std::result::Result<Event, Infallible>>(32);
    let call_version = negotiated_version.clone();

    tokio::spawn(async move {
        let mut pending_ids = Vec::new();
        if !send_associated_request(&session, &event_tx, first_outgoing, &mut pending_ids).await {
            session
                .fail_pending_requests(
                    &pending_ids,
                    "originating POST disconnected before the client request was delivered",
                )
                .await;
            return;
        }

        let mut requests_open = true;
        loop {
            tokio::select! {
                _ = event_tx.closed() => {
                    session
                        .fail_pending_requests(
                            &pending_ids,
                            "originating POST response stream disconnected",
                        )
                        .await;
                    return;
                }
                result = &mut call => {
                    session
                        .fail_pending_requests(
                            &pending_ids,
                            "originating POST completed before the client request response arrived",
                        )
                        .await;

                    let mut response = match result {
                        Ok(response) => response,
                        Err(error) => JsonRpcResponse::error(
                            Some(original_request_id),
                            JsonRpcError::internal_error(error.to_string()),
                        ),
                    };
                    apply_protocol_result_fields(
                        &mut response,
                        &request_method,
                        &call_version,
                    );

                    match serde_json::to_string(&response) {
                        Ok(data) => {
                            let _ = event_tx
                                .send(Ok(
                                    Event::default()
                                        .event(SSE_MESSAGE_EVENT)
                                        .data(data),
                                ))
                                .await;
                        }
                        Err(error) => {
                            tracing::error!(
                                error = %error,
                                "Failed to serialize associated POST response",
                            );
                        }
                    }
                    return;
                }
                outgoing = request_rx.recv(), if requests_open => {
                    match outgoing {
                        Some(outgoing) => {
                            if !send_associated_request(
                                &session,
                                &event_tx,
                                outgoing,
                                &mut pending_ids,
                            )
                            .await
                            {
                                session
                                    .fail_pending_requests(
                                        &pending_ids,
                                        "originating POST disconnected before the client request was delivered",
                                    )
                                    .await;
                                return;
                            }
                        }
                        None => requests_open = false,
                    }
                }
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(event_rx);
    let mut response = Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(30))
                .text("ping"),
        )
        .into_response();
    response.headers_mut().insert(
        MCP_PROTOCOL_VERSION_HEADER,
        HeaderValue::from_str(&negotiated_version).unwrap(),
    );
    response
}

async fn send_associated_request(
    session: &Session,
    event_tx: &tokio::sync::mpsc::Sender<std::result::Result<Event, Infallible>>,
    outgoing: OutgoingRequest,
    pending_ids: &mut Vec<RequestId>,
) -> bool {
    let id = outgoing.id.clone();
    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: id.clone(),
        method: outgoing.method,
        params: Some(outgoing.params),
    };
    let data = match serde_json::to_string(&request) {
        Ok(data) => data,
        Err(error) => {
            let _ = outgoing.response_tx.send(Err(Error::Internal(format!(
                "Failed to serialize associated client request: {error}"
            ))));
            return true;
        }
    };

    session
        .add_pending_request(id.clone(), outgoing.response_tx)
        .await;
    pending_ids.push(id);

    event_tx
        .send(Ok(Event::default().event(SSE_MESSAGE_EVENT).data(data)))
        .await
        .is_ok()
}

/// Returns `true` when the given protocol version string enables `subscriptions/listen`.
///
/// `subscriptions/listen` is part of the 2026-07-28 spec (SEP-2575 / SEP-2567).
/// Unknown future dates do not opt into behavior that has not been compiled
/// and explicitly enabled.
fn version_supports_subscriptions_listen(
    version: &str,
    protocol_support: &ProtocolSupport,
) -> bool {
    version == PROTOCOL_VERSION_2026_07_28 && protocol_support.contains(version)
}

/// Returns `true` when the given protocol version string enables stateless
/// (sessionless) mode for the HTTP transport.
///
/// Stateless mode is introduced in the 2026-07-28 protocol (SEP-2575 /
/// SEP-2567). Only the exact, compiled-and-enabled version opts in; unknown
/// future dates must not silently inherit revision-specific behavior.
#[cfg(feature = "stateless")]
fn is_stateless_protocol_version(version: &str) -> bool {
    version == PROTOCOL_VERSION_2026_07_28
}

/// Stamp `_meta["io.modelcontextprotocol/serverInfo"]` onto a successful
/// response, per SEP-2575: servers SHOULD identify themselves in each
/// result's `_meta` unless configured not to (see
/// [`HttpTransport::stamp_server_info()`]).
///
/// A no-op for error responses, and for any result whose top-level JSON
/// value isn't an object (defensive; every `McpResponse` variant serializes
/// to an object).
#[cfg(feature = "stateless")]
fn stamp_server_info(response: &mut JsonRpcResponse, implementation: &Implementation) {
    let JsonRpcResponse::Result(result) = response else {
        return;
    };
    let Some(obj) = result.result.as_object_mut() else {
        return;
    };
    let meta = obj
        .entry("_meta")
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    let Some(meta_obj) = meta.as_object_mut() else {
        return;
    };
    if let Ok(value) = serde_json::to_value(implementation) {
        meta_obj.insert("io.modelcontextprotocol/serverInfo".to_string(), value);
    }
}

/// Drop guard that cancels a per-request [`CancellationToken`] when the
/// request is abandoned before its response is produced.
///
/// On the sessionless POST path the response future (plain JSON) or the SSE
/// response stream is dropped when the client disconnects; holding this
/// guard in that future/stream turns the drop into a cancellation signal.
/// [`disarm`](Self::disarm) once the handler's terminal response resolves
/// so normal completion doesn't signal cancellation.
#[cfg(feature = "stateless")]
struct CancelOnDisconnect(Option<crate::context::CancellationToken>);

#[cfg(feature = "stateless")]
impl CancelOnDisconnect {
    fn arm(token: crate::context::CancellationToken) -> Self {
        Self(Some(token))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

#[cfg(feature = "stateless")]
impl Drop for CancelOnDisconnect {
    fn drop(&mut self) {
        if let Some(token) = self.0.take() {
            token.cancel();
        }
    }
}

/// Stream a sessionless POST response as SSE: the notifications the handler
/// emitted, in order, followed by the terminal JSON-RPC response.
///
/// Invoked when a handler produced a notification before its terminal
/// response on the 2026-07-28 sessionless path. A plain JSON body would drop
/// those notifications (there is no session stream to carry them), so the
/// response falls back to `text/event-stream`: the buffered first
/// notification, any further notifications as they arrive, and finally the
/// terminal response, after which the stream ends.
#[cfg(feature = "stateless")]
struct StatelessSseContext {
    version: String,
    method: String,
    cancel_guard: CancelOnDisconnect,
    server_identity: Option<Implementation>,
    subscriptions: Arc<ModernSubscriptionRegistry>,
}

#[cfg(feature = "stateless")]
fn stateless_sse_with_notifications(
    first: crate::context::ServerNotification,
    call: std::pin::Pin<
        Box<dyn std::future::Future<Output = crate::error::Result<JsonRpcResponse>> + Send>,
    >,
    rx: crate::context::NotificationReceiver,
    request: StatelessSseContext,
) -> Response {
    struct Ctx {
        call: Option<
            std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::error::Result<JsonRpcResponse>> + Send>,
            >,
        >,
        rx: crate::context::NotificationReceiver,
        rx_open: bool,
        queue: std::collections::VecDeque<String>,
        terminal: Option<String>,
        version: String,
        method: String,
        /// Cancels the per-request token if the client disconnects (the
        /// stream, and with it this state, is dropped) while the handler
        /// is still in flight. Disarmed once the handler resolves.
        cancel_guard: CancelOnDisconnect,
        /// Stamped into `_meta.serverInfo` on the terminal response, if set
        /// (see [`HttpTransport::stamp_server_info()`]).
        server_identity: Option<Implementation>,
        subscriptions: Arc<ModernSubscriptionRegistry>,
    }

    let mut queue = std::collections::VecDeque::new();
    if !request.subscriptions.publish(&first)
        && let Some(json) = crate::transport::stdio::serialize_notification(&first)
    {
        queue.push_back(json);
    }
    let ctx = Ctx {
        call: Some(call),
        rx,
        rx_open: true,
        queue,
        terminal: None,
        version: request.version,
        method: request.method,
        cancel_guard: request.cancel_guard,
        server_identity: request.server_identity,
        subscriptions: request.subscriptions,
    };

    let stream = futures::stream::unfold(ctx, |mut ctx| async move {
        loop {
            // Buffered notifications flush first to preserve emission order.
            if let Some(json) = ctx.queue.pop_front() {
                return Some((
                    Ok::<_, Infallible>(Event::default().event(SSE_MESSAGE_EVENT).data(json)),
                    ctx,
                ));
            }
            // The terminal response is the last event on the stream.
            if let Some(json) = ctx.terminal.take() {
                return Some((
                    Ok(Event::default().event(SSE_MESSAGE_EVENT).data(json)),
                    ctx,
                ));
            }
            let mut call = ctx.call.take()?;
            tokio::select! {
                result = &mut call => {
                    // Handler finished; a later disconnect is no longer a
                    // cancellation.
                    ctx.cancel_guard.disarm();
                    // Drain notifications that were queued before the handler
                    // finished so they precede the terminal response.
                    while let Ok(n) = ctx.rx.try_recv() {
                        if !ctx.subscriptions.publish(&n)
                            && let Some(json) =
                                crate::transport::stdio::serialize_notification(&n)
                        {
                            ctx.queue.push_back(json);
                        }
                    }
                    let terminal_json = match result {
                        Ok(mut response) => {
                            // Same initialize version patch as the JSON path.
                            if ctx.method == "initialize"
                                && let JsonRpcResponse::Result(ref mut r) = response
                                && let Some(pv) = r.result.get_mut("protocolVersion")
                            {
                                *pv = serde_json::Value::String(ctx.version.clone());
                            }
                            apply_protocol_result_fields(
                                &mut response,
                                &ctx.method,
                                &ctx.version,
                            );
                            if let Some(ref identity) = ctx.server_identity {
                                stamp_server_info(&mut response, identity);
                            }
                            serde_json::to_string(&response).ok()
                        }
                        Err(e) => Some(
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": serde_json::Value::Null,
                                "error": JsonRpcError::internal_error(e.to_string()),
                            })
                            .to_string(),
                        ),
                    };
                    ctx.terminal = terminal_json;
                    // `call` is complete and intentionally not restored.
                }
                maybe = ctx.rx.recv(), if ctx.rx_open => {
                    match maybe {
                        Some(n) => {
                            if !ctx.subscriptions.publish(&n)
                                && let Some(json) =
                                    crate::transport::stdio::serialize_notification(&n)
                            {
                                ctx.queue.push_back(json);
                            }
                        }
                        None => ctx.rx_open = false,
                    }
                    ctx.call = Some(call);
                }
            }
        }
    });

    Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(30))
                .text("ping"),
        )
        .into_response()
}

/// Serve the final, sessionless `subscriptions/listen` protocol over its
/// owning POST response.
#[cfg(feature = "stateless")]
async fn handle_modern_subscriptions_listen_sse(
    state: Arc<AppState>,
    parsed: &serde_json::Value,
    http_extensions: &axum::http::Extensions,
) -> Response {
    let id = extract_request_id(parsed);
    let Some(subscription_id) = id.clone() else {
        return json_rpc_error_response_with_status(
            None,
            JsonRpcError::invalid_request("subscriptions/listen requires a request id"),
            StatusCode::BAD_REQUEST,
        );
    };
    let request: JsonRpcRequest = match serde_json::from_value(parsed.clone()) {
        Ok(request) => request,
        Err(error) => {
            return json_rpc_error_response_with_status(
                id,
                JsonRpcError::invalid_request(format!("Invalid request: {error}")),
                StatusCode::BAD_REQUEST,
            );
        }
    };

    // Dispatch the request through the per-request service before upgrading,
    // so `Service<RouterRequest>` middleware observes accepted and rejected
    // listens and the router owns validation and filter negotiation (#1182).
    // The service response never reaches the wire: an error is returned as
    // the reply, and a success carries the accepted filter this handler
    // consumes to register the stream. The stream lifetime stays entirely
    // transport-owned.
    let service = match &state.service_source {
        ServiceSource::Router { router, factory } => {
            let ephemeral = router.with_fresh_session();
            ephemeral.session().mark_initialized();
            JsonRpcService::new(factory(ephemeral))
        }
        ServiceSource::Service(mutex) => JsonRpcService::new(mutex.lock().unwrap().clone()),
    };
    let mut ext = crate::router::Extensions::new();
    ext.insert(state.protocol_support.clone());
    stash_per_request_meta(&request, &mut ext);
    crate::transport::extension_bridge::apply_extension_bridges(
        &state.extension_bridges,
        http_extensions,
        &mut ext,
    );
    if ext
        .get::<crate::stateless::StatelessRequestMeta>()
        .is_none()
    {
        // This handler is only reached for effective-final requests, but the
        // version can arrive via the HTTP header with no per-request `_meta`.
        // Seed the meta so the router classifies the request correctly.
        ext.insert(crate::stateless::StatelessRequestMeta {
            protocol_version: Some(crate::protocol::PROTOCOL_VERSION_2026_07_28.to_string()),
            ..Default::default()
        });
    }
    let mut service = service.with_extensions(ext);

    let response = match service.call_single(request).await {
        Ok(response) => response,
        Err(error) => {
            return json_rpc_error_response_with_status(
                id,
                JsonRpcError::internal_error(error.to_string()),
                StatusCode::INTERNAL_SERVER_ERROR,
            );
        }
    };
    let accepted = match &response {
        JsonRpcResponse::Result(result) => match result
            .result
            .get("notifications")
            .cloned()
            .map(serde_json::from_value::<SubscriptionFilter>)
        {
            Some(Ok(accepted)) => accepted,
            _ => {
                return json_rpc_error_response_with_status(
                    id,
                    JsonRpcError::internal_error(
                        "subscriptions/listen produced an unrecognized service result",
                    ),
                    StatusCode::INTERNAL_SERVER_ERROR,
                );
            }
        },
        _ => {
            // Rejected: middleware already observed the error; reply with it.
            let mut resp = axum::Json(&response).into_response();
            *resp.status_mut() = StatusCode::BAD_REQUEST;
            return resp;
        }
    };

    let (rx, guard) = state
        .modern_subscriptions
        .register(subscription_id.clone(), accepted.clone());
    let acknowledgment = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/subscriptions/acknowledged",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/subscriptionId": subscription_id
            },
            "notifications": accepted
        }
    })
    .to_string();

    struct ModernListenStream {
        first: Option<String>,
        rx: mpsc::UnboundedReceiver<String>,
        _guard: ModernSubscriptionGuard,
    }

    let stream = futures::stream::unfold(
        ModernListenStream {
            first: Some(acknowledgment),
            rx,
            _guard: guard,
        },
        |mut state| async move {
            let message = match state.first.take() {
                Some(first) => Some(first),
                None => state.rx.recv().await,
            }?;
            Some((
                Ok::<_, Infallible>(Event::default().event(SSE_MESSAGE_EVENT).data(message)),
                state,
            ))
        },
    );

    let mut response = Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(30))
                .text("ping"),
        )
        .into_response();
    response.headers_mut().insert(
        MCP_PROTOCOL_VERSION_HEADER,
        HeaderValue::from_static(PROTOCOL_VERSION_2026_07_28),
    );
    response
}

/// Serve a `subscriptions/listen` request as an SSE stream.
///
/// Subscribes to the session's notification broadcast channel and returns a
/// streaming `text/event-stream` response. The stream closes naturally when:
/// - The client disconnects (axum drops the response body).
/// - The broadcast channel closes (server shutdown / session expiry).
///
/// Each notification is assigned a monotonically increasing event ID for
/// potential stream resumption (SEP-1699).
async fn handle_subscriptions_listen_sse(session: Arc<Session>) -> Response {
    let rx = session.notifications_tx.subscribe();
    let session_clone = session.clone();

    let stream = BroadcastStream::new(rx)
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

    Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(30))
                .text("ping"),
        )
        .into_response()
}

/// Handle GET requests (SSE stream for server notifications and outgoing requests)
async fn handle_get(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    let (parts, _body) = request.into_parts();
    let headers = parts.headers;
    let uri = parts.uri.clone();

    // Validate Host (DNS rebinding defense, complement to Origin)
    if let Some(resp) = validate_host(&headers, &uri, &state) {
        return resp;
    }

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

    // GET is the resumable notification stream. Restricted server-to-client
    // requests are emitted only on their originating POST response stream.
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

/// Handle DELETE requests (session termination)
async fn handle_delete(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    let (parts, _body) = request.into_parts();
    let headers = parts.headers;
    let uri = parts.uri.clone();

    // Validate Host (DNS rebinding defense, complement to Origin)
    if let Some(resp) = validate_host(&headers, &uri, &state) {
        return resp;
    }

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

/// Build a synchronous JSON-RPC response wrapped in SSE format.
///
/// Used when [`AppState::sse_responses`] is `true`. The body is a single SSE
/// event followed by the required blank line:
///
/// ```text
/// event: message
/// data: <json>
///
/// ```
fn sse_json_response(response: impl serde::Serialize) -> Response {
    let json = match serde_json::to_string(&response) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "Failed to serialize response for SSE wrapping");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let sse_body = format!("event: message\ndata: {json}\n\n");
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        sse_body,
    )
        .into_response()
}

/// Create a JSON-RPC error response
fn json_rpc_error_response(
    id: Option<crate::protocol::RequestId>,
    error: JsonRpcError,
) -> Response {
    let response = JsonRpcResponse::error(id, error);
    axum::Json(response).into_response()
}

fn json_rpc_error_response_with_status(
    id: Option<crate::protocol::RequestId>,
    error: JsonRpcError,
    status: StatusCode,
) -> Response {
    let mut response = json_rpc_error_response(id, error);
    *response.status_mut() = status;
    response
}

/// HTTP 413 response for a POST body exceeding [`HttpTransport::max_body_size`].
fn body_too_large_response(limit: usize) -> Response {
    let mut resp = json_rpc_error_response(
        None,
        JsonRpcError::invalid_request(format!(
            "Request body exceeds the maximum size of {} bytes",
            limit
        )),
    );
    *resp.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
    resp
}

/// Returns `true` when the body-read error was caused by exceeding the
/// configured length limit (as opposed to a transport-level I/O failure).
fn is_length_limit_error(err: &axum::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = source {
        if e.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        source = e.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use proptest::prelude::*;
    use tower::ServiceExt;

    #[cfg(feature = "oauth")]
    fn oauth_test_token(audience: &str, scope: &str) -> String {
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &serde_json::json!({
                "sub": "test-user",
                "aud": audience,
                "scope": scope,
            }),
            &jsonwebtoken::EncodingKey::from_secret(b"resource-server-test-secret"),
        )
        .unwrap()
    }

    #[cfg(feature = "oauth")]
    #[tokio::test]
    async fn oauth_resource_server_setup_is_path_aware_and_audience_bound() {
        let resource = "http://localhost:3000/tenant/mcp";
        let metadata = crate::oauth::ProtectedResourceMetadata::new(resource)
            .authorization_server("https://auth.example.com")
            .scope("mcp:read");
        let validator = crate::oauth::JwtValidator::from_secret(b"resource-server-test-secret")
            .disable_exp_validation();
        let app = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .disable_host_validation()
            .into_oauth_router_at(
                "/tenant/mcp",
                validator,
                metadata,
                crate::oauth::ScopePolicy::new().default_scope("mcp:read"),
            )
            .unwrap();

        let metadata_request = Request::builder()
            .uri("/.well-known/oauth-protected-resource/tenant/mcp")
            .body(Body::empty())
            .unwrap();
        let metadata_response = app.clone().oneshot(metadata_request).await.unwrap();
        assert_eq!(metadata_response.status(), StatusCode::OK);

        let unauthenticated = Request::builder()
            .method("POST")
            .uri("/tenant/mcp")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(unauthenticated).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            response
                .headers()
                .get("WWW-Authenticate")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("http://localhost:3000/.well-known/oauth-protected-resource/tenant/mcp")
        );

        let wrong_audience = oauth_test_token("http://localhost:3000/other", "mcp:read");
        let request = Request::builder()
            .method("POST")
            .uri("/tenant/mcp")
            .header("Authorization", format!("Bearer {wrong_audience}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let token = oauth_test_token(resource, "mcp:read");
        let request = Request::builder()
            .method("POST")
            .uri("/tenant/mcp")
            .header("Authorization", format!("Bearer {token}"))
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
                        "clientInfo": { "name": "oauth-test", "version": "1.0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("result").is_some(), "unexpected response: {json}");
    }

    #[cfg(feature = "oauth")]
    #[test]
    fn oauth_resource_server_setup_validates_metadata() {
        let result = HttpTransport::new(create_test_router()).into_oauth_router(
            crate::oauth::JwtValidator::from_secret(b"secret"),
            crate::oauth::ProtectedResourceMetadata::new("https://mcp.example.com"),
            crate::oauth::ScopePolicy::new(),
        );
        assert!(matches!(
            result.unwrap_err(),
            crate::oauth::ProtectedResourceMetadataError::MissingAuthorizationServer
        ));
    }

    fn arb_json() -> impl Strategy<Value = serde_json::Value> {
        let leaf = prop_oneof![
            Just(serde_json::Value::Null),
            any::<bool>().prop_map(serde_json::Value::Bool),
            any::<i64>().prop_map(|number| serde_json::json!(number)),
            prop::collection::vec(any::<char>(), 0..256)
                .prop_map(|chars| serde_json::Value::String(chars.into_iter().collect())),
        ];
        leaf.prop_recursive(6, 128, 10, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..10).prop_map(serde_json::Value::Array),
                prop::collection::hash_map("[a-zA-Z0-9_]{0,24}", inner, 0..10)
                    .prop_map(|map| serde_json::Value::Object(map.into_iter().collect())),
            ]
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Request-ID extraction runs on untrusted JSON before dispatch and
        /// must reject surprising shapes without panicking.
        #[test]
        fn extract_request_id_never_panics(value in arb_json()) {
            let _ = extract_request_id(&value);
        }

        #[test]
        fn extract_request_id_accepts_all_i64_values(id in any::<i64>()) {
            prop_assert_eq!(
                extract_request_id(&serde_json::json!({ "id": id })),
                Some(RequestId::Number(id))
            );
        }

        #[test]
        fn extract_request_id_accepts_arbitrary_strings(
            chars in prop::collection::vec(any::<char>(), 0..1024)
        ) {
            let id: String = chars.into_iter().collect();
            prop_assert_eq!(
                extract_request_id(&serde_json::json!({ "id": id })),
                Some(RequestId::String(id))
            );
        }
    }

    fn create_test_router() -> McpRouter {
        McpRouter::new().server_info("test-server", "1.0.0")
    }

    #[test]
    fn final_result_fields_are_method_and_version_aware() {
        for method in [
            "server/discover",
            "tools/list",
            "prompts/list",
            "resources/list",
            "resources/read",
            "resources/templates/list",
        ] {
            let mut response =
                JsonRpcResponse::result(RequestId::Number(1), serde_json::json!({"value": true}));
            apply_protocol_result_fields(&mut response, method, PROTOCOL_VERSION_2026_07_28);
            let json = serde_json::to_value(response).unwrap();
            assert_eq!(json["result"]["resultType"], "complete", "{method}");
            assert_eq!(json["result"]["ttlMs"], 0, "{method}");
            assert_eq!(json["result"]["cacheScope"], "private", "{method}");
        }

        let mut ordinary =
            JsonRpcResponse::result(RequestId::Number(1), serde_json::json!({"content": []}));
        apply_protocol_result_fields(&mut ordinary, "tools/call", PROTOCOL_VERSION_2026_07_28);
        let json = serde_json::to_value(ordinary).unwrap();
        assert_eq!(json["result"]["resultType"], "complete");
        assert!(json["result"].get("ttlMs").is_none());
        assert!(json["result"].get("cacheScope").is_none());
    }

    #[test]
    fn final_result_fields_preserve_explicit_values_and_legacy_wire_shape() {
        let explicit = serde_json::json!({
            "contents": [],
            "ttlMs": 42,
            "cacheScope": "public"
        });
        let mut response = JsonRpcResponse::result(RequestId::Number(1), explicit.clone());
        apply_protocol_result_fields(&mut response, "resources/read", PROTOCOL_VERSION_2026_07_28);
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["result"]["ttlMs"], 42);
        assert_eq!(json["result"]["cacheScope"], "public");

        for discriminator in ["input_required", "task"] {
            let mut response = JsonRpcResponse::result(
                RequestId::Number(1),
                serde_json::json!({"resultType": discriminator}),
            );
            apply_protocol_result_fields(&mut response, "tools/call", PROTOCOL_VERSION_2026_07_28);
            let json = serde_json::to_value(response).unwrap();
            assert_eq!(json["result"]["resultType"], discriminator);
        }

        let mut legacy = JsonRpcResponse::result(RequestId::Number(1), explicit);
        let before = serde_json::to_value(&legacy).unwrap();
        apply_protocol_result_fields(&mut legacy, "resources/read", "2025-11-25");
        assert_eq!(serde_json::to_value(legacy).unwrap(), before);
    }

    #[tokio::test]
    #[cfg(feature = "stateless")]
    async fn modern_subscription_registry_reports_closes() {
        use crate::transport::subscriptions::{
            SubscriptionClose, SubscriptionCloseReason, SubscriptionObserver,
        };

        #[derive(Default)]
        struct RecordingObserver {
            closes: std::sync::Mutex<Vec<SubscriptionClose>>,
        }
        impl SubscriptionObserver for RecordingObserver {
            fn on_close(&self, close: SubscriptionClose) {
                self.closes.lock().unwrap().push(close);
            }
        }

        let observer = Arc::new(RecordingObserver::default());
        let registry = Arc::new(ModernSubscriptionRegistry::new(
            None,
            Some(observer.clone()),
        ));

        // A dropped guard is a client disconnect.
        let (_rx, guard) = registry.register(
            RequestId::String("disconnects".into()),
            SubscriptionFilter {
                tools_list_changed: Some(true),
                ..SubscriptionFilter::default()
            },
        );
        drop(guard);

        // A drained registry is a graceful server-side close.
        let (_rx2, guard2) = registry.register(
            RequestId::String("drains".into()),
            SubscriptionFilter::default(),
        );
        assert_eq!(registry.close_all(), 1);
        // The entry is already gone, so the later guard drop must not
        // produce a second record.
        drop(guard2);

        let closes = observer.closes.lock().unwrap();
        assert_eq!(closes.len(), 2, "one record per stream: {closes:?}");
        assert_eq!(
            closes[0].subscription_id,
            RequestId::String("disconnects".into())
        );
        assert_eq!(closes[0].reason, SubscriptionCloseReason::Disconnected);
        assert_eq!(
            closes[1].subscription_id,
            RequestId::String("drains".into())
        );
        assert_eq!(closes[1].reason, SubscriptionCloseReason::Drained);
    }

    #[tokio::test]
    #[cfg(feature = "stateless")]
    async fn modern_subscription_registry_filters_and_tags_notifications() {
        let registry = Arc::new(ModernSubscriptionRegistry::default());
        let (mut rx, guard) = registry.register(
            RequestId::String("listen-1".to_string()),
            SubscriptionFilter {
                tools_list_changed: Some(true),
                ..SubscriptionFilter::default()
            },
        );

        assert!(registry.publish(&ServerNotification::PromptsListChanged));
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        assert!(registry.publish(&ServerNotification::ToolsListChanged));
        let message = rx.recv().await.expect("matching notification");
        let json: serde_json::Value = serde_json::from_str(&message).unwrap();
        assert_eq!(json["method"], "notifications/tools/list_changed");
        assert_eq!(
            json["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
            "listen-1"
        );

        drop(guard);
        assert!(registry.subscriptions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn runtime_protocol_allowlist_drives_discovery() {
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .protocol_versions(["2025-03-26"])
            .unwrap();
        let app = transport.into_router();
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "server/discover"
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["result"]["supportedVersions"],
            serde_json::json!(["2025-03-26"])
        );
    }

    #[tokio::test]
    async fn test_oversized_body_rejected_with_413() {
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .max_body_size(1024);
        let app = transport.into_router();

        // 2 KiB of body against a 1 KiB limit.
        let padding = "x".repeat(2048);
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"ping","params":{{"pad":"{}"}}}}"#,
            padding
        );
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(Body::from(body))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn test_oversized_content_length_rejected_without_reading() {
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .max_body_size(1024);
        let app = transport.into_router();

        // Declared Content-Length above the limit short-circuits even
        // though the actual body is tiny.
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Length", "10485760")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn test_body_within_limit_accepted() {
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .max_body_size(1024);
        let app = transport.into_router();

        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
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
    async fn unsupported_protocol_version_returns_spec_shape_error() {
        // SEP-2575: requests carrying an unrecognized MCP-Protocol-Version
        // header (post-initialize) get a JSON-RPC error with code -32022 and
        // data `{ supported: [...], requested: "..." }`.
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();

        // Initialize first so we're past the init exemption.
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
                        "clientInfo": { "name": "t", "version": "0" }
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

        // Now send a request with a bogus version header.
        let bad = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_SESSION_ID_HEADER, &session_id)
            .header(MCP_PROTOCOL_VERSION_HEADER, "1999-01-01")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 99,
                    "method": "tools/list"
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(bad).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"].as_i64().unwrap(), -32022);
        assert_eq!(json["error"]["data"]["requested"], "1999-01-01");
        let supported = json["error"]["data"]["supported"]
            .as_array()
            .expect("supported must be an array");
        assert!(supported.contains(&serde_json::json!("2025-11-25")));
        // The request id must be echoed (we have one in the body).
        assert_eq!(json["id"], 99);
        // Field name must be `supported`, NOT `supportedVersions` (SEP-2575 shape).
        assert!(
            json["error"]["data"].get("supportedVersions").is_none(),
            "error data must use 'supported', not 'supportedVersions': {:?}",
            json["error"]["data"]
        );
        // The supported set must exactly match the compiled transport default --
        // no extras, none missing.
        let expected: Vec<serde_json::Value> = crate::COMPILED_PROTOCOL_VERSIONS
            .iter()
            .map(|v| serde_json::json!(v))
            .collect();
        assert_eq!(
            supported, &expected,
            "data.supported must exactly match COMPILED_PROTOCOL_VERSIONS"
        );
    }

    /// When a request (no session) arrives with an invalid `Mcp-Protocol-Version`
    /// header, the transport must return -32022 with the correct SEP-2575 wire
    /// shape: `{ supported: [...], requested: "..." }`. This verifies the
    /// version-validation path fires without requiring a session.
    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn stateless_unsupported_protocol_version_returns_spec_shape_error() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();

        // A future-looking unknown version must not enter the 2026-07-28
        // stateless path merely because its date sorts after 2026-07-28.
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_PROTOCOL_VERSION_HEADER, "2099-01-01")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 42,
                    "method": "tools/list"
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["error"]["code"].as_i64().unwrap(),
            -32022,
            "must return UnsupportedProtocolVersion (-32022): {json}"
        );
        assert_eq!(
            json["error"]["data"]["requested"], "2099-01-01",
            "data.requested must echo the version: {json}"
        );
        // Field name must be `supported`, not `supportedVersions`.
        assert!(
            json["error"]["data"].get("supportedVersions").is_none(),
            "error data must use 'supported', not 'supportedVersions': {json}"
        );
        let supported = json["error"]["data"]["supported"]
            .as_array()
            .expect("data.supported must be an array");
        let expected: Vec<serde_json::Value> = crate::COMPILED_PROTOCOL_VERSIONS
            .iter()
            .map(|v| serde_json::json!(v))
            .collect();
        assert_eq!(
            supported, &expected,
            "data.supported must exactly match COMPILED_PROTOCOL_VERSIONS"
        );
    }

    // =========================================================================
    // SEP-2243: HTTP header standardization (Mcp-Method, Mcp-Name, Mcp-Param-*)
    // =========================================================================

    /// In lenient mode (negotiated protocol version < 2026-07-28) a
    /// request without any SEP-2243 headers must still succeed — older
    /// clients that haven't opted in must keep working.
    #[tokio::test]
    async fn sep_2243_lenient_mode_accepts_missing_headers() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();

        // Initialize (no SEP-2243 headers) negotiates 2025-11-25 — lenient.
        let init = Request::builder()
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
                        "clientInfo": { "name": "t", "version": "0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let init_response = app.clone().oneshot(init).await.unwrap();
        assert_eq!(init_response.status(), StatusCode::OK);
        let session_id = init_response
            .headers()
            .get(MCP_SESSION_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // tools/list with no Mcp-Method header — must succeed in lenient mode.
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
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
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// In lenient mode, if the client opts in by sending Mcp-Method,
    /// the server still validates against the body and rejects a
    /// mismatch with -32020.
    #[tokio::test]
    async fn sep_2243_lenient_mode_validates_present_headers() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();

        let init = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header(MCP_METHOD_HEADER, "initialize")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": { "name": "t", "version": "0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let init_response = app.clone().oneshot(init).await.unwrap();
        assert_eq!(init_response.status(), StatusCode::OK);
        let session_id = init_response
            .headers()
            .get(MCP_SESSION_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // tools/list with a deliberately-wrong Mcp-Method header.
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_SESSION_ID_HEADER, &session_id)
            .header(MCP_METHOD_HEADER, "ping")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/list"
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"].as_i64().unwrap(), -32020);
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Mcp-Method")
        );
        assert_eq!(json["id"], 2);
    }

    /// tools/call with matching Mcp-Method + Mcp-Name passes validation
    /// even in strict mode. Driven via initialize with the upcoming
    /// 2026-07-28 protocol version so we exercise the strict branch.
    ///
    /// Gated to `not(stateless)` because with the stateless feature enabled,
    /// initialize requests for 2026-07-28 are handled without a session (chunk 5).
    /// The stateless-mode equivalent is `stateless_v2026_tools_call_without_session_succeeds`.
    #[tokio::test]
    #[cfg(not(feature = "stateless"))]
    async fn sep_2243_strict_mode_tools_call_with_matching_headers() {
        use crate::{CallToolResult, ToolBuilder};

        let router = McpRouter::new().server_info("t", "1.0.0").tool(
            ToolBuilder::new("echo")
                .description("echo")
                .handler(|args: serde_json::Value| async move {
                    Ok(CallToolResult::text(args.to_string()))
                })
                .build(),
        );
        let transport = HttpTransport::new(router).disable_origin_validation();
        let app = transport.into_router();

        // Initialize requesting 2026-07-28 so the session falls into
        // strict mode. The server will negotiate the actual returned
        // version against SUPPORTED_PROTOCOL_VERSIONS, but for SEP-2243
        // gating on init the requested version is what counts.
        let init = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header(MCP_METHOD_HEADER, "initialize")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2026-07-28",
                        "capabilities": {},
                        "clientInfo": { "name": "t", "version": "0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let init_response = app.clone().oneshot(init).await.unwrap();
        assert_eq!(init_response.status(), StatusCode::OK);
        let session_id = init_response
            .headers()
            .get(MCP_SESSION_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let negotiated_version = init_response
            .headers()
            .get(MCP_PROTOCOL_VERSION_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // For subsequent requests, the session's negotiated protocol
        // version is what the validator gates on. If the server did
        // NOT honor 2026-07-28 (because it isn't in SUPPORTED yet) the
        // session will be lenient — which is fine, we just want to
        // confirm the happy path works. If it IS honored, the strict
        // branch is exercised.
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_SESSION_ID_HEADER, &session_id)
            .header(MCP_PROTOCOL_VERSION_HEADER, &negotiated_version)
            .header(MCP_METHOD_HEADER, "tools/call")
            .header(MCP_NAME_HEADER, "echo")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "name": "echo",
                        "arguments": {"message": "hi"}
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// tools/call with mismatched Mcp-Name vs body params.name MUST be
    /// rejected with -32020 and HTTP 400 even in lenient mode.
    #[tokio::test]
    async fn sep_2243_tools_call_mcp_name_mismatch_rejected() {
        use crate::{CallToolResult, ToolBuilder};

        let router = McpRouter::new().server_info("t", "1.0.0").tool(
            ToolBuilder::new("echo")
                .description("echo")
                .handler(|args: serde_json::Value| async move {
                    Ok(CallToolResult::text(args.to_string()))
                })
                .build(),
        );
        let transport = HttpTransport::new(router).disable_origin_validation();
        let app = transport.into_router();

        let init = Request::builder()
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
                        "clientInfo": { "name": "t", "version": "0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let init_response = app.clone().oneshot(init).await.unwrap();
        let session_id = init_response
            .headers()
            .get(MCP_SESSION_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_SESSION_ID_HEADER, &session_id)
            .header(MCP_METHOD_HEADER, "tools/call")
            .header(MCP_NAME_HEADER, "not-echo")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 7,
                    "method": "tools/call",
                    "params": {
                        "name": "echo",
                        "arguments": {"message": "hi"}
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"].as_i64().unwrap(), -32020);
        let msg = json["error"]["message"].as_str().unwrap();
        assert!(msg.contains("Mcp-Name"), "got: {msg}");
        assert_eq!(json["id"], 7);
    }

    /// Mcp-Param-* with a Base64-encoded value that decodes to the
    /// body argument must pass validation.
    #[tokio::test]
    async fn sep_2243_mcp_param_base64_decoded_and_matched() {
        use crate::{CallToolResult, ToolBuilder};

        let router = McpRouter::new().server_info("t", "1.0.0").tool(
            ToolBuilder::new("echo")
                .description("echo")
                .handler(|args: serde_json::Value| async move {
                    Ok(CallToolResult::text(args.to_string()))
                })
                .build(),
        );
        let transport = HttpTransport::new(router).disable_origin_validation();
        let app = transport.into_router();

        let init = Request::builder()
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
                        "clientInfo": { "name": "t", "version": "0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let init_response = app.clone().oneshot(init).await.unwrap();
        let session_id = init_response
            .headers()
            .get(MCP_SESSION_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // Body argument is "Hello"; header is "=?base64?SGVsbG8=?=".
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_SESSION_ID_HEADER, &session_id)
            .header(MCP_METHOD_HEADER, "tools/call")
            .header(MCP_NAME_HEADER, "echo")
            .header("mcp-param-message", "=?base64?SGVsbG8=?=")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 5,
                    "method": "tools/call",
                    "params": {
                        "name": "echo",
                        "arguments": {"message": "Hello"}
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn sep_2243_final_request_requires_schema_annotated_header() {
        use crate::extract::RawArgs;
        use crate::{CallToolResult, ToolBuilder};

        let tool = ToolBuilder::new("route")
            .input_schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "tenant_id": {
                        "type": "string",
                        "x-mcp-header": "Tenant"
                    }
                }
            }))
            .extractor_handler((), |RawArgs(args): RawArgs| async move {
                Ok(CallToolResult::text(args["tenant_id"].to_string()))
            })
            .build();
        let app = HttpTransport::new(
            McpRouter::new()
                .server_info("header-test", "1.0.0")
                .tool(tool),
        )
        .disable_origin_validation()
        .into_router();

        let request = |custom_header: Option<&'static str>| {
            let mut builder = Request::builder()
                .method("POST")
                .uri("/")
                .header("Content-Type", "application/json")
                .header(MCP_PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION_2026_07_28)
                .header(MCP_METHOD_HEADER, "tools/call")
                .header(MCP_NAME_HEADER, "route");
            if let Some(value) = custom_header {
                builder = builder.header("Mcp-Param-Tenant", value);
            }
            builder
                .body(Body::from(
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 9,
                        "method": "tools/call",
                        "params": {
                            "name": "route",
                            "arguments": {"tenant_id": "acme"},
                            "_meta": {
                                "io.modelcontextprotocol/protocolVersion":
                                    PROTOCOL_VERSION_2026_07_28,
                                "io.modelcontextprotocol/clientCapabilities": {}
                            }
                        }
                    })
                    .to_string(),
                ))
                .unwrap()
        };

        let response = app.clone().oneshot(request(None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["id"], 9);
        assert_eq!(error["error"]["code"], -32020);

        let response = app.oneshot(request(Some("acme"))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// notifications/initialized still receives ACCEPTED when SEP-2243
    /// headers match (regression for the notification fast path).
    #[tokio::test]
    async fn sep_2243_notification_with_matching_method_header_accepted() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();

        let init = Request::builder()
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
                        "clientInfo": { "name": "t", "version": "0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let init_response = app.clone().oneshot(init).await.unwrap();
        let session_id = init_response
            .headers()
            .get(MCP_SESSION_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header(MCP_SESSION_ID_HEADER, &session_id)
            .header(MCP_METHOD_HEADER, "notifications/initialized")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
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
        let record = store
            .load(&session_id)
            .await
            .unwrap()
            .expect("expected session to be persisted");
        assert_eq!(record.id, session_id);

        // After initialize completes the record must carry the client's
        // advertised identity / capabilities (issue #786). Previously these
        // were left as `None` because the record was created before
        // initialize ran.
        let client_info = record
            .client_info
            .expect("client_info should be populated after initialize");
        assert_eq!(client_info.name, "test-client");
        assert_eq!(client_info.version, "1.0.0");
        assert!(
            record.client_capabilities.is_some(),
            "client_capabilities should be populated after initialize"
        );

        // Terminate session via the handle -- store should be cleared.
        assert!(handle.terminate_session(&session_id).await);
        assert_eq!(store.len().await, 0);
        assert!(store.load(&session_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_session_store_record_carries_negotiated_protocol_version() {
        // Issue #786: stored record should reflect the negotiated protocol
        // version (taken from the initialize response), not the default the
        // session was created with.
        use crate::session_store::{MemorySessionStore, SessionStore as PublicSessionStore};

        let store = Arc::new(MemorySessionStore::new());
        let store_dyn: Arc<dyn PublicSessionStore> = store.clone();

        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .session_store(store_dyn);
        let app = transport.into_router();

        // Initialize using an older supported protocol version so we can
        // tell the persisted version apart from `LATEST_PROTOCOL_VERSION`.
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
                        "clientInfo": { "name": "v-client", "version": "2.0.0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(init_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let session_id = response
            .headers()
            .get(MCP_SESSION_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let record = store
            .load(&session_id)
            .await
            .unwrap()
            .expect("session should be persisted");
        assert_eq!(record.protocol_version, "2025-03-26");
        let client_info = record.client_info.expect("client_info should be populated");
        assert_eq!(client_info.name, "v-client");
    }

    #[tokio::test]
    async fn test_restored_session_exposes_original_client_info() {
        // Issue #786: a session restored from the persistent store on a
        // peer instance should retain the original client's identity and
        // capabilities, not the synthetic defaults used for auto-reinit.
        use crate::session_store::{MemorySessionStore, SessionStore as PublicSessionStore};

        let store = Arc::new(MemorySessionStore::new());
        let store_dyn: Arc<dyn PublicSessionStore> = store.clone();

        // First "instance": initialize, then drop the transport so the
        // local registry is gone but the persistent record survives.
        let session_id = {
            let transport = HttpTransport::new(create_test_router())
                .disable_origin_validation()
                .session_store(store_dyn.clone());
            let app = transport.into_router();

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
                            "capabilities": { "roots": {} },
                            "clientInfo": {
                                "name": "original-client",
                                "version": "3.1.4"
                            }
                        }
                    })
                    .to_string(),
                ))
                .unwrap();
            let response = app.oneshot(init_request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            response
                .headers()
                .get(MCP_SESSION_ID_HEADER)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        };

        // Sanity check: the persisted record now carries the client info.
        let stored = store
            .load(&session_id)
            .await
            .unwrap()
            .expect("record should survive transport drop");
        assert_eq!(
            stored.client_info.as_ref().map(|c| c.name.as_str()),
            Some("original-client")
        );

        // Second "instance": brand new transport, same store. A request
        // with the existing session id triggers restore_from_record.
        let transport2 = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .session_store(store_dyn);
        let app2 = transport2.into_router();

        let list_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header(MCP_SESSION_ID_HEADER, &session_id)
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/list"
                })
                .to_string(),
            ))
            .unwrap();
        let response = app2.oneshot(list_request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json.get("result").is_some(),
            "expected tools/list result, got {json}"
        );

        // After the restore, the record in the store still carries the
        // original client info (refreshed expiry, not a synthetic
        // "auto-recovered" identity).
        let after_restore = store
            .load(&session_id)
            .await
            .unwrap()
            .expect("record should still be present after restore");
        let client_info = after_restore
            .client_info
            .expect("restored record should retain client_info");
        assert_eq!(client_info.name, "original-client");
        assert_eq!(client_info.version, "3.1.4");
        assert!(
            after_restore.client_capabilities.is_some(),
            "restored record should retain client_capabilities"
        );
    }

    #[tokio::test]
    async fn test_auto_reinitialize_marks_synthetic_client_info() {
        // Companion to the restored-client-info test: the auto-reinit
        // path must continue to flag the client as `"auto-recovered"` so
        // the two paths remain distinguishable on inspection of the
        // persisted record.
        use crate::session_store::{MemorySessionStore, SessionStore as PublicSessionStore};

        let store = Arc::new(MemorySessionStore::new());
        let store_dyn: Arc<dyn PublicSessionStore> = store.clone();

        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .session_store(store_dyn)
            .auto_reinitialize_sessions(true);
        let app = transport.into_router();

        let list_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header(MCP_SESSION_ID_HEADER, "made-up-id")
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

        let record = store
            .load("made-up-id")
            .await
            .unwrap()
            .expect("auto-reinitialize should persist a record");
        assert_eq!(
            record.client_info.as_ref().map(|c| c.name.as_str()),
            Some("auto-recovered")
        );
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
        tower_mcp_types::testing::assert_jsonrpc_error_response(&json);
        assert!(
            json["id"].is_null(),
            "id must be null on parse error: {json}"
        );
        assert_eq!(json["error"]["code"].as_i64().unwrap(), -32700);
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

    // =========================================================================
    // Host header validation (DNS rebinding defense complement to Origin)
    // =========================================================================

    fn initialize_body() -> Body {
        Body::from(
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
        )
    }

    #[test]
    fn test_is_localhost_host_variants() {
        assert!(is_localhost_host("localhost"));
        assert!(is_localhost_host("localhost:3000"));
        assert!(is_localhost_host("127.0.0.1"));
        assert!(is_localhost_host("127.0.0.1:8080"));
        assert!(is_localhost_host("[::1]"));
        assert!(is_localhost_host("[::1]:3000"));

        assert!(!is_localhost_host("evil.com"));
        assert!(!is_localhost_host("api.example.com:8443"));
        assert!(!is_localhost_host("10.0.0.1"));
    }

    #[tokio::test]
    async fn test_host_validation_allows_localhost() {
        let transport = HttpTransport::new(create_test_router())
            .allowed_hosts(vec!["api.example.com".to_string()]);
        let app = transport.into_router();

        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Host", "127.0.0.1:3000")
            .body(initialize_body())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_host_validation_allows_configured_host() {
        let transport = HttpTransport::new(create_test_router())
            .allowed_hosts(vec!["api.example.com".to_string()]);
        let app = transport.into_router();

        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Host", "api.example.com")
            .body(initialize_body())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_host_validation_rejects_unconfigured_host() {
        let transport = HttpTransport::new(create_test_router())
            .allowed_hosts(vec!["api.example.com".to_string()]);
        let app = transport.into_router();

        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Host", "evil.com")
            .body(initialize_body())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_host_validation_no_allowlist_accepts_any_host() {
        // Existing deployments that haven't opted into Host validation
        // (no `.allowed_hosts(...)`) should keep accepting non-localhost
        // hosts; Origin still protects browsers.
        let transport = HttpTransport::new(create_test_router());
        let app = transport.into_router();

        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Host", "any.example.com")
            .body(initialize_body())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_disabled_host_validation_allows_any_with_allowlist() {
        let transport = HttpTransport::new(create_test_router())
            .disable_host_validation()
            .allowed_hosts(vec!["api.example.com".to_string()]);
        let app = transport.into_router();

        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Host", "evil.com")
            .body(initialize_body())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn test_effective_host_prefers_header() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("api.example.com"));
        let uri: axum::http::Uri = "http://other.example.com/path".parse().unwrap();
        assert_eq!(effective_host(&headers, &uri), Some("api.example.com"));
    }

    #[test]
    fn test_effective_host_falls_back_to_authority() {
        // When Host header is missing (HTTP/2 + middleware that strips it),
        // we should fall back to the URI authority.
        let headers = HeaderMap::new();
        let uri: axum::http::Uri = "http://api.example.com/path".parse().unwrap();
        assert_eq!(effective_host(&headers, &uri), Some("api.example.com"));
    }

    #[test]
    fn test_effective_host_returns_none_when_both_missing() {
        let headers = HeaderMap::new();
        let uri: axum::http::Uri = "/path".parse().unwrap();
        assert_eq!(effective_host(&headers, &uri), None);
    }

    // =========================================================================
    // External notification fan-out
    // =========================================================================

    /// Initialize a session against `app` and return its session id.
    async fn init_session(app: &Router) -> String {
        let req = Request::builder()
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
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        resp.headers()
            .get(MCP_SESSION_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .expect("initialize must return a session id")
    }

    #[tokio::test]
    async fn test_external_notification_reaches_single_session() {
        let (notif_tx, notif_rx) = notification_channel(8);
        let transport = HttpTransport::with_notifications(create_test_router(), notif_rx);
        let (app, session_handle) = transport.into_router_with_handle();

        let session_id = init_session(&app).await;

        // Subscribe to the session's broadcast channel before firing.
        let mut rx = {
            let sessions = session_handle.store.sessions.read().await;
            let session = sessions
                .get(&session_id)
                .expect("session should be registered");
            session.notifications_tx.subscribe()
        };

        notif_tx
            .send(crate::context::ServerNotification::ResourceUpdated {
                uri: "claude://chats/abc".to_string(),
            })
            .await
            .unwrap();

        let json = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("notification should arrive within timeout")
            .expect("broadcast channel closed");
        assert!(json.contains("notifications/resources/updated"));
        assert!(json.contains("claude://chats/abc"));
    }

    #[tokio::test]
    async fn test_external_notification_fans_out_to_all_sessions() {
        let (notif_tx, notif_rx) = notification_channel(8);
        let transport = HttpTransport::with_notifications(create_test_router(), notif_rx);
        let (app, session_handle) = transport.into_router_with_handle();

        let session_a = init_session(&app).await;
        let session_b = init_session(&app).await;
        assert_ne!(session_a, session_b);

        let (mut rx_a, mut rx_b) = {
            let sessions = session_handle.store.sessions.read().await;
            let a = sessions.get(&session_a).unwrap();
            let b = sessions.get(&session_b).unwrap();
            (
                a.notifications_tx.subscribe(),
                b.notifications_tx.subscribe(),
            )
        };

        notif_tx
            .send(crate::context::ServerNotification::ResourcesListChanged)
            .await
            .unwrap();

        let json_a = tokio::time::timeout(Duration::from_secs(1), rx_a.recv())
            .await
            .unwrap()
            .unwrap();
        let json_b = tokio::time::timeout(Duration::from_secs(1), rx_b.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(json_a.contains("notifications/resources/list_changed"));
        assert!(json_b.contains("notifications/resources/list_changed"));
    }

    #[tokio::test]
    async fn test_external_notifications_builder_method() {
        // `external_notifications` should be equivalent to the constructor.
        let (notif_tx, notif_rx) = notification_channel(8);
        let transport = HttpTransport::new(create_test_router()).external_notifications(notif_rx);
        let (app, session_handle) = transport.into_router_with_handle();

        let session_id = init_session(&app).await;
        let mut rx = {
            let sessions = session_handle.store.sessions.read().await;
            sessions
                .get(&session_id)
                .unwrap()
                .notifications_tx
                .subscribe()
        };

        notif_tx
            .send(crate::context::ServerNotification::ToolsListChanged)
            .await
            .unwrap();

        let json = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(json.contains("notifications/tools/list_changed"));
    }

    #[tokio::test]
    async fn test_default_transport_has_no_external_fanout_task() {
        // Smoke test: a transport without external notifications builds and
        // serves normally. (Verifying the fan-out task is *not* spawned is
        // hard to do directly; this just confirms we didn't accidentally
        // gate the happy path on the channel being present.)
        let transport = HttpTransport::new(create_test_router());
        let (app, _handle) = transport.into_router_with_handle();
        let _session_id = init_session(&app).await;
    }

    // =========================================================================
    // Chunk 5: version-gated stateless mode for 2026-07-28+ clients
    // =========================================================================

    /// 2026-07-28 removed initialize entirely.
    #[tokio::test]
    #[cfg(feature = "stateless")]
    async fn stateless_v2026_initialize_is_method_not_found() {
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .disable_host_validation();
        let app = transport.into_router();
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header(MCP_METHOD_HEADER, "initialize")
            .header(MCP_PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION_2026_07_28)
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2026-07-28",
                        "capabilities": {},
                        "clientInfo": { "name": "sc", "version": "1.0" },
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientCapabilities": {}
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            !response.headers().contains_key(MCP_SESSION_ID_HEADER),
            "removed final method must not create a session"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["id"], 1);
        assert_eq!(json["error"]["code"], ErrorCode::MethodNotFound.code());
    }

    #[tokio::test]
    #[cfg(feature = "stateless")]
    async fn stateless_v2026_rejects_missing_required_meta_with_http_400() {
        let app = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .disable_host_validation()
            .into_router();
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_METHOD_HEADER, "server/discover")
            .header(MCP_PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION_2026_07_28)
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 101,
                    "method": "server/discover",
                    "params": {}
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["id"], 101);
        assert_eq!(json["error"]["code"], ErrorCode::InvalidParams.code());
    }

    #[tokio::test]
    #[cfg(feature = "stateless")]
    async fn stateless_v2026_rejects_invalid_meta_and_extension_keys_with_http_400() {
        let app = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .disable_host_validation()
            .into_router();

        let build_request =
            |id: i64, extra_meta: serde_json::Value, extensions: serde_json::Value| {
                let mut meta = serde_json::json!({
                    "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION_2026_07_28,
                    "io.modelcontextprotocol/clientCapabilities": {
                        "extensions": extensions
                    }
                });
                meta.as_object_mut()
                    .unwrap()
                    .extend(extra_meta.as_object().unwrap().clone());
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json")
                    .header(MCP_METHOD_HEADER, "server/discover")
                    .header(MCP_PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION_2026_07_28)
                    .body(Body::from(
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "method": "server/discover",
                            "params": { "_meta": meta }
                        })
                        .to_string(),
                    ))
                    .unwrap()
            };

        for request in [
            build_request(
                111,
                serde_json::json!({"com.example/-invalid": true}),
                serde_json::json!({}),
            ),
            build_request(
                112,
                serde_json::json!({}),
                serde_json::json!({"unprefixed": {}}),
            ),
            build_request(
                113,
                serde_json::json!({}),
                serde_json::json!({"com.example/feature": true}),
            ),
        ] {
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["error"]["code"], ErrorCode::InvalidParams.code());
        }
    }

    #[tokio::test]
    #[cfg(feature = "stateless")]
    async fn stateless_v2026_rejects_missing_protocol_header_with_http_400() {
        let app = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .disable_host_validation()
            .into_router();
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_METHOD_HEADER, "server/discover")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 102,
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
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["id"], 102);
        assert_eq!(json["error"]["code"], McpErrorCode::HeaderMismatch.code());
    }

    #[tokio::test]
    #[cfg(feature = "stateless")]
    async fn stateless_v2026_unknown_method_is_http_404() {
        let app = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .disable_host_validation()
            .into_router();
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_METHOD_HEADER, "unknown/method")
            .header(MCP_PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION_2026_07_28)
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 103,
                    "method": "unknown/method",
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
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["id"], 103);
        assert_eq!(json["error"]["code"], ErrorCode::MethodNotFound.code());
    }

    #[tokio::test]
    #[cfg(feature = "stateless")]
    async fn stateless_v2026_ignores_legacy_session_and_resumption_headers() {
        let app = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .disable_host_validation()
            .into_router();
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_METHOD_HEADER, "tools/list")
            .header(MCP_PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION_2026_07_28)
            .header(MCP_SESSION_ID_HEADER, "legacy-session-that-does-not-exist")
            .header(LAST_EVENT_ID_HEADER, "legacy-event")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 104,
                    "method": "tools/list",
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
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(MCP_SESSION_ID_HEADER));
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["result"]["tools"].is_array());
    }

    #[tokio::test]
    #[cfg(feature = "stateless")]
    async fn stateless_v2026_enforces_tool_client_capability_requirements() {
        use crate::{CallToolResult, SamplingCapability, ToolBuilder};

        let tool = ToolBuilder::new("sample")
            .no_params_handler(|| async { Ok(CallToolResult::text("ok")) })
            .build()
            .require_client_capabilities(ClientCapabilities {
                sampling: Some(SamplingCapability::default()),
                ..ClientCapabilities::default()
            });
        let router = McpRouter::new()
            .server_info("test-server", "1.0.0")
            .tool(tool);
        let app = HttpTransport::new(router)
            .disable_origin_validation()
            .disable_host_validation()
            .into_router();

        let build_request = |id: i64, capabilities: serde_json::Value| {
            Request::builder()
                .method("POST")
                .uri("/")
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .header(MCP_METHOD_HEADER, "tools/call")
                .header(MCP_NAME_HEADER, "sample")
                .header(MCP_PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION_2026_07_28)
                .body(Body::from(
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "method": "tools/call",
                        "params": {
                            "name": "sample",
                            "arguments": {},
                            "_meta": {
                                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                                "io.modelcontextprotocol/clientCapabilities": capabilities
                            }
                        }
                    })
                    .to_string(),
                ))
                .unwrap()
        };

        let response = app
            .clone()
            .oneshot(build_request(105, serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["id"], 105);
        assert_eq!(
            json["error"]["code"],
            McpErrorCode::MissingRequiredClientCapability.code()
        );
        assert_eq!(
            json["error"]["data"]["requiredCapabilities"],
            serde_json::json!({ "sampling": {} })
        );

        let response = app
            .oneshot(build_request(106, serde_json::json!({ "sampling": {} })))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["result"]["content"][0]["text"], "ok");
    }

    /// 2026-07-28 tools/call without session header succeeds.
    #[tokio::test]
    #[cfg(feature = "stateless")]
    async fn stateless_v2026_tools_call_without_session_succeeds() {
        use crate::{CallToolResult, ToolBuilder};
        let router = McpRouter::new().server_info("t", "1.0.0").tool(
            ToolBuilder::new("echo")
                .description("echo")
                .handler(|args: serde_json::Value| async move {
                    Ok(CallToolResult::text(args.to_string()))
                })
                .build(),
        );
        let transport = HttpTransport::new(router).disable_origin_validation();
        let app = transport.into_router();
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_PROTOCOL_VERSION_HEADER, "2026-07-28")
            .header(MCP_METHOD_HEADER, "tools/call")
            .header(MCP_NAME_HEADER, "echo")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {
                        "name": "echo",
                        "arguments": {"message": "hello"},
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientInfo": {
                                "name": "sc", "version": "1.0"
                            },
                            "io.modelcontextprotocol/clientCapabilities": {}
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !response.headers().contains_key(MCP_SESSION_ID_HEADER),
            "stateless tools/call must not set mcp-session-id"
        );
        assert_eq!(
            response
                .headers()
                .get(MCP_PROTOCOL_VERSION_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("2026-07-28")
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json.get("result").is_some(),
            "expected tools/call result, got: {json}"
        );
        assert_eq!(json["result"]["resultType"], "complete");
    }

    /// A stateless 2026-07-28 request body helper for the serverInfo tests below.
    #[cfg(feature = "stateless")]
    fn stateless_tools_call_request() -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_PROTOCOL_VERSION_HEADER, "2026-07-28")
            .header(MCP_METHOD_HEADER, "tools/call")
            .header(MCP_NAME_HEADER, "echo")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {
                        "name": "echo",
                        "arguments": {"message": "hello"},
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientInfo": {
                                "name": "sc", "version": "1.0"
                            },
                            "io.modelcontextprotocol/clientCapabilities": {}
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap()
    }

    #[cfg(feature = "stateless")]
    fn echo_router() -> McpRouter {
        use crate::{CallToolResult, ToolBuilder};
        McpRouter::new().server_info("t", "1.0.0").tool(
            ToolBuilder::new("echo")
                .description("echo")
                .handler(|args: serde_json::Value| async move {
                    Ok(CallToolResult::text(args.to_string()))
                })
                .build(),
        )
    }

    /// SEP-2575: 2026-07-28 stateless responses carry server identity in
    /// `_meta["io.modelcontextprotocol/serverInfo"]` by default.
    #[tokio::test]
    #[cfg(feature = "stateless")]
    async fn stateless_v2026_response_stamps_server_info_by_default() {
        let transport = HttpTransport::new(echo_router()).disable_origin_validation();
        let app = transport.into_router();
        let response = app.oneshot(stateless_tools_call_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "t",
            "expected serverInfo stamped into result._meta, got: {json}"
        );
        assert_eq!(
            json["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["version"],
            "1.0.0"
        );
    }

    /// `.stamp_server_info(false)` opts out of the SEP-2575 `_meta` stamp.
    #[tokio::test]
    #[cfg(feature = "stateless")]
    async fn stateless_v2026_response_omits_server_info_when_disabled() {
        let transport = HttpTransport::new(echo_router())
            .disable_origin_validation()
            .stamp_server_info(false);
        let app = transport.into_router();
        let response = app.oneshot(stateless_tools_call_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["result"].get("_meta").is_none(),
            "expected no _meta when stamping is disabled, got: {json}"
        );
    }

    /// 2025-11-25 initialize still returns mcp-session-id (unchanged).
    #[tokio::test]
    async fn stateless_v2025_initialize_still_gets_session_id() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": { "name": "old-client", "version": "1.0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers().contains_key(MCP_SESSION_ID_HEADER),
            "2025-11-25 initialize must return mcp-session-id"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["result"].get("resultType").is_none(),
            "legacy result must remain unchanged: {json}"
        );
    }

    /// With require_sessions(), 2025-11-25 tools/list without session
    /// header fails with SessionRequired (-32006) -- behavior unchanged.
    #[tokio::test]
    async fn stateless_v2025_tools_list_without_session_rejected() {
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .require_sessions();
        let app = transport.into_router();
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_PROTOCOL_VERSION_HEADER, "2025-11-25")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tools/list"
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("error").is_some(), "expected error, got: {json}");
        assert_eq!(
            json["error"]["code"].as_i64().unwrap(),
            -32006,
            "expected SessionRequired (-32006)"
        );
    }

    /// 2026-07-28 tools/list without session header succeeds (#856).
    #[tokio::test]
    #[cfg(feature = "stateless")]
    async fn stateless_v2026_tools_list_without_session_succeeds() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_PROTOCOL_VERSION_HEADER, "2026-07-28")
            .header(MCP_METHOD_HEADER, "tools/list")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/list",
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
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !response.headers().contains_key(MCP_SESSION_ID_HEADER),
            "stateless tools/list must not set mcp-session-id"
        );
        assert_eq!(
            response
                .headers()
                .get(MCP_PROTOCOL_VERSION_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("2026-07-28")
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["result"]["tools"].is_array(),
            "expected tools array in result, got: {json}"
        );
        assert_eq!(json["result"]["resultType"], "complete");
        assert_eq!(json["result"]["ttlMs"], 0);
        assert_eq!(json["result"]["cacheScope"], "private");
    }

    /// A final-protocol cancellation notification returns 202 and creates no
    /// session (#857).
    #[tokio::test]
    #[cfg(feature = "stateless")]
    async fn stateless_v2026_notification_returns_202_no_session() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let (app, handle) = transport.into_router_with_handle();
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_PROTOCOL_VERSION_HEADER, "2026-07-28")
            .header(MCP_METHOD_HEADER, "notifications/cancelled")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": {
                        "requestId": 99,
                        "reason": "test",
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientCapabilities": {}
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::ACCEPTED,
            "stateless notification must return 202 ACCEPTED"
        );
        assert!(
            !response.headers().contains_key(MCP_SESSION_ID_HEADER),
            "stateless notification must not set mcp-session-id"
        );
        assert_eq!(
            handle.session_count().await,
            0,
            "stateless notification must not create a session"
        );
    }

    /// 2026-07-28 stateless request missing Mcp-Method returns -32020 + HTTP 400 (#859).
    #[tokio::test]
    #[cfg(feature = "stateless")]
    async fn stateless_v2026_missing_mcp_method_returns_400() {
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();
        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_PROTOCOL_VERSION_HEADER, "2026-07-28")
            // Intentionally NO Mcp-Method header
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/list",
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
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "missing Mcp-Method must return HTTP 400"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("error").is_some(), "expected error, got: {json}");
        assert_eq!(
            json["error"]["code"].as_i64().unwrap(),
            -32020,
            "expected HeaderMismatch (-32020)"
        );
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("Mcp-Method"),
            "error message must mention Mcp-Method, got: {json}"
        );
    }

    #[tokio::test]
    async fn sse_responses_false_returns_application_json() {
        // Default behavior: synchronous responses use Content-Type: application/json
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .sse_responses(false);
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
                        "clientInfo": {"name": "test", "version": "0.1"}
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let ct = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.contains("application/json"),
            "sse_responses(false) should return application/json, got: {ct}"
        );
    }

    #[tokio::test]
    async fn sse_responses_true_returns_text_event_stream_with_valid_json() {
        // When sse_responses is enabled, synchronous responses use SSE format
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .sse_responses(true);
        let app = transport.into_router();

        let init_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0.1"}
            }
        })
        .to_string();

        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(Body::from(init_body))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let ct = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.contains("text/event-stream"),
            "sse_responses(true) should return text/event-stream, got: {ct}"
        );

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_text = String::from_utf8_lossy(&bytes);

        // SSE body must contain the event type line and data line
        assert!(
            body_text.contains("event: message"),
            "SSE body missing 'event: message': {body_text}"
        );
        assert!(
            body_text.contains("data: "),
            "SSE body missing 'data: ' line: {body_text}"
        );

        // Extract and validate the JSON from the data: line
        let data_line = body_text
            .lines()
            .find(|l| l.starts_with("data: "))
            .expect("no data: line in SSE body");
        let json_str = data_line.trim_start_matches("data: ");
        let val: serde_json::Value =
            serde_json::from_str(json_str).expect("data: line is not valid JSON");

        // Verify it's a well-formed JSON-RPC response with the expected result
        assert_eq!(val["jsonrpc"], "2.0", "jsonrpc version mismatch: {val}");
        assert_eq!(val["id"], 1, "id mismatch: {val}");
        assert!(
            val["result"].is_object(),
            "result should be an object: {val}"
        );
        // The initialize result must contain protocolVersion
        assert_eq!(
            val["result"]["protocolVersion"].as_str(),
            Some("2025-11-25"),
            "protocolVersion missing or wrong: {val}"
        );
    }

    #[tokio::test]
    async fn sse_responses_true_tools_list_returns_valid_sse() {
        // Verify tools/list (non-init request) also returns SSE when enabled
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .sse_responses(true);
        let app = transport.into_router();

        // Initialize first to get a session ID
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
                        "clientInfo": {"name": "test", "version": "0.1"}
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let init_response = app.clone().oneshot(init_request).await.unwrap();
        assert_eq!(init_response.status(), StatusCode::OK);
        let session_id = init_response
            .headers()
            .get(MCP_SESSION_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .expect("missing session ID from initialize");

        // Send notifications/initialized to complete the MCP handshake.
        let notif_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header(MCP_SESSION_ID_HEADER, &session_id)
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                })
                .to_string(),
            ))
            .unwrap();
        app.clone().oneshot(notif_request).await.unwrap();

        // Now call tools/list
        let list_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header(MCP_SESSION_ID_HEADER, &session_id)
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

        let list_response = app.oneshot(list_request).await.unwrap();
        assert_eq!(list_response.status(), StatusCode::OK);

        let ct = list_response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.contains("text/event-stream"),
            "tools/list with sse_responses(true) should return text/event-stream, got: {ct}"
        );

        let bytes = axum::body::to_bytes(list_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_text = String::from_utf8_lossy(&bytes);
        let data_line = body_text
            .lines()
            .find(|l| l.starts_with("data: "))
            .expect("no data: line in SSE body for tools/list");
        let json_str = data_line.trim_start_matches("data: ");
        let val: serde_json::Value =
            serde_json::from_str(json_str).expect("tools/list data: line is not valid JSON");

        assert_eq!(val["jsonrpc"], "2.0");
        assert_eq!(val["id"], 2);
        // tools/list result has a "tools" array (may be empty for create_test_router())
        assert!(
            val["result"]["tools"].is_array(),
            "tools/list result.tools should be an array: {val}"
        );
    }

    // =========================================================================
    // notifications/initialized enforcement (#901)
    // =========================================================================

    /// Helper: do the `initialize` handshake and return the session ID.
    async fn do_initialize(app: &axum::Router) -> String {
        do_initialize_for_revision(app, "2025-11-25").await
    }

    async fn do_initialize_for_revision(app: &axum::Router, revision: &str) -> String {
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
                        "protocolVersion": revision,
                        "capabilities": {},
                        "clientInfo": { "name": "test-client", "version": "1.0.0" }
                    }
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.clone().oneshot(init_request).await.unwrap();
        response
            .headers()
            .get(MCP_SESSION_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    }

    async fn send_initialized(app: &axum::Router, session_id: &str) {
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header(MCP_SESSION_ID_HEADER, session_id)
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    async fn post_legacy_batch(app: &axum::Router, session_id: &str) -> serde_json::Value {
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_SESSION_ID_HEADER, session_id)
            .body(Body::from(
                serde_json::json!([
                    {"jsonrpc": "2.0", "id": 2, "method": "ping"},
                    {"jsonrpc": "2.0", "id": 3, "method": "tools/list"}
                ])
                .to_string(),
            ))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn http_batch_policy_uses_exact_session_revision() {
        let march_app = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .protocol_versions(["2025-03-26"])
            .unwrap()
            .into_router();
        let march_session = do_initialize_for_revision(&march_app, "2025-03-26").await;
        send_initialized(&march_app, &march_session).await;
        let march_response = post_legacy_batch(&march_app, &march_session).await;
        assert_eq!(march_response.as_array().map(Vec::len), Some(2));

        let november_app = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .into_router();
        let november_session = do_initialize(&november_app).await;
        send_initialized(&november_app, &november_session).await;
        let november_response = post_legacy_batch(&november_app, &november_session).await;
        assert_eq!(november_response["error"]["code"], -32600);
        assert!(
            november_response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("does not permit top-level JSON-RPC batches")
        );
    }

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn http_final_batch_is_rejected_before_object_routing() {
        let app = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .into_router();
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header(MCP_PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION_2026_07_28)
            .body(Body::from(
                serde_json::json!([{
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/list",
                    "params": {
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientCapabilities": {}
                        }
                    }
                }])
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let response: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(response["error"]["code"], -32600);
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("does not permit top-level JSON-RPC batches")
        );
    }

    #[tokio::test]
    async fn tools_list_before_initialized_notification_returns_error() {
        // Spec: clients MUST send notifications/initialized before any other
        // request. Skipping it should yield -32600 InvalidRequest.
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();

        let session_id = do_initialize(&app).await;

        // Send tools/list WITHOUT sending notifications/initialized first.
        let list_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
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
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json.get("error").is_some(),
            "expected error when notifications/initialized not sent, got: {json}"
        );
        assert_eq!(
            json["error"]["code"].as_i64().unwrap(),
            -32600,
            "expected InvalidRequest (-32600), got: {json}"
        );
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("notifications/initialized"),
            "error message should mention notifications/initialized, got: {json}"
        );
    }

    #[tokio::test]
    async fn tools_list_after_initialized_notification_succeeds() {
        // After sending notifications/initialized, tool requests should succeed.
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();

        let session_id = do_initialize(&app).await;

        // Send notifications/initialized.
        let notif_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header(MCP_SESSION_ID_HEADER, &session_id)
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                })
                .to_string(),
            ))
            .unwrap();
        app.clone().oneshot(notif_request).await.unwrap();

        // Now tools/list should succeed.
        let list_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
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
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json.get("result").is_some(),
            "expected success after notifications/initialized, got: {json}"
        );
    }

    #[tokio::test]
    async fn notifications_initialized_itself_always_accepted() {
        // The notifications/initialized notification itself must always be
        // accepted (202 ACCEPTED) regardless of the initialization flag.
        let transport = HttpTransport::new(create_test_router()).disable_origin_validation();
        let app = transport.into_router();

        let session_id = do_initialize(&app).await;

        let notif_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header(MCP_SESSION_ID_HEADER, &session_id)
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(notif_request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::ACCEPTED,
            "notifications/initialized must return 202 ACCEPTED"
        );
    }

    #[tokio::test]
    async fn strict_initialization_false_allows_tools_list_without_notification() {
        // When strict_initialization is disabled, tool requests must succeed
        // even if the client skips notifications/initialized.
        let config = SessionConfig {
            strict_initialization: false,
            ..Default::default()
        };
        let transport = HttpTransport::new(create_test_router())
            .disable_origin_validation()
            .session_config(config);
        let app = transport.into_router();

        let session_id = do_initialize(&app).await;

        // No notifications/initialized -- should still succeed.
        let list_request = Request::builder()
            .method("POST")
            .uri("/")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
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
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json.get("result").is_some(),
            "expected success with strict_initialization=false, got: {json}"
        );
    }
}
