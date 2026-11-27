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

use std::sync::Arc;
#[cfg(feature = "stateless")]
use std::sync::Mutex as StdMutex;

use crate::context::{
    ChannelClientRequester, ClientRequesterHandle, NotificationReceiver, OutgoingRequestReceiver,
    notification_channel, outgoing_request_channel,
};
use crate::error::Result;
use crate::jsonrpc::JsonRpcService;
use crate::protocol::{JsonRpcRequest, JsonRpcResponse, McpNotification, RequestId, notifications};
use crate::router::{McpRouter, RouterRequest, RouterResponse};
use crate::transport::pending::{
    PendingClientRequests, abandon_channel, cancelled_notification_json,
};
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
///
/// # Cancellation
///
/// A client's `notifications/cancelled` is forwarded to the router, so the
/// handler of the request it names observes cancellation through its
/// [`RequestContext`](crate::RequestContext), and the request is then left
/// unanswered, as MCP asks. Each channel tracks its requests in its own
/// registry, so a cancellation reaches only requests sent on the same
/// channel, even when several channels serve one router.
///
/// A request can be cancelled while the router has it registered: from when
/// the router registers the request's context until it unregisters the
/// request's dispatch. A cancellation that arrives outside that window is
/// ignored, including one for a request still making its way through
/// middleware or not yet registered by the router.
///
/// Only a `notifications/cancelled` frame whose `jsonrpc` is `"2.0"` is
/// acted on; any other is ignored.
///
/// A cancellation that names an active `subscriptions/listen` stream closes
/// that stream. When the id is shared by an active stream and a registered
/// request on the same channel, the cancellation reaches both.
///
/// In the other direction, a server-to-client request
/// (sampling, elicitation) that its handler abandons after it was sent is
/// cancelled with `notifications/cancelled` on the response stream.
pub struct ChannelTransport {
    /// Send raw JSON messages to the server task.
    request_tx: mpsc::Sender<String>,
    /// Receive raw JSON responses and notification frames from the server tasks.
    response_rx: mpsc::Receiver<String>,
    connected: bool,
    /// Fired by [`close`](ClientTransport::close): stops the server tasks and
    /// releases handlers waiting on this client.
    closed: tokio_util::sync::CancellationToken,
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

    /// Prepare the router this channel serves. Must run before the router is
    /// wrapped by a service or layer, so the reader and the dispatch service
    /// hold the same prepared router.
    ///
    /// Attaches a client requester so handlers can call back to the client:
    /// server-initiated requests (elicitation, sampling) need a route from
    /// the handler back out to the client. The stdio, HTTP, and WebSocket
    /// transports build one; this does the same for the in-process path.
    ///
    /// Also gives the channel its own in-flight registry, so a client's
    /// `notifications/cancelled` cannot reach a request with the same id on
    /// another channel built from the same router (#1710).
    fn connection_router(router: McpRouter) -> (McpRouter, OutgoingRequestReceiver) {
        let (request_tx, request_rx) = outgoing_request_channel(32);
        let requester: ClientRequesterHandle = Arc::new(ChannelClientRequester::new(request_tx));
        let router = router.with_own_in_flight().with_client_requester(requester);
        (router, request_rx)
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
        let (router, request_rx) = Self::connection_router(router);
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
    /// The transport owns `subscriptions/listen` streams: it registers them,
    /// filters notifications into them, and closes them. The listen request
    /// itself is dispatched through the layer, so middleware sees it (#1182).
    /// The channel waits for that dispatch before it reads the next frame, so
    /// a slow layer delays the frames, including notifications, that follow
    /// a listen request.
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
        let (router, request_rx) = Self::connection_router(router);
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
    /// the client notification forward. Requests dispatch through
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
        let closed = tokio_util::sync::CancellationToken::new();

        // Server-initiated requests awaiting a client reply, keyed by the id
        // the requester allocated.
        let pending = PendingClientRequests::new();

        // Outgoing request pump: serialize each server-initiated request onto
        // the shared stream and park its responder until the client answers.
        // The same task emits `notifications/cancelled` for requests whose
        // waiter gave up (#1705), so a cancellation never overtakes the
        // request it cancels.
        let outgoing_out = response_tx.clone();
        let outgoing_pending = pending.clone();
        let (abandon_tx, mut abandon_rx) = abandon_channel();
        let pump_closed = closed.clone();
        tokio::spawn(async move {
            // However the pump ends, release every handler waiting on the
            // client.
            let _release_pending = outgoing_pending.clear_on_drop();
            loop {
                tokio::select! {
                    // Cancellations first: one already reported goes out
                    // before any request queued after it.
                    biased;
                    // The client dropped its end; nothing can answer.
                    () = outgoing_out.closed() => return,
                    // The client called `close()`.
                    () = pump_closed.cancelled() => return,
                    Some(id) = abandon_rx.recv() => {
                        if let Some(json) = cancelled_notification_json(&id)
                            && outgoing_out.send(json).await.is_err()
                        {
                            return;
                        }
                    }
                    outgoing = outgoing_rx.recv() => {
                        let Some(outgoing) = outgoing else { break };
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
                        if !outgoing_pending.register(
                            outgoing.id.clone(),
                            outgoing.response_tx,
                            abandon_tx.clone(),
                        ) {
                            continue;
                        }
                        if outgoing_out.send(json).await.is_err() {
                            // The client is gone; fail the request rather than
                            // leave the handler awaiting a reply that cannot
                            // arrive.
                            outgoing_pending.fail(
                                std::slice::from_ref(&outgoing.id),
                                "ChannelTransport: client disconnected",
                            );
                            return;
                        }
                    }
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

        let reader_closed = closed.clone();
        tokio::spawn(async move {
            loop {
                let raw_request = tokio::select! {
                    raw_request = request_rx.recv() => raw_request,
                    () = reader_closed.cancelled() => None,
                };
                let Some(raw_request) = raw_request else {
                    break;
                };
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
                // Gate both cancellation lookups (the listen-stream registry
                // and the router) on a JSON-RPC 2.0 envelope. The shared
                // parser accepts any string `jsonrpc`, and stdio keeps that
                // behavior, so the check lives here.
                if is_non_2_0_cancellation(&parsed) {
                    tracing::debug!(
                        "ChannelTransport: ignoring a notifications/cancelled that is not JSON-RPC 2.0"
                    );
                    continue;
                }
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
                            // A handled notification is a
                            // `notifications/cancelled` that closed a listen
                            // stream. It still goes to the router below: an
                            // ordinary request in flight under the same id
                            // is cancelled too, rather than the stream
                            // consuming the cancellation.
                            if parsed.get("id").is_some() {
                                continue;
                            }
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
                        pending.complete(&id, result).log_undelivered(&id);
                    }
                    continue;
                }

                if parsed.get("id").is_none() {
                    // Every client notification goes to the router, as on
                    // stdio: `notifications/cancelled` reaches the in-flight
                    // request it names (#1710), and the rest are the
                    // router's to handle or log. Handled inline, so a
                    // cancellation does not wait for a spawned request to
                    // finish. It can still wait behind a
                    // `subscriptions/listen` request, which this loop
                    // dispatches inline above.
                    forward_notification(&router, &parsed);
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
                    let response = service.call_single_answerable(req).await;

                    let json = match response {
                        // The client cancelled it; MCP asks us not to answer
                        // (#1547).
                        Ok(None) => return,
                        Ok(Some(resp)) => match serde_json::to_string(&resp) {
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

            // The client dropped its sender or closed the transport, so no
            // reply can arrive: release every handler still waiting on one.
            pending.clear();

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
            closed,
        }
    }
}

/// Whether a frame is a `notifications/cancelled` notification whose
/// `jsonrpc` is anything other than `"2.0"`, including missing.
fn is_non_2_0_cancellation(frame: &serde_json::Value) -> bool {
    frame.get("id").is_none()
        && frame.get("method").and_then(serde_json::Value::as_str) == Some(notifications::CANCELLED)
        && frame.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0")
}

/// Hand a client notification frame to the router.
///
/// The frame is parsed with
/// [`parse_notification_frame`](crate::transport::stdio::parse_notification_frame),
/// the same envelope parse the listen-stream registry applies, so the two
/// agree on which frames are a `notifications/cancelled`: one the registry
/// skips cannot cancel a request here either.
///
/// The one exception is `notifications/initialized` without the `jsonrpc`
/// marker, which this transport has always honored and still does.
fn forward_notification(router: &McpRouter, frame: &serde_json::Value) {
    if let Some(notification) = crate::transport::stdio::parse_notification_frame(frame) {
        router.handle_notification(notification);
    } else if frame.get("method").and_then(serde_json::Value::as_str)
        == Some(notifications::INITIALIZED)
    {
        router.handle_notification(McpNotification::Initialized);
    } else {
        tracing::debug!("ChannelTransport: ignoring a malformed notification");
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
        self.closed.cancel();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionPhase;

    /// `notifications/initialized` without the `jsonrpc` marker completed the
    /// handshake on this transport before #1710 and still does, although the
    /// shared envelope parse rejects the frame.
    #[tokio::test]
    async fn markerless_initialized_still_completes_the_handshake() {
        let router = McpRouter::new().server_info("channel-unit", "0.0.0");
        let mut service = JsonRpcService::new(router.clone());
        let initialize: JsonRpcRequest = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "t", "version": "0"}
            }
        }))
        .unwrap();
        service.call_single(initialize).await.unwrap();
        assert_eq!(router.session_phase(), SessionPhase::Initializing);

        let markerless = serde_json::json!({"method": "notifications/initialized"});
        assert!(crate::transport::stdio::parse_notification_frame(&markerless).is_none());
        forward_notification(&router, &markerless);
        assert_eq!(router.session_phase(), SessionPhase::Initialized);
    }

    /// The registry skips a `notifications/cancelled` without the `jsonrpc`
    /// marker, so the router forward must skip it too.
    #[test]
    fn markerless_cancellation_is_not_parsed() {
        let markerless = serde_json::json!({
            "method": "notifications/cancelled",
            "params": {"requestId": 7}
        });
        assert!(crate::transport::stdio::parse_notification_frame(&markerless).is_none());
        let marked = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": 7}
        });
        assert!(matches!(
            crate::transport::stdio::parse_notification_frame(&marked),
            Some(McpNotification::Cancelled(_))
        ));
    }
}
