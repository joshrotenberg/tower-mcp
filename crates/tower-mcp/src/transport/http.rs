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

/// Resource limits for final-protocol `subscriptions/listen` streams.
///
/// These limits apply only to sessionless HTTP subscriptions negotiated with
/// the 2026-07-28 protocol. They bound both the number of live streams and the
/// notification backlog retained for a client that stops reading.
///
/// Defaults are deliberately finite: 256 active streams per transport, 64 KiB
/// of serialized request identity/filter metadata, 64 queued notifications,
/// and 256 KiB of serialized notification data per stream. Per-principal
/// admission is optional and disabled by default.
#[cfg(feature = "stateless")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SubscriptionLimits {
    /// Maximum active final subscriptions across this transport.
    pub max_active: usize,
    /// Optional maximum active subscriptions for one authenticated principal.
    pub max_active_per_principal: Option<usize>,
    /// Maximum combined serialized request-ID and negotiated-filter bytes.
    pub max_metadata_bytes: usize,
    /// Maximum queued notifications for one stream.
    pub max_buffered_messages: usize,
    /// Maximum serialized notification bytes queued for one stream.
    pub max_buffered_bytes: usize,
}

#[cfg(feature = "stateless")]
impl Default for SubscriptionLimits {
    fn default() -> Self {
        Self {
            max_active: 256,
            max_active_per_principal: None,
            max_metadata_bytes: 64 * 1024,
            max_buffered_messages: 64,
            max_buffered_bytes: 256 * 1024,
        }
    }
}

#[cfg(feature = "stateless")]
impl SubscriptionLimits {
    /// Set the process-local active subscription limit.
    pub fn max_active(mut self, max: usize) -> Self {
        self.max_active = max;
        self
    }

    /// Set the active subscription limit for each authenticated principal.
    pub fn max_active_per_principal(mut self, max: usize) -> Self {
        self.max_active_per_principal = Some(max);
        self
    }

    /// Set the serialized request-ID and negotiated-filter budget per stream.
    pub fn max_metadata_bytes(mut self, max: usize) -> Self {
        self.max_metadata_bytes = max;
        self
    }

    /// Set the queued notification count limit for each stream.
    pub fn max_buffered_messages(mut self, max: usize) -> Self {
        self.max_buffered_messages = max;
        self
    }

    /// Set the serialized queued-notification byte limit for each stream.
    pub fn max_buffered_bytes(mut self, max: usize) -> Self {
        self.max_buffered_bytes = max;
        self
    }
}

/// SSE event type for JSON-RPC messages
const SSE_MESSAGE_EVENT: &str = "message";

/// Header name for Last-Event-ID (for SSE stream resumption per SEP-1699)
const LAST_EVENT_ID_HEADER: &str = "last-event-id";

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
    /// request handler. Drained by a background task and routed to live
    /// session SSE streams. Legacy resource updates honor each router-backed
    /// session's `resources/subscribe` membership.
    external_notifications: Option<NotificationReceiver>,
    #[cfg(feature = "stateless")]
    stateless_config: Option<crate::stateless::StatelessConfig>,
    /// Admission and buffering policy for final subscription streams.
    #[cfg(feature = "stateless")]
    subscription_limits: SubscriptionLimits,
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
            subscription_limits: SubscriptionLimits::default(),
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
    /// HTTP session, but the transport cannot make arbitrary service-internal
    /// state session-local. In particular, passing an [`McpRouter`] directly
    /// here shares that router's logical-session state, including legacy
    /// `resources/subscribe` membership. Use [`HttpTransport::new`] for an
    /// `McpRouter`; it creates a fresh, isolated router session per client.
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
            subscription_limits: SubscriptionLimits::default(),
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
    /// channel and routes the items to live session SSE streams.
    ///
    /// This mirrors [`GenericStdioTransport::with_notifications`](crate::transport::stdio::GenericStdioTransport::with_notifications)
    /// and is the supported way to push server-originated notifications
    /// (e.g. `notifications/resources/updated`) from outside any request
    /// handler — background tasks, lifecycle hooks, anything async that
    /// needs to notify subscribed clients.
    ///
    /// Per-session notification channels (in-handler `ctx.send_log()`,
    /// progress updates) are unaffected. The external channel runs in
    /// parallel. For legacy router-backed sessions,
    /// [`ServerNotification::ResourceUpdated`] is delivered only to sessions
    /// that successfully called `resources/subscribe` for that exact URI.
    /// Other notification kinds continue to reach every active session.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::{BoxError, McpRouter, ResourceBuilder};
    /// use tower_mcp::context::{ServerNotification, notification_channel};
    /// use tower_mcp::transport::http::HttpTransport;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), BoxError> {
    ///     let (notif_tx, notif_rx) = notification_channel(256);
    ///
    ///     let router = McpRouter::new()
    ///         .server_info("my-server", "1.0.0")
    ///         .resource(
    ///             ResourceBuilder::new("claude://chats/123")
    ///                 .name("Chat 123")
    ///                 .text("chat contents"),
    ///         );
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
    /// router isn't part of the flow. A service-backed session exposes no
    /// router whose resource subscriptions the transport can inspect, so all
    /// external notifications retain the caller-owned broadcast behavior in
    /// that mode. Filter the supplied channel before sending when a custom
    /// service needs narrower delivery. See
    /// [`with_notifications`](Self::with_notifications) for the typical
    /// router-based path.
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

    /// Configure admission and buffering limits for final subscriptions.
    ///
    /// The policy is process-local to this transport. A stream that exceeds
    /// either buffering budget is closed with an observable terminal error;
    /// clients can reconnect and use `tasks/get` for authoritative task state.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tower_mcp::{HttpTransport, McpRouter, SubscriptionLimits};
    ///
    /// let limits = SubscriptionLimits::default()
    ///     .max_active(128)
    ///     .max_active_per_principal(8)
    ///     .max_metadata_bytes(32 * 1024)
    ///     .max_buffered_messages(32)
    ///     .max_buffered_bytes(128 * 1024);
    /// let transport = HttpTransport::new(McpRouter::new())
    ///     .subscription_limits(limits);
    /// # let _ = transport;
    /// ```
    #[cfg(feature = "stateless")]
    pub fn subscription_limits(mut self, limits: SubscriptionLimits) -> Self {
        self.subscription_limits = limits;
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
    /// Runtime state (broadcast channels, pending requests, service instances,
    /// and legacy `resources/subscribe` memberships) is always kept
    /// per-instance; only persistent metadata is mirrored to the store. A
    /// client whose session is restored on another instance must resubscribe to
    /// resources.
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
            self.subscription_limits,
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
/// Drain a caller-supplied notification channel and route items to live
/// session SSE broadcasts.
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
                sessions
                    .broadcast_external_notification(&notification, &json)
                    .await;
            }
        }
        tracing::debug!("External notification channel closed; fan-out task exiting");
    });
}

mod handlers;
mod session;
#[cfg(feature = "stateless")]
mod stateless_dispatch;

use handlers::{handle_delete, handle_get, handle_health, handle_post};
pub use session::{DEFAULT_SESSION_TTL, SessionConfig, SessionHandle, SessionInfo};
// Only `stateless_dispatch` (gated below) calls back into these two; importing
// them unconditionally would be an unused import (-> error under -D warnings)
// in a build without the feature.
#[cfg(feature = "stateless")]
use handlers::{extract_request_id, json_rpc_error_response_with_status};
use session::{Session, SessionRegistry};
// Gated in `stateless_dispatch` too, so importing any of these unconditionally
// breaks the default build that `--all-features` never exercises.
#[cfg(feature = "stateless")]
use stateless_dispatch::{
    CancelOnDisconnect, ModernSubscriptionRegistry, StatelessSseContext,
    handle_modern_subscriptions_listen_sse, is_stateless_protocol_version, modern_response_status,
    stamp_server_info, stash_per_request_meta, stateless_sse_with_notifications,
};

#[cfg(test)]
mod tests;
