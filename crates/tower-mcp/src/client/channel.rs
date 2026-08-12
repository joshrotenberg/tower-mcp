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

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use crate::context::{
    ChannelClientRequester, ClientRequesterHandle, NotificationReceiver, OutgoingRequestReceiver,
    notification_channel, outgoing_request_channel,
};
use crate::error::Result;
use crate::jsonrpc::JsonRpcService;
use crate::protocol::{JsonRpcRequest, JsonRpcResponse, McpNotification, RequestId};
use crate::router::{McpRouter, RouterRequest, RouterResponse};
use crate::transport::service::{CatchError, InjectAnnotations};
#[cfg(feature = "stateless")]
use crate::transport::stdio::{StdioSubscriptionInput, StdioSubscriptions};
use tower_service::Service;

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

    /// Attach a client requester so handlers can call back to the client.
    ///
    /// Server-initiated requests (elicitation, sampling) need a route from
    /// the handler back out to the client. The stdio, HTTP, and WebSocket
    /// transports build one; this does the same for the in-process path, and
    /// must run before the router is wrapped by a service or layer.
    fn with_client_requester(router: McpRouter) -> (McpRouter, OutgoingRequestReceiver) {
        let (request_tx, request_rx) = outgoing_request_channel(32);
        let requester: ClientRequesterHandle = Arc::new(ChannelClientRequester::new(request_tx));
        (router.with_client_requester(requester), request_rx)
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
    pub fn with_notifications(router: McpRouter, notification_rx: NotificationReceiver) -> Self {
        let (router, request_rx) = Self::with_client_requester(router);
        let service = JsonRpcService::new(router.clone());
        Self::spawn_with_service(router, service, notification_rx, request_rx)
    }

    /// Create a channel transport whose dispatch runs through a Tower layer.
    ///
    /// The channel counterpart of [`StdioTransport::layer`]: the layer wraps
    /// the router's dispatch service, so standard middleware (timeout, rate
    /// limit, tracing, audit) observes every JSON-RPC request an
    /// [`McpClient`](crate::client::McpClient) makes in-process, exactly as
    /// it would over stdio or HTTP. Layers that produce errors are wrapped
    /// with [`CatchError`] and tool-annotation injection is preserved.
    ///
    /// `subscriptions/listen` remains transport-owned on every transport and
    /// does not pass through the layer (#1182 tracks that boundary).
    ///
    /// [`StdioTransport::layer`]: crate::transport::StdioTransport::layer
    pub fn layer<L>(router: McpRouter, layer: L) -> Self
    where
        L: tower::Layer<McpRouter>,
        L::Service: Service<RouterRequest, Response = RouterResponse> + Clone + Send + 'static,
        <L::Service as Service<RouterRequest>>::Error: std::fmt::Display + Send,
        <L::Service as Service<RouterRequest>>::Future: Send,
    {
        let (notification_tx, notification_rx) = notification_channel(64);
        let router = router.with_notification_sender(notification_tx);
        Self::layer_with_notifications(router, layer, notification_rx)
    }

    /// [`layer`](Self::layer) with a caller-owned notification receiver, the
    /// layered counterpart of [`with_notifications`](Self::with_notifications).
    pub fn layer_with_notifications<L>(
        router: McpRouter,
        layer: L,
        notification_rx: NotificationReceiver,
    ) -> Self
    where
        L: tower::Layer<McpRouter>,
        L::Service: Service<RouterRequest, Response = RouterResponse> + Clone + Send + 'static,
        <L::Service as Service<RouterRequest>>::Error: std::fmt::Display + Send,
        <L::Service as Service<RouterRequest>>::Future: Send,
    {
        let (router, request_rx) = Self::with_client_requester(router);
        let annotations = router.tool_annotations_map();
        let wrapped = layer.layer(router.clone());
        let service = InjectAnnotations::new(CatchError::new(wrapped), annotations);
        Self::spawn_with_service(
            router,
            JsonRpcService::new(service),
            notification_rx,
            request_rx,
        )
    }

    /// Spawn the request and notification loops over an arbitrary dispatch
    /// service.
    ///
    /// The router is retained alongside the service for transport metadata
    /// only: server identity and tasks opt-in for subscription handling, and
    /// the `notifications/initialized` forward. Requests dispatch through
    /// `service`, never through the router directly, so a wrapping layer sees
    /// every request.
    fn spawn_with_service<S>(
        router: McpRouter,
        service: JsonRpcService<S>,
        mut notification_rx: NotificationReceiver,
        mut outgoing_rx: OutgoingRequestReceiver,
    ) -> Self
    where
        S: Service<RouterRequest, Response = RouterResponse, Error = std::convert::Infallible>
            + Clone
            + Send
            + 'static,
        S::Future: Send,
    {
        let (request_tx, mut request_rx) = mpsc::channel::<String>(64);
        let (response_tx, response_rx) = mpsc::channel::<String>(64);

        // Server-initiated requests awaiting a client reply, keyed by the id
        // the requester allocated.
        type PendingClientRequests = Arc<
            StdMutex<HashMap<RequestId, tokio::sync::oneshot::Sender<Result<serde_json::Value>>>>,
        >;
        let pending: PendingClientRequests = Arc::new(StdMutex::new(HashMap::new()));

        // Outgoing request pump: serialize each server-initiated request onto
        // the shared stream and park its responder until the client answers.
        let outgoing_out = response_tx.clone();
        let outgoing_pending = pending.clone();
        tokio::spawn(async move {
            while let Some(outgoing) = outgoing_rx.recv().await {
                let frame = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": outgoing.id,
                    "method": outgoing.method,
                    "params": outgoing.params,
                });
                let Ok(json) = serde_json::to_string(&frame) else {
                    let _ = outgoing.response_tx.send(Err(crate::error::Error::internal(
                        "ChannelTransport: failed to serialize server request",
                    )));
                    continue;
                };
                if let Ok(mut pending) = outgoing_pending.lock() {
                    pending.insert(outgoing.id.clone(), outgoing.response_tx);
                } else {
                    continue;
                }
                if outgoing_out.send(json).await.is_err() {
                    // The client is gone; fail the request rather than leave
                    // the handler awaiting a reply that cannot arrive.
                    if let Ok(mut pending) = outgoing_pending.lock()
                        && let Some(responder) = pending.remove(&outgoing.id)
                    {
                        let _ = responder.send(Err(crate::error::Error::internal(
                            "ChannelTransport: client disconnected",
                        )));
                    }
                    return;
                }
            }
        });

        #[cfg(feature = "stateless")]
        let subscriptions = Arc::new(StdMutex::new(
            StdioSubscriptions::new(Some(router.implementation()))
                .with_observer(router.subscription_observer()),
        ));

        // Notification pump: serialize ServerNotifications into JSON-RPC
        // notification frames on the shared response stream.
        let notification_out = response_tx.clone();
        #[cfg(feature = "stateless")]
        let notification_subscriptions = subscriptions.clone();
        tokio::spawn(async move {
            while let Some(notification) = notification_rx.recv().await {
                #[cfg(feature = "stateless")]
                {
                    let frames = notification_subscriptions
                        .lock()
                        .ok()
                        .and_then(|subscriptions| subscriptions.route_notification(&notification));
                    if let Some(frames) = frames {
                        for frame in frames {
                            if notification_out.send(frame).await.is_err() {
                                return;
                            }
                        }
                        continue;
                    }
                }
                if let Some(json) = crate::transport::stdio::serialize_notification(&notification)
                    && notification_out.send(json).await.is_err()
                {
                    break; // Client dropped
                }
            }
        });

        tokio::spawn(async move {
            while let Some(raw_request) = request_rx.recv().await {
                // Notifications carry no `id`, so they cannot parse as
                // JsonRpcRequest (whose id is required). Inspect the raw
                // frame first and handle them by method.
                let parsed: serde_json::Value = match serde_json::from_str(&raw_request) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!("ChannelTransport: failed to parse frame: {}", e);
                        continue;
                    }
                };
                #[cfg(feature = "stateless")]
                {
                    // The registry lock is never held across an await: the
                    // dispatch through the service happens between the
                    // handle_input and complete_listen critical sections.
                    let handled = subscriptions
                        .lock()
                        .ok()
                        .map(|mut subscriptions| subscriptions.handle_input(&service, &parsed));
                    match handled {
                        Some(Ok(StdioSubscriptionInput::Handled(frames))) => {
                            for frame in frames {
                                if response_tx.send(frame).await.is_err() {
                                    return;
                                }
                            }
                            continue;
                        }
                        Some(Ok(StdioSubscriptionInput::Dispatch(request))) => {
                            let request_id = request.id.clone();
                            let response = crate::transport::stdio::dispatch_listen_request(
                                &mut service.clone(),
                                *request,
                                request_id.clone(),
                            )
                            .await;
                            let frames = subscriptions.lock().ok().map(|mut subscriptions| {
                                subscriptions.complete_listen(request_id, &response)
                            });
                            if let Some(Ok(frames)) = frames {
                                for frame in frames {
                                    if response_tx.send(frame).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            continue;
                        }
                        _ => {}
                    }
                }
                // A reply to a server-initiated request: it carries an id
                // and a result or error but no method. This must be checked
                // before the request parse below, which such a frame cannot
                // satisfy (JsonRpcRequest requires `method`).
                if parsed.get("id").is_some() && crate::framing::is_response_frame(&parsed) {
                    if let Ok(id) = serde_json::from_value::<RequestId>(parsed["id"].clone()) {
                        let responder = pending.lock().ok().and_then(|mut p| p.remove(&id));
                        if let Some(responder) = responder {
                            let result = if let Some(error) = parsed.get("error") {
                                Err(crate::error::Error::internal(format!(
                                    "Client error: {error}"
                                )))
                            } else {
                                Ok(parsed
                                    .get("result")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null))
                            };
                            let _ = responder.send(result);
                        } else {
                            tracing::warn!(?id, "ChannelTransport: reply for unknown request");
                        }
                    }
                    continue;
                }

                if parsed.get("id").is_none() {
                    if parsed.get("method").and_then(|m| m.as_str())
                        == Some("notifications/initialized")
                    {
                        router.handle_notification(McpNotification::Initialized);
                    }
                    // No response for notifications
                    continue;
                }

                let req: JsonRpcRequest = match serde_json::from_value(parsed) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!("ChannelTransport: failed to parse request: {}", e);
                        continue;
                    }
                };

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

            // The client dropped its sender: any registered streams die with
            // the transport and cannot receive a terminal frame.
            #[cfg(feature = "stateless")]
            if let Ok(mut subscriptions) = subscriptions.lock() {
                subscriptions.drain_disconnected();
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
