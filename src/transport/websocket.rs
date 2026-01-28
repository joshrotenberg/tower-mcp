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
//!         .build()?;
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
//! Use [`WebSocketTransport::with_sampling`] to enable this feature:
//!
//! ```rust,no_run
//! use tower_mcp::{BoxError, McpRouter, ToolBuilder, CallToolResult, CreateMessageParams, SamplingMessage};
//! use tower_mcp::context::RequestContext;
//! use tower_mcp::transport::websocket::WebSocketTransport;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), BoxError> {
//!     let tool = ToolBuilder::new("ai-tool")
//!         .handler_with_context(|ctx: RequestContext, _: serde_json::Value| async move {
//!             // Request LLM completion from client
//!             let params = CreateMessageParams::new(
//!                 vec![SamplingMessage::user("Summarize this...")],
//!                 500,
//!             );
//!             let result = ctx.sample(params).await?;
//!             Ok(CallToolResult::text(format!("{:?}", result.content)))
//!         })
//!         .build()?;
//!
//!     let router = McpRouter::new()
//!         .server_info("my-server", "1.0.0")
//!         .tool(tool);
//!
//!     let transport = WebSocketTransport::with_sampling(router);
//!     transport.serve("127.0.0.1:3000").await?;
//!     Ok(())
//! }
//! ```

use std::collections::HashMap;
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
use tokio::sync::{Mutex, RwLock};

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
use crate::transport::service::{CatchError, McpBoxService, ServiceFactory, identity_factory};

/// Session state for WebSocket transport
struct Session {
    id: String,
    router: McpRouter,
    service_factory: ServiceFactory,
}

impl Session {
    fn new(router: McpRouter, service_factory: ServiceFactory) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            router,
            service_factory,
        }
    }

    /// Create a middleware-wrapped service from this session's router.
    fn make_service(&self) -> McpBoxService {
        (self.service_factory)(self.router.clone())
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

    async fn create(&self, router: McpRouter, service_factory: ServiceFactory) -> Arc<Session> {
        let session = Arc::new(Session::new(router, service_factory));
        let mut sessions = self.sessions.write().await;
        sessions.insert(session.id.clone(), session.clone());
        tracing::debug!(session_id = %session.id, "Created WebSocket session");
        session
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
    service_factory: ServiceFactory,
    sessions: SessionStore,
    /// Whether sampling is enabled
    sampling_enabled: bool,
}

/// WebSocket transport for MCP servers
///
/// Provides full-duplex communication over WebSocket.
pub struct WebSocketTransport {
    router: McpRouter,
    sampling_enabled: bool,
    service_factory: ServiceFactory,
}

impl WebSocketTransport {
    /// Create a new WebSocket transport
    pub fn new(router: McpRouter) -> Self {
        Self {
            router,
            sampling_enabled: false,
            service_factory: identity_factory(),
        }
    }

    /// Create a new WebSocket transport with sampling support enabled
    ///
    /// When sampling is enabled, tool handlers can use `ctx.sample()` to
    /// request LLM completions from connected clients.
    pub fn with_sampling(router: McpRouter) -> Self {
        Self {
            router,
            sampling_enabled: true,
            service_factory: identity_factory(),
        }
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
    /// use tower::timeout::TimeoutLayer;
    /// use tower_mcp::McpRouter;
    /// use tower_mcp::transport::websocket::WebSocketTransport;
    ///
    /// let router = McpRouter::new().server_info("my-server", "1.0.0");
    /// let transport = WebSocketTransport::new(router)
    ///     .layer(TimeoutLayer::new(Duration::from_secs(30)));
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

    /// Build the axum router for this transport
    pub fn into_router(self) -> Router {
        let state = Arc::new(AppState {
            router_template: self.router,
            service_factory: self.service_factory,
            sessions: SessionStore::new(),
            sampling_enabled: self.sampling_enabled,
        });

        Router::new()
            .route("/", get(handle_websocket))
            .with_state(state)
    }

    /// Build an axum router mounted at a specific path
    pub fn into_router_at(self, path: &str) -> Router {
        let state = Arc::new(AppState {
            router_template: self.router,
            service_factory: self.service_factory,
            sessions: SessionStore::new(),
            sampling_enabled: self.sampling_enabled,
        });

        let ws_router = Router::new()
            .route("/", get(handle_websocket))
            .with_state(state);

        Router::new().nest(path, ws_router)
    }

    /// Serve the transport on the given address
    pub async fn serve(self, addr: &str) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| Error::Transport(format!("Failed to bind to {}: {}", addr, e)))?;

        tracing::info!("MCP WebSocket transport listening on {}", addr);

        let router = self.into_router();
        axum::serve(listener, router)
            .await
            .map_err(|e| Error::Transport(format!("Server error: {}", e)))?;

        Ok(())
    }
}

/// Handle WebSocket upgrade
async fn handle_websocket(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Handle an individual WebSocket connection
async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let session = state
        .sessions
        .create(state.router_template.clone(), state.service_factory.clone())
        .await;
    let session_id = session.id.clone();

    tracing::info!(session_id = %session_id, "WebSocket connection established");

    if state.sampling_enabled {
        handle_socket_bidirectional(socket, session, &session_id).await;
    } else {
        handle_socket_simple(socket, session, &session_id).await;
    }

    // Cleanup session
    state.sessions.remove(&session_id).await;
    tracing::info!(session_id = %session_id, "WebSocket connection closed");
}

/// Handle WebSocket connection without sampling (simple mode)
async fn handle_socket_simple(socket: WebSocket, session: Arc<Session>, session_id: &str) {
    let mut service = JsonRpcService::new(session.make_service());
    let (mut sender, mut receiver) = socket.split();

    // Process incoming messages
    while let Some(msg) = receiver.next().await {
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
            Message::Binary(data) => {
                // Try to parse binary as UTF-8 text
                if let Ok(text) = String::from_utf8(data.to_vec()) {
                    match process_message(&mut service, &session.router, &text).await {
                        Ok(Some(response)) => {
                            if let Ok(json) = serde_json::to_string(&response) {
                                let _ = sender.send(Message::Text(json.into())).await;
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::error!(error = %e, "Error processing binary message");
                        }
                    }
                }
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
async fn handle_socket_bidirectional(socket: WebSocket, session: Arc<Session>, session_id: &str) {
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
    let mut service = JsonRpcService::new((session.service_factory)(router.clone()));

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
                    Message::Binary(data) => {
                        if let Ok(text) = String::from_utf8(data.to_vec()) {
                            let result = handle_incoming_message(
                                &text,
                                &mut service,
                                &router,
                                pending_requests.clone(),
                                sender.clone(),
                            ).await;
                            if let Err(e) = result {
                                tracing::error!(error = %e, "Error handling binary message");
                            }
                        }
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

    // Check if this is a response to one of our pending requests
    if parsed.get("method").is_none()
        && (parsed.get("result").is_some() || parsed.get("error").is_some())
    {
        return handle_response(&parsed, pending_requests).await;
    }

    // Check if it's a notification (no id field)
    if parsed.get("id").is_none() {
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
    // Check if it's a notification (no id field)
    let parsed: serde_json::Value = serde_json::from_str(text)?;
    if parsed.get("id").is_none()
        && let Ok(notification) = serde_json::from_str::<JsonRpcNotification>(text)
    {
        let mcp_notification = McpNotification::from_jsonrpc(&notification)?;
        router.handle_notification(mcp_notification);
        return Ok(None);
    }

    // Parse and process as a request
    let message: JsonRpcMessage = serde_json::from_str(text)?;
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
}
