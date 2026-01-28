//! Streamable HTTP transport for MCP
//!
//! Implements the Streamable HTTP transport from MCP specification 2025-11-25.
//!
//! Features:
//! - Single endpoint for POST (requests) and GET (SSE notifications)
//! - Session management via `MCP-Session-Id` header
//! - SSE streaming for server notifications and progress updates
//!
//! # Example
//!
//! ```rust,no_run
//! use tower_mcp::{McpRouter, ToolBuilder, CallToolResult};
//! use tower_mcp::transport::http::HttpTransport;
//! use schemars::JsonSchema;
//! use serde::Deserialize;
//!
//! #[derive(Debug, Deserialize, JsonSchema)]
//! struct Input { value: String }
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let tool = ToolBuilder::new("echo")
//!         .handler(|i: Input| async move { Ok(CallToolResult::text(i.value)) })
//!         .build()?;
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{delete, get, post},
};
use tokio::sync::{RwLock, broadcast};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::error::{Error, JsonRpcError, Result};
use crate::protocol::{
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, McpNotification,
    SUPPORTED_PROTOCOL_VERSIONS,
};
use crate::router::{JsonRpcService, McpRouter};

/// Header name for MCP session ID
pub const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";

/// Header name for MCP protocol version
pub const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";

/// SSE event type for JSON-RPC messages
const SSE_MESSAGE_EVENT: &str = "message";

/// Session state for HTTP transport
#[derive(Debug)]
struct Session {
    /// Session ID
    id: String,
    /// The MCP router for this session
    router: McpRouter,
    /// Broadcast channel for SSE notifications
    notifications_tx: broadcast::Sender<String>,
    /// Timestamp of last activity (seconds since UNIX epoch)
    last_accessed: AtomicU64,
}

impl Session {
    fn new(router: McpRouter) -> Self {
        let (notifications_tx, _) = broadcast::channel(100);
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            router,
            notifications_tx,
            last_accessed: AtomicU64::new(current_timestamp_ms()),
        }
    }

    /// Update the last accessed timestamp
    fn touch(&self) {
        self.last_accessed
            .store(current_timestamp_ms(), Ordering::Relaxed);
    }

    /// Check if the session has expired
    fn is_expired(&self, ttl_ms: u64) -> bool {
        let last = self.last_accessed.load(Ordering::Relaxed);
        let now = current_timestamp_ms();
        now.saturating_sub(last) > ttl_ms
    }
}

/// Get current timestamp in milliseconds since UNIX epoch
fn current_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Default session TTL: 30 minutes
pub const DEFAULT_SESSION_TTL_SECS: u64 = 30 * 60;

/// Default maximum number of sessions
pub const DEFAULT_MAX_SESSIONS: usize = 10_000;

/// Session store for managing multiple client sessions
#[derive(Debug)]
struct SessionStore {
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    /// Session time-to-live in milliseconds
    ttl_ms: u64,
    /// Maximum number of sessions
    max_sessions: usize,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            ttl_ms: DEFAULT_SESSION_TTL_SECS * 1000,
            max_sessions: DEFAULT_MAX_SESSIONS,
        }
    }
}

impl SessionStore {
    fn new() -> Self {
        Self::default()
    }

    fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl_ms = ttl.as_millis() as u64;
        self
    }

    fn with_max_sessions(mut self, max: usize) -> Self {
        self.max_sessions = max;
        self
    }

    async fn create(&self, router: McpRouter) -> Option<Arc<Session>> {
        let mut sessions = self.sessions.write().await;

        // Check capacity
        if sessions.len() >= self.max_sessions {
            tracing::warn!(
                max = self.max_sessions,
                current = sessions.len(),
                "Session limit reached, rejecting new session"
            );
            return None;
        }

        let session = Arc::new(Session::new(router));
        sessions.insert(session.id.clone(), session.clone());
        tracing::debug!(session_id = %session.id, total = sessions.len(), "Created new session");
        Some(session)
    }

    async fn get(&self, id: &str) -> Option<Arc<Session>> {
        let sessions = self.sessions.read().await;
        if let Some(session) = sessions.get(id) {
            // Check if expired
            if session.is_expired(self.ttl_ms) {
                tracing::debug!(session_id = %id, "Session expired on access");
                return None;
            }
            // Update last accessed time
            session.touch();
            Some(session.clone())
        } else {
            None
        }
    }

    async fn remove(&self, id: &str) -> bool {
        let mut sessions = self.sessions.write().await;
        let removed = sessions.remove(id).is_some();
        if removed {
            tracing::debug!(session_id = %id, total = sessions.len(), "Removed session");
        }
        removed
    }

    /// Remove all expired sessions
    async fn cleanup_expired(&self) -> usize {
        let mut sessions = self.sessions.write().await;
        let before = sessions.len();
        sessions.retain(|id, session| {
            let expired = session.is_expired(self.ttl_ms);
            if expired {
                tracing::debug!(session_id = %id, "Removing expired session");
            }
            !expired
        });
        let removed = before - sessions.len();
        if removed > 0 {
            tracing::info!(
                removed = removed,
                remaining = sessions.len(),
                "Cleaned up expired sessions"
            );
        }
        removed
    }

    /// Get the number of active sessions
    #[cfg(test)]
    async fn len(&self) -> usize {
        self.sessions.read().await.len()
    }
}

/// Shared state for the HTTP transport
struct AppState {
    /// Template router for creating new sessions
    router_template: McpRouter,
    /// Session store
    sessions: SessionStore,
    /// Whether to validate Origin header
    validate_origin: bool,
    /// Allowed origins (if validation is enabled)
    allowed_origins: Vec<String>,
}

/// HTTP transport for MCP servers
///
/// Implements the Streamable HTTP transport from the MCP specification.
///
/// # Session Management
///
/// Sessions are automatically cleaned up after the configured TTL (default 30 minutes).
/// Use [`session_ttl`](Self::session_ttl) and [`max_sessions`](Self::max_sessions) to configure.
///
/// # Example
///
/// ```rust,no_run
/// # use tower_mcp::{McpRouter, transport::http::HttpTransport};
/// # use std::time::Duration;
/// let transport = HttpTransport::new(McpRouter::new())
///     .session_ttl(Duration::from_secs(60 * 60))  // 1 hour
///     .max_sessions(1000);
/// ```
pub struct HttpTransport {
    router: McpRouter,
    validate_origin: bool,
    allowed_origins: Vec<String>,
    session_ttl: Duration,
    max_sessions: usize,
}

impl HttpTransport {
    /// Create a new HTTP transport wrapping an MCP router
    pub fn new(router: McpRouter) -> Self {
        Self {
            router,
            validate_origin: true,
            allowed_origins: vec![],
            session_ttl: Duration::from_secs(DEFAULT_SESSION_TTL_SECS),
            max_sessions: DEFAULT_MAX_SESSIONS,
        }
    }

    /// Set the session time-to-live
    ///
    /// Sessions that are inactive for longer than this duration will be
    /// automatically removed. Default is 30 minutes.
    pub fn session_ttl(mut self, ttl: Duration) -> Self {
        self.session_ttl = ttl;
        self
    }

    /// Set the maximum number of concurrent sessions
    ///
    /// Once this limit is reached, new session requests will be rejected
    /// with a 503 Service Unavailable response. Default is 10,000.
    pub fn max_sessions(mut self, max: usize) -> Self {
        self.max_sessions = max;
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

    /// Build the axum router for this transport
    ///
    /// This also starts a background task that periodically cleans up expired sessions.
    pub fn into_router(self) -> Router {
        let state = self.create_app_state();

        Router::new()
            .route("/", post(handle_post))
            .route("/", get(handle_get))
            .route("/", delete(handle_delete))
            .with_state(state)
    }

    /// Build an axum router mounted at a specific path
    ///
    /// This also starts a background task that periodically cleans up expired sessions.
    pub fn into_router_at(self, path: &str) -> Router {
        let state = self.create_app_state();

        let mcp_router = Router::new()
            .route("/", post(handle_post))
            .route("/", get(handle_get))
            .route("/", delete(handle_delete))
            .with_state(state);

        Router::new().nest(path, mcp_router)
    }

    /// Create app state and start background cleanup task
    fn create_app_state(self) -> Arc<AppState> {
        let sessions = SessionStore::new()
            .with_ttl(self.session_ttl)
            .with_max_sessions(self.max_sessions);

        let state = Arc::new(AppState {
            router_template: self.router,
            sessions,
            validate_origin: self.validate_origin,
            allowed_origins: self.allowed_origins,
        });

        // Start background session cleanup task
        let cleanup_state = state.clone();
        let cleanup_interval = self.session_ttl / 2; // Run cleanup at half the TTL interval
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(cleanup_interval.max(Duration::from_secs(60)));
            loop {
                interval.tick().await;
                cleanup_state.sessions.cleanup_expired().await;
            }
        });

        state
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
}

/// Validate Origin header for security.
/// Returns Some(Response) if validation fails, None if it passes.
fn validate_origin(headers: &HeaderMap, state: &AppState) -> Option<Response> {
    if !state.validate_origin {
        return None;
    }

    if let Some(origin) = headers.get(header::ORIGIN) {
        let origin_str = origin.to_str().unwrap_or("");

        // If allowed_origins is empty and we're validating, reject all cross-origin requests
        if state.allowed_origins.is_empty() {
            // Allow same-origin (no Origin header means same-origin in most cases)
            // But if Origin is present and we have no whitelist, reject
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

/// Check if the request is an initialize request
fn is_initialize_request(body: &serde_json::Value) -> bool {
    body.get("method")
        .and_then(|m| m.as_str())
        .map(|m| m == "initialize")
        .unwrap_or(false)
}

/// Handle POST requests (JSON-RPC messages from client)
async fn handle_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    // Validate Origin
    if let Some(resp) = validate_origin(&headers, &state) {
        return resp;
    }

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
        match state.sessions.create(state.router_template.clone()).await {
            Some(s) => s,
            None => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Server at capacity, try again later",
                )
                    .into_response();
            }
        }
    } else {
        // Require existing session
        let session_id = match get_session_id(&headers) {
            Some(id) => id,
            None => {
                return (StatusCode::BAD_REQUEST, "Missing MCP-Session-Id header").into_response();
            }
        };

        match state.sessions.get(&session_id).await {
            Some(s) => s,
            None => {
                return (StatusCode::NOT_FOUND, "Session not found or expired").into_response();
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

    // Process the request
    let mut service = JsonRpcService::new(session.router.clone());
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

/// Handle GET requests (SSE stream for server notifications)
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
            return (StatusCode::BAD_REQUEST, "Missing MCP-Session-Id header").into_response();
        }
    };

    let session = match state.sessions.get(&session_id).await {
        Some(s) => s,
        None => {
            return (StatusCode::NOT_FOUND, "Session not found or expired").into_response();
        }
    };

    // Create SSE stream from broadcast channel
    let rx = session.notifications_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result: std::result::Result<String, _>| {
        result
            .ok()
            .map(|msg| Ok::<_, Infallible>(Event::default().event(SSE_MESSAGE_EVENT).data(msg)))
    });

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
            return (StatusCode::BAD_REQUEST, "Missing MCP-Session-Id header").into_response();
        }
    };

    if state.sessions.remove(&session_id).await {
        tracing::info!(session_id = %session_id, "Session terminated");
        StatusCode::OK.into_response()
    } else {
        (StatusCode::NOT_FOUND, "Session not found").into_response()
    }
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

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
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
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_session_expiration() {
        // Create a session store with very short TTL
        let store = SessionStore::new().with_ttl(Duration::from_millis(50));

        let router = create_test_router();
        let session = store.create(router.clone()).await.unwrap();
        let session_id = session.id.clone();

        // Session should be accessible immediately
        assert!(store.get(&session_id).await.is_some());

        // Wait for expiration
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Session should now be expired
        assert!(store.get(&session_id).await.is_none());
    }

    #[tokio::test]
    async fn test_session_touch_extends_lifetime() {
        let store = SessionStore::new().with_ttl(Duration::from_millis(100));

        let router = create_test_router();
        let session = store.create(router.clone()).await.unwrap();
        let session_id = session.id.clone();

        // Wait a bit but not enough to expire
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Access session (should touch and extend lifetime)
        let session = store.get(&session_id).await.unwrap();
        assert_eq!(session.id, session_id);

        // Wait again
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Session should still be valid because we touched it
        assert!(store.get(&session_id).await.is_some());
    }

    #[tokio::test]
    async fn test_max_sessions_limit() {
        let store = SessionStore::new().with_max_sessions(2);

        let router = create_test_router();

        // Create two sessions - should succeed
        let s1 = store.create(router.clone()).await;
        let s2 = store.create(router.clone()).await;
        assert!(s1.is_some());
        assert!(s2.is_some());

        // Third session should fail
        let s3 = store.create(router.clone()).await;
        assert!(s3.is_none());

        // Remove one session
        store.remove(&s1.unwrap().id).await;

        // Now we can create another
        let s4 = store.create(router.clone()).await;
        assert!(s4.is_some());
    }

    #[tokio::test]
    async fn test_cleanup_expired_sessions() {
        let store = SessionStore::new().with_ttl(Duration::from_millis(50));

        let router = create_test_router();

        // Create multiple sessions
        let s1 = store.create(router.clone()).await.unwrap();
        let s2 = store.create(router.clone()).await.unwrap();
        let s3 = store.create(router.clone()).await.unwrap();

        assert_eq!(store.len().await, 3);

        // Wait for expiration
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Touch one session to keep it alive
        s2.touch();

        // Run cleanup
        let removed = store.cleanup_expired().await;

        // Two sessions should have been removed
        assert_eq!(removed, 2);
        assert_eq!(store.len().await, 1);

        // The touched session should still exist
        assert!(store.get(&s2.id).await.is_some());
        assert!(store.get(&s1.id).await.is_none());
        assert!(store.get(&s3.id).await.is_none());
    }

    #[tokio::test]
    async fn test_session_ttl_config() {
        let transport = HttpTransport::new(create_test_router())
            .session_ttl(Duration::from_secs(3600))
            .max_sessions(500);

        // Verify config is stored
        assert_eq!(transport.session_ttl, Duration::from_secs(3600));
        assert_eq!(transport.max_sessions, 500);
    }
}
