//! In-process channel transport for connecting an [`McpClient`] to an [`McpRouter`].
//!
//! This transport bridges client and server in the same process without
//! any network or subprocess overhead. It is primarily useful for testing
//! (e.g., proxy tests) but can also be used for in-process composition.
//!
//! # Example
//!
//! ```rust,no_run
//! use tower_mcp::client::{McpClient, ChannelTransport};
//! use tower_mcp::McpRouter;
//!
//! # async fn example() -> Result<(), tower_mcp::BoxError> {
//! let router = McpRouter::new().server_info("backend", "1.0.0");
//! let transport = ChannelTransport::new(router);
//! let client = McpClient::connect(transport).await?;
//! client.initialize("my-client", "1.0.0").await?;
//! # Ok(())
//! # }
//! ```

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::context::notification_channel;
use crate::error::Result;
use crate::jsonrpc::JsonRpcService;
use crate::protocol::{JsonRpcRequest, JsonRpcResponse, McpNotification};
use crate::router::McpRouter;

use super::transport::ClientTransport;

/// An in-process [`ClientTransport`] that connects directly to an [`McpRouter`].
///
/// Messages are passed through tokio channels — the transport spawns a
/// background task that feeds incoming JSON-RPC requests to a
/// [`JsonRpcService<McpRouter>`] and returns responses.
pub struct ChannelTransport {
    /// Send raw JSON messages to the server task.
    request_tx: mpsc::Sender<String>,
    /// Receive raw JSON responses from the server task.
    response_rx: mpsc::Receiver<String>,
    connected: bool,
}

impl ChannelTransport {
    /// Create a new channel transport backed by the given router.
    ///
    /// Spawns a background tokio task that processes requests.
    pub fn new(router: McpRouter) -> Self {
        let (request_tx, mut request_rx) = mpsc::channel::<String>(64);
        let (response_tx, response_rx) = mpsc::channel::<String>(64);

        let (notification_tx, _notification_rx) = notification_channel(64);
        let router = router.with_notification_sender(notification_tx);
        let mut service = JsonRpcService::new(router.clone());

        tokio::spawn(async move {
            while let Some(raw_request) = request_rx.recv().await {
                // Parse the incoming JSON
                let req: JsonRpcRequest = match serde_json::from_str(&raw_request) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!("ChannelTransport: failed to parse request: {}", e);
                        continue;
                    }
                };

                // Check for initialized notification embedded as a request
                // (McpClient sends notifications as JSON-RPC messages)
                if req.method == "notifications/initialized" {
                    router.handle_notification(McpNotification::Initialized);
                    // No response for notifications
                    continue;
                }

                // Handle other notifications (no response expected)
                if req.method.starts_with("notifications/") {
                    continue;
                }

                // Process the request through JsonRpcService
                let response = service.call_single(req).await;

                let json = match response {
                    Ok(resp) => match serde_json::to_string(&resp) {
                        Ok(j) => j,
                        Err(e) => {
                            tracing::error!(
                                "ChannelTransport: failed to serialize response: {}",
                                e
                            );
                            continue;
                        }
                    },
                    Err(e) => {
                        // Convert error to a JSON-RPC error response
                        let err_resp = JsonRpcResponse::error(
                            None,
                            tower_mcp_types::JsonRpcError::internal_error(e.to_string()),
                        );
                        match serde_json::to_string(&err_resp) {
                            Ok(j) => j,
                            Err(_) => continue,
                        }
                    }
                };

                if response_tx.send(json).await.is_err() {
                    break; // Client dropped
                }
            }
        });

        Self {
            request_tx,
            response_rx,
            connected: true,
        }
    }
}

#[async_trait]
impl ClientTransport for ChannelTransport {
    async fn send(&mut self, message: &str) -> Result<()> {
        self.request_tx
            .send(message.to_string())
            .await
            .map_err(|_| crate::error::Error::internal("ChannelTransport: server task dropped"))?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<String>> {
        match self.response_rx.recv().await {
            Some(msg) => Ok(Some(msg)),
            None => {
                self.connected = false;
                Ok(None)
            }
        }
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    async fn close(&mut self) -> Result<()> {
        self.connected = false;
        Ok(())
    }
}
