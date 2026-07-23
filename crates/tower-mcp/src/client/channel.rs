//! In-process channel transport for connecting an [`McpClient`] to an [`McpRouter`].
//!
//! This transport bridges client and server in the same process without
//! any network or subprocess overhead. It is useful for testing (e.g.,
//! proxy tests) and for in-process composition, where a co-located client
//! (a REPL, an editor integration, an orchestrator) drives a router living
//! in the same process.
//!
//! Server notifications emitted through the router's notification sender
//! (progress, log messages, list-changed) are serialized into JSON-RPC
//! notification frames and interleaved into [`recv`](ClientTransport::recv),
//! so a [`NotificationHandler`](crate::client::NotificationHandler) works
//! identically to the network transports. Requests are processed
//! concurrently: a slow tool call does not block other requests on the
//! transport (the client correlates responses by request id).
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
//!
//! # Host-pushed notifications
//!
//! When the host process wants to push notifications from its own tasks
//! (mirroring [`HttpTransport::with_notifications`]), it keeps the sender
//! and hands the receiver to the transport:
//!
//! ```rust,no_run
//! use tower_mcp::client::{McpClient, ChannelTransport};
//! use tower_mcp::context::notification_channel;
//! use tower_mcp::McpRouter;
//!
//! # async fn example() -> Result<(), tower_mcp::BoxError> {
//! let (notif_tx, notif_rx) = notification_channel(64);
//! let router = McpRouter::new()
//!     .server_info("backend", "1.0.0")
//!     .with_notification_sender(notif_tx.clone());
//!
//! let transport = ChannelTransport::with_notifications(router, notif_rx);
//! let client = McpClient::connect(transport).await?;
//!
//! // Elsewhere in the host process:
//! // notif_tx.send(ServerNotification::ToolsListChanged).await.ok();
//! # Ok(())
//! # }
//! ```
//!
//! [`HttpTransport::with_notifications`]: crate::transport::HttpTransport::with_notifications

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::context::{NotificationReceiver, notification_channel};
use crate::error::Result;
use crate::jsonrpc::JsonRpcService;
use crate::protocol::{JsonRpcRequest, JsonRpcResponse, McpNotification};
use crate::router::McpRouter;

use super::transport::ClientTransport;

/// An in-process [`ClientTransport`] that connects directly to an [`McpRouter`].
///
/// Messages are passed through tokio channels: background tasks feed
/// incoming JSON-RPC requests to a [`JsonRpcService<McpRouter>`] (one spawned
/// task per request, so calls run concurrently) and pump server
/// notifications into the response stream.
pub struct ChannelTransport {
    /// Send raw JSON messages to the server task.
    request_tx: mpsc::Sender<String>,
    /// Receive raw JSON responses and notification frames from the server tasks.
    response_rx: mpsc::Receiver<String>,
    connected: bool,
}

impl ChannelTransport {
    /// Create a new channel transport backed by the given router.
    ///
    /// Wires an internal notification channel into the router, so
    /// notifications emitted during request handling (progress, log
    /// messages, list-changed) are delivered to the client. To push
    /// notifications from the host process's own tasks, use
    /// [`with_notifications`](Self::with_notifications) instead.
    ///
    /// Note: this overwrites any notification sender previously set on the
    /// router, matching the transport-owns-the-channel behavior of
    /// [`HttpTransport::new`](crate::transport::HttpTransport::new).
    pub fn new(router: McpRouter) -> Self {
        let (notification_tx, notification_rx) = notification_channel(64);
        let router = router.with_notification_sender(notification_tx);
        Self::with_notifications(router, notification_rx)
    }

    /// Create a channel transport with a caller-owned notification receiver.
    ///
    /// Mirrors [`HttpTransport::with_notifications`]: the host process keeps
    /// the sender (its own clone from [`notification_channel`], or via
    /// [`McpRouter::notification_sender`]) and pushes
    /// [`ServerNotification`](crate::context::ServerNotification)s from its
    /// own tasks; the transport serializes them into JSON-RPC notification
    /// frames and interleaves them into [`recv`](ClientTransport::recv).
    ///
    /// The router passed here should already carry the matching sender (see
    /// the module-level example) so notifications emitted during request
    /// handling flow through the same channel.
    ///
    /// [`HttpTransport::with_notifications`]: crate::transport::HttpTransport::with_notifications
    pub fn with_notifications(
        router: McpRouter,
        mut notification_rx: NotificationReceiver,
    ) -> Self {
        let (request_tx, mut request_rx) = mpsc::channel::<String>(64);
        let (response_tx, response_rx) = mpsc::channel::<String>(64);

        // Notification pump: serialize ServerNotifications into JSON-RPC
        // notification frames on the shared response stream.
        let notification_out = response_tx.clone();
        tokio::spawn(async move {
            while let Some(notification) = notification_rx.recv().await {
                if let Some(json) = crate::transport::stdio::serialize_notification(&notification)
                    && notification_out.send(json).await.is_err()
                {
                    break; // Client dropped
                }
            }
        });

        let service = JsonRpcService::new(router.clone());

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

                // Process each request in its own task so a slow call does
                // not block the transport. The client correlates responses
                // by request id, so completion order does not matter.
                let mut service = service.clone();
                let response_out = response_tx.clone();
                tokio::spawn(async move {
                    let response = service.call_single(req).await;

                    let json = match response {
                        Ok(resp) => match serde_json::to_string(&resp) {
                            Ok(j) => j,
                            Err(e) => {
                                tracing::error!(
                                    "ChannelTransport: failed to serialize response: {}",
                                    e
                                );
                                return;
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
                                Err(_) => return,
                            }
                        }
                    };

                    // Best effort: if the client dropped, the send fails and
                    // the task simply ends.
                    let _ = response_out.send(json).await;
                });
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
