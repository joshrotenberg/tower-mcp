//! WebSocket transport for MCP
//!
//! Provides full-duplex communication over WebSocket, ideal for:
//! - Bidirectional notifications
//! - Long-lived connections
//! - Lower latency than HTTP polling
//!
//! # Example
//!
//! ```rust,no_run
//! use tower_mcp::{McpRouter, ToolBuilder, CallToolResult};
//! use tower_mcp::transport::websocket::WebSocketTransport;
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
//!     let transport = WebSocketTransport::new(router);
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
use tokio::sync::RwLock;

use crate::error::{Error, JsonRpcError, Result};
use crate::jsonrpc::JsonRpcService;
use crate::protocol::{JsonRpcMessage, JsonRpcNotification, JsonRpcResponse, McpNotification};
use crate::router::McpRouter;

/// Session state for WebSocket transport
#[derive(Debug)]
struct Session {
    id: String,
    router: McpRouter,
}

impl Session {
    fn new(router: McpRouter) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            router,
        }
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

    async fn create(&self, router: McpRouter) -> Arc<Session> {
        let session = Arc::new(Session::new(router));
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

/// Shared state for WebSocket transport
struct AppState {
    router_template: McpRouter,
    sessions: SessionStore,
}

/// WebSocket transport for MCP servers
///
/// Provides full-duplex communication over WebSocket.
pub struct WebSocketTransport {
    router: McpRouter,
}

impl WebSocketTransport {
    /// Create a new WebSocket transport
    pub fn new(router: McpRouter) -> Self {
        Self { router }
    }

    /// Build the axum router for this transport
    pub fn into_router(self) -> Router {
        let state = Arc::new(AppState {
            router_template: self.router,
            sessions: SessionStore::new(),
        });

        Router::new()
            .route("/", get(handle_websocket))
            .with_state(state)
    }

    /// Build an axum router mounted at a specific path
    pub fn into_router_at(self, path: &str) -> Router {
        let state = Arc::new(AppState {
            router_template: self.router,
            sessions: SessionStore::new(),
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
    let session = state.sessions.create(state.router_template.clone()).await;
    let session_id = session.id.clone();

    tracing::info!(session_id = %session_id, "WebSocket connection established");

    let mut service = JsonRpcService::new(session.router.clone());
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

    // Cleanup session
    state.sessions.remove(&session_id).await;
    tracing::info!(session_id = %session_id, "WebSocket connection closed");
}

/// Process a JSON-RPC message
async fn process_message(
    service: &mut JsonRpcService<McpRouter>,
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
}
