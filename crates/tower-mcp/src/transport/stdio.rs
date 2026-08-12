//! Stdio transport for MCP
//!
//! Reads JSON-RPC messages from stdin and writes responses to stdout.
//! Uses line-delimited JSON format.
//!
//! # Protocol lifecycles
//!
//! Final 2026-07-28 requests carry protocol version and client capabilities
//! inline in each request's `_meta` and can begin with a `server/discover`
//! probe. Requests without that metadata retain the legacy initialize
//! lifecycle, so both eras can coexist on one stdio stream. Use
//! [`StdioTransport::protocol_support`] to set the exact runtime allow-list.
//!
//! # Concurrency
//!
//! Requests are handled on their own tasks, so a slow tool does not block
//! the rest of the connection. This matches the HTTP and WebSocket
//! transports, which matters because servers are usually developed over
//! stdio and deployed over HTTP. Responses are written by the read loop
//! alone, keeping the single-writer requirement of a line-delimited stream,
//! and arrive in completion order rather than request order: JSON-RPC pairs
//! a response to its request by id, not by position.
//!
//! `initialize` and `notifications/initialized` are the exception. They
//! establish the protocol revision that later requests are read against, so
//! they are handled before anything behind them.
//!
//! Use [`StdioTransport::max_concurrent_requests`] to bound how many run at
//! once, or to return to strictly serial handling with `1`.
//!
//! # Malformed input
//!
//! Frames are delimited by newline bytes and decoded one at a time, so a
//! frame the peer sends malformed costs that frame and nothing else. Both
//! JSON that does not parse and bytes that are not valid UTF-8 are answered
//! with a JSON-RPC parse error (`-32700`, null id) and discarded; the loop
//! keeps reading and later requests are served normally.
//!
//! # Bidirectional Support
//!
//! For legacy protocols, [`BidirectionalStdioTransport`] enables
//! server-to-client requests like sampling (LLM requests). It multiplexes the
//! stdio streams to handle both incoming requests and outgoing
//! requests/responses. Final 2026-07-28 handlers do not receive a client
//! requester because that protocol does not permit server-initiated requests.
//!
//! # Server Notifications
//!
//! [`StdioTransport`] and [`BidirectionalStdioTransport`] automatically set up
//! notification channels and forward server notifications (progress, logging,
//! resource/tool/prompt list changes) to stdout as JSON-RPC notifications.
//! Once a final `subscriptions/listen` request is accepted, list and resource
//! change notifications use final subscription filtering and ID tagging for
//! the rest of that stream; they are not also broadcast in the legacy
//! untagged form.

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, Semaphore, mpsc, oneshot};

use crate::context::{
    ChannelClientRequester, ClientRequesterHandle, NotificationReceiver, NotificationSender,
    OutgoingRequest, OutgoingRequestReceiver, ServerNotification, notification_channel,
    outgoing_request_channel,
};
use tower_service::Service;

use crate::error::{Error, Result};
use crate::framing::{FrameReader, InputFrame, clean_input_line, read_frame_blocking};
use crate::jsonrpc::JsonRpcService;
#[cfg(feature = "stateless")]
use crate::protocol::{Implementation, SubscriptionFilter};
use crate::protocol::{
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, JsonRpcResponseMessage,
    McpNotification, RequestId, notifications,
};
use crate::router::{McpRouter, RouterRequest, RouterResponse};
use crate::transport::service::{CatchError, InjectAnnotations};
#[cfg(feature = "stateless")]
use crate::transport::subscriptions::{
    SubscriptionClose, SubscriptionCloseReason, SubscriptionObserver, subscription_acknowledgment,
    subscription_complete_response, subscription_matches, tagged_subscription_notification,
};
use crate::{ProtocolSupport, ProtocolSupportError};

// ============================================================================
// Shared helpers
// ============================================================================

enum StdioControl {
    #[cfg(feature = "stateless")]
    CloseSubscription(RequestId),
    Shutdown,
}

/// Cloneable control handle for an asynchronous stdio server.
///
/// The handle can gracefully finish one final-protocol subscription without
/// closing the shared stdio channel, or gracefully finish every active
/// subscription and stop the transport.
#[derive(Clone)]
pub struct StdioTransportHandle {
    control_tx: mpsc::UnboundedSender<StdioControl>,
    stopping: tokio::sync::watch::Receiver<bool>,
}

impl StdioTransportHandle {
    /// Gracefully finish one active `subscriptions/listen` request.
    ///
    /// The transport writes a `SubscriptionsListenResult` for `request_id`.
    /// Unknown or already-finished IDs are harmless.
    #[cfg(feature = "stateless")]
    pub fn close_subscription(&self, request_id: RequestId) -> Result<()> {
        self.control_tx
            .send(StdioControl::CloseSubscription(request_id))
            .map_err(|_| Error::Transport("stdio transport is not running".to_string()))
    }

    /// Resolves once the transport has stopped reading input, before it
    /// waits for in-flight requests.
    ///
    /// Fires on end of input, a [`shutdown`](Self::shutdown) request, or a
    /// read failure. Observing it is what lets a server begin its own
    /// shutdown while `run` is still draining, which is the only way to
    /// break this cycle (#1252):
    ///
    /// 1. closing stdin is the host's shutdown signal
    /// 2. the server awaits `run()`, planning to stop its workers afterwards
    /// 3. `run()` stops reading and waits for every request task
    /// 4. a request is blocked on work only that shutdown would release
    ///
    /// ```rust,no_run
    /// # use tower_mcp::{McpRouter, StdioTransport};
    /// # async fn example(app: impl Clone + Send + 'static) -> Result<(), tower_mcp::BoxError> {
    /// let mut transport = StdioTransport::new(McpRouter::new());
    /// let handle = transport.handle();
    /// tokio::spawn(async move {
    ///     handle.stopping().await;
    ///     // Release anything holding an in-flight handler open.
    /// });
    /// transport.run().await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Returns immediately if the transport has already stopped or been
    /// dropped.
    pub async fn stopping(&self) {
        let mut stopping = self.stopping.clone();
        while !*stopping.borrow_and_update() {
            if stopping.changed().await.is_err() {
                return;
            }
        }
    }

    /// Gracefully finish all subscriptions and stop the stdio transport.
    pub fn shutdown(&self) -> Result<()> {
        self.control_tx
            .send(StdioControl::Shutdown)
            .map_err(|_| Error::Transport("stdio transport is not running".to_string()))
    }
}

fn stdio_control_channel() -> (
    mpsc::UnboundedSender<StdioControl>,
    mpsc::UnboundedReceiver<StdioControl>,
) {
    mpsc::unbounded_channel()
}

/// Write whatever in-flight requests still produce, then return.
///
/// `timeout` bounds the wait. `None` waits for every in-flight request,
/// which is the historical behaviour and stays the default: a finite call
/// that is still running should be answered rather than dropped.
///
/// A bound exists for the case where a handler cannot finish until the
/// application shuts down, and the application is waiting on `run` to
/// return before shutting down (#1252). Reaching it abandons the remaining
/// responses, which is why it is opt-in.
async fn drain_responses<W>(
    out_rx: &mut mpsc::UnboundedReceiver<String>,
    writer: &mut W,
    timeout: Option<std::time::Duration>,
) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    let drain = async {
        while let Some(frame) = out_rx.recv().await {
            write_line_to_stdout(writer, &frame).await?;
        }
        Ok(())
    };

    match timeout {
        None => drain.await,
        Some(limit) => match tokio::time::timeout(limit, drain).await {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!(
                    timeout_ms = limit.as_millis() as u64,
                    "drain timed out; abandoning in-flight responses"
                );
                Ok(())
            }
        },
    }
}

/// Signals handles that the read loop has ended (#1252).
fn stopping_signal() -> tokio::sync::watch::Sender<bool> {
    tokio::sync::watch::channel(false).0
}

#[cfg(feature = "stateless")]
#[derive(Default)]
pub(crate) struct StdioSubscriptions {
    active: HashMap<RequestId, ActiveSubscription>,
    modern_mode: bool,
    server_info: Option<Implementation>,
    observer: Option<Arc<dyn SubscriptionObserver>>,
}

/// One registered listen stream: the accepted filter and when it started.
#[cfg(feature = "stateless")]
struct ActiveSubscription {
    filter: SubscriptionFilter,
    started: std::time::Instant,
}

#[cfg(feature = "stateless")]
pub(crate) enum StdioSubscriptionInput {
    NotHandled,
    Handled(Vec<String>),
    /// A modern listen request that passed the transport-owned checks.
    /// Dispatch it through the JSON-RPC service (so middleware observes it
    /// and the router validates and negotiates the filter), then hand the
    /// response to [`StdioSubscriptions::complete_listen`].
    Dispatch(Box<JsonRpcRequest>),
}

#[cfg(feature = "stateless")]
impl StdioSubscriptions {
    pub(crate) fn new(server_info: Option<Implementation>) -> Self {
        Self {
            server_info,
            ..Self::default()
        }
    }

    pub(crate) fn with_observer(mut self, observer: Option<Arc<dyn SubscriptionObserver>>) -> Self {
        self.observer = observer;
        self
    }

    fn observe_close(
        &self,
        subscription_id: RequestId,
        started: std::time::Instant,
        reason: SubscriptionCloseReason,
    ) {
        if let Some(observer) = &self.observer {
            observer.on_close(SubscriptionClose {
                subscription_id,
                reason,
                duration: started.elapsed(),
            });
        }
    }

    /// Report every remaining stream as disconnected.
    ///
    /// Called when the transport's read loop ends (EOF or client drop): the
    /// streams die with the connection and no terminal frame can be written.
    pub(crate) fn drain_disconnected(&mut self) {
        for (id, subscription) in std::mem::take(&mut self.active) {
            self.observe_close(
                id,
                subscription.started,
                SubscriptionCloseReason::Disconnected,
            );
        }
    }

    pub(crate) fn handle_input<S>(
        &mut self,
        service: &JsonRpcService<S>,
        parsed: &serde_json::Value,
    ) -> Result<StdioSubscriptionInput> {
        let method = parsed.get("method").and_then(serde_json::Value::as_str);

        if method == Some(notifications::CANCELLED) && parsed.get("id").is_none() {
            let notification: JsonRpcNotification = match serde_json::from_value(parsed.clone()) {
                Ok(notification) => notification,
                Err(_) => return Ok(StdioSubscriptionInput::NotHandled),
            };
            let Ok(McpNotification::Cancelled(params)) =
                McpNotification::from_jsonrpc(&notification)
            else {
                return Ok(StdioSubscriptionInput::NotHandled);
            };
            if let Some(request_id) = params.request_id
                && let Some(subscription) = self.active.remove(&request_id)
            {
                tracing::debug!(?request_id, "Cancelled stdio subscription");
                self.observe_close(
                    request_id,
                    subscription.started,
                    SubscriptionCloseReason::Cancelled,
                );
                return Ok(StdioSubscriptionInput::Handled(Vec::new()));
            }
            return Ok(StdioSubscriptionInput::NotHandled);
        }

        if method != Some("subscriptions/listen") || parsed.get("id").is_none() {
            return Ok(StdioSubscriptionInput::NotHandled);
        }

        let claims_modern = parsed
            .pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion")
            .is_some();
        if !claims_modern {
            return Ok(StdioSubscriptionInput::NotHandled);
        }

        let request: JsonRpcRequest = match serde_json::from_value(parsed.clone()) {
            Ok(request) => request,
            Err(error) => {
                let response = parse_error_response(error.to_string());
                return Ok(StdioSubscriptionInput::Handled(vec![
                    serde_json::to_string(&response)?,
                ]));
            }
        };
        let request_id = request.id.clone();
        if let Err(error) = service.validate_request_protocol(&request) {
            let response = JsonRpcResponse::error(Some(request_id), error);
            return Ok(StdioSubscriptionInput::Handled(vec![
                serde_json::to_string(&response)?,
            ]));
        }

        // Duplicate request ids are stream-registry state only this
        // transport can see, so the check stays here, ahead of the service
        // dispatch. Everything else about the request (params shape, the
        // required filter, the SEP-2663 capability gate, filter negotiation)
        // is the router's job, reached through the service so middleware
        // observes the request (#1182).
        if self.active.contains_key(&request_id) {
            let response = JsonRpcResponse::error(
                Some(request_id),
                crate::error::JsonRpcError::invalid_request(
                    "subscription request id is already active",
                ),
            );
            return Ok(StdioSubscriptionInput::Handled(vec![
                serde_json::to_string(&response)?,
            ]));
        }

        Ok(StdioSubscriptionInput::Dispatch(Box::new(request)))
    }

    /// Complete a listen request the caller dispatched through the service.
    ///
    /// An error response is emitted verbatim: middleware already observed
    /// the rejection and the wire shape must match what the transport-owned
    /// validation produced before #1182. A success response carries the
    /// accepted filter, which registers the stream and becomes the
    /// `notifications/subscriptions/acknowledged` frame.
    pub(crate) fn complete_listen(
        &mut self,
        request_id: RequestId,
        response: &JsonRpcResponse,
    ) -> Result<Vec<String>> {
        // `JsonRpcResponse` is non_exhaustive; anything that is not a
        // success result is passed through verbatim like an error.
        let JsonRpcResponse::Result(result) = response else {
            return Ok(vec![serde_json::to_string(response)?]);
        };
        let result = &result.result;
        let accepted = result
            .get("notifications")
            .cloned()
            .map(serde_json::from_value::<SubscriptionFilter>)
            .and_then(std::result::Result::ok);
        let Some(accepted) = accepted else {
            let response = JsonRpcResponse::error(
                Some(request_id),
                crate::error::JsonRpcError::internal_error(
                    "subscriptions/listen produced an unrecognized service result",
                ),
            );
            return Ok(vec![serde_json::to_string(&response)?]);
        };

        let acknowledgment = subscription_acknowledgment(request_id.clone(), accepted.clone());
        let acknowledgment = serde_json::to_string(&acknowledgment)?;
        self.modern_mode = true;
        self.active.insert(
            request_id,
            ActiveSubscription {
                filter: accepted,
                started: std::time::Instant::now(),
            },
        );
        Ok(vec![acknowledgment])
    }

    /// Route a subscription-scoped notification and suppress its untagged
    /// form whenever at least one final subscription is active.
    pub(crate) fn route_notification(
        &self,
        notification: &ServerNotification,
    ) -> Option<Vec<String>> {
        if !self.modern_mode
            || !matches!(
                notification,
                ServerNotification::ResourceUpdated { .. }
                    | ServerNotification::ResourcesListChanged
                    | ServerNotification::ToolsListChanged
                    | ServerNotification::PromptsListChanged
                    | ServerNotification::FinalTaskStatusChanged(_)
            )
        {
            return None;
        }

        Some(
            self.active
                .iter()
                .filter(|(_, subscription)| {
                    subscription_matches(notification, &subscription.filter)
                })
                .filter_map(|(id, _)| tagged_subscription_notification(notification, id))
                .collect(),
        )
    }

    fn close(&mut self, request_id: &RequestId) -> Result<Option<String>> {
        let Some(subscription) = self.active.remove(request_id) else {
            return Ok(None);
        };
        self.observe_close(
            request_id.clone(),
            subscription.started,
            SubscriptionCloseReason::Drained,
        );
        Ok(Some(serde_json::to_string(
            &subscription_complete_response(request_id.clone(), self.server_info.clone()),
        )?))
    }

    fn close_all(&mut self) -> Result<Vec<String>> {
        let ids: Vec<_> = self.active.keys().cloned().collect();
        ids.into_iter()
            .map(|id| {
                self.close(&id)?
                    .ok_or_else(|| Error::Internal("active subscription disappeared".to_string()))
            })
            .collect()
    }
}

/// The parse-error message for a frame whose bytes are not valid UTF-8.
const UNDECODABLE_FRAME: &str = "invalid UTF-8 in input frame";

/// Build the `-32700` frame that answers an undecodable frame, logging the
/// discard on the way through so every transport reports it identically.
///
/// This is the server's half of the [`crate::framing`] contract: a client
/// reading the same frames has nobody to send this to and skips instead
/// (#1296).
fn undecodable_frame_response() -> Result<String> {
    tracing::warn!("{UNDECODABLE_FRAME}, discarding it");
    serde_json::to_string(&parse_error_response(UNDECODABLE_FRAME))
        .map_err(|e| Error::Transport(format!("Failed to serialize error: {}", e)))
}

/// Build a JSON-RPC parse-error response from a parser/dispatch error message.
///
/// Per JSON-RPC 2.0, a parse error sets `code` to `-32700` and `id` to
/// `null` (the request id cannot be recovered from unparseable input).
/// Returning a single shared constructor keeps every stdio parse-error
/// path consistent and gives the wire-format tests in
/// [`tower_mcp_types::testing`] one stable surface to assert against.
pub(crate) fn parse_error_response(message: impl Into<String>) -> JsonRpcResponse {
    JsonRpcResponse::error(None, crate::error::JsonRpcError::parse_error(message))
}

/// Frames produced by concurrently handled requests, drained by the read
/// loop so that exactly one task ever writes to the output stream.
type OutboundFrames = mpsc::UnboundedSender<String>;

/// Whether a frame has to be handled before anything that follows it.
///
/// `initialize` establishes the protocol revision every later request is
/// interpreted against, and `notifications/initialized` closes that
/// handshake, so neither can race the traffic behind it. Both are also
/// first on the connection, where there is nothing to run concurrently
/// with anyway, so holding the read loop for them costs nothing.
///
/// Everything else is explicitly not a barrier. `notifications/cancelled`
/// in particular has to overtake the request it cancels, which is the
/// whole point of handling requests concurrently (#1231).
fn is_ordering_barrier(line: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct MethodPeek<'a> {
        #[serde(borrow, default)]
        method: Option<std::borrow::Cow<'a, str>>,
    }

    match serde_json::from_str::<MethodPeek>(line) {
        Ok(MethodPeek {
            method: Some(method),
        }) => matches!(method.as_ref(), "initialize" | "notifications/initialized"),
        _ => false,
    }
}

/// Wait for permission to start another concurrent request.
///
/// `None` means the caller set no bound, so there is nothing to wait for.
async fn acquire_request_permit(
    limit: &Option<Arc<Semaphore>>,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    match limit {
        Some(semaphore) => semaphore.clone().acquire_owned().await.ok(),
        None => None,
    }
}

/// Routes a client-to-server notification to whatever owns request state.
///
/// [`GenericStdioTransport`] has no router of its own, so without this it
/// drops every inbound notification, including `notifications/cancelled` for
/// an ordinary in-flight request (#1250).
pub(crate) type IncomingNotificationHandler = Arc<dyn Fn(McpNotification) + Send + Sync + 'static>;

/// Build a handler that routes notifications back into a router.
pub(crate) fn router_notification_handler(router: McpRouter) -> IncomingNotificationHandler {
    Arc::new(move |notification| router.handle_notification(notification))
}

/// Whether a frame expects a response.
///
/// A JSON-RPC notification carries no `id`, and a notification must never
/// queue behind an ordinary request or wait for an execution permit:
/// `notifications/cancelled` has to reach a running handler precisely when
/// every slot is busy, which is the case that would otherwise deadlock
/// (#1251). Batches count as requests, since one may contain them.
fn expects_a_response(line: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct IdPeek {
        #[serde(default)]
        id: Option<serde_json::Value>,
    }

    if line.trim_start().starts_with('[') {
        return true;
    }
    match serde_json::from_str::<IdPeek>(line) {
        Ok(IdPeek { id: Some(id) }) => !id.is_null(),
        // Unparseable input still takes the request path, so the existing
        // parse-error response is produced exactly as before.
        Ok(IdPeek { id: None }) => false,
        Err(_) => true,
    }
}

/// One request's work, queued for the dispatcher.
type QueuedRequest = std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>;

/// Run the queue that stands between reading and executing.
///
/// #1251: waiting for a permit in the read loop meant a saturated limit
/// stopped the transport reading at all, so a `notifications/cancelled`
/// queued behind an ordinary request could never arrive, and the request it
/// would have cancelled never released its permit. Nothing progressed.
///
/// Moving the wait here keeps the reader free for control traffic while
/// still bounding how many handlers run at once. Taking requests in order
/// from a channel, rather than letting each spawned task race for a permit,
/// is what keeps execution order predictable under a limit.
fn spawn_request_dispatcher(
    mut queue: mpsc::UnboundedReceiver<QueuedRequest>,
    limit: Option<Arc<Semaphore>>,
    out_tx: OutboundFrames,
) {
    tokio::spawn(async move {
        while let Some(work) = queue.recv().await {
            let permit = acquire_request_permit(&limit).await;
            spawn_request(permit, &out_tx, work);
        }
    });
}

/// Run one request on its own task, sending any response frame back to the
/// read loop.
///
/// #1231: awaiting the handler inline meant a single slow tool blocked
/// every other call on the connection, including the `notifications/cancelled`
/// a client would use to stop it.
fn spawn_request<F>(
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    out_tx: &OutboundFrames,
    work: F,
) where
    F: std::future::Future<Output = Option<String>> + Send + 'static,
{
    let out_tx = out_tx.clone();
    tokio::spawn(async move {
        // Held for the life of the request so the bound counts work in
        // flight rather than requests accepted.
        let _permit = permit;
        if let Some(frame) = work.await {
            let _ = out_tx.send(frame);
        }
    });
}

/// Build the semaphore backing a `max_concurrent_requests` setting.
fn request_limiter(max_concurrent_requests: Option<usize>) -> Option<Arc<Semaphore>> {
    max_concurrent_requests.map(|limit| Arc::new(Semaphore::new(limit.max(1))))
}

/// Process a single line of JSON-RPC input
///
/// Returns `Ok(Some(response))` for requests, `Ok(None)` for notifications.
async fn process_line(
    service: &mut JsonRpcService<McpRouter>,
    router: &McpRouter,
    line: &str,
) -> Result<Option<JsonRpcResponseMessage>> {
    let parsed: serde_json::Value = serde_json::from_str(line)?;

    // Classify before validating. A frame carrying no id has nowhere to put a
    // response, so answering one is a JSON-RPC violation whatever validation
    // would have said about it. Validating first meant a malformed
    // notification came back as an error the client could not correlate
    // (#1272).
    if !parsed.is_array() && parsed.get("id").is_none() {
        let method = parsed.get("method").and_then(|m| m.as_str());
        if let Err(error) =
            service.inspect_incoming_value(&parsed, crate::inspection::McpDirection::ClientToServer)
        {
            // Still worth surfacing, just not to the client.
            tracing::debug!(method, %error, "rejected an invalid notification");
            return Ok(None);
        }
        match serde_json::from_str::<JsonRpcNotification>(line) {
            Ok(notification) => handle_notification(router, notification)?,
            Err(_) => tracing::debug!(method, "unparseable notification"),
        }
        return Ok(None);
    }

    // A response carries an id, no method, and one of `result` or `error`.
    // All three matter: an id with no method and neither of those is an
    // invalid *request* and must still be refused. `BidirectionalStdioTransport`
    // uses the same test, so the two transports agree on what a response is.
    //
    // This transport never sends requests, so a response arriving is the
    // peer's mistake. Ignoring it beats answering, which previously produced
    // a parse error naming an internal type (#1272).
    if !parsed.is_array()
        && parsed.get("method").is_none()
        && (parsed.get("result").is_some() || parsed.get("error").is_some())
    {
        tracing::debug!("ignoring an unexpected JSON-RPC response frame");
        return Ok(None);
    }

    if let Err(error) =
        service.inspect_incoming_value(&parsed, crate::inspection::McpDirection::ClientToServer)
    {
        return Ok(Some(JsonRpcResponseMessage::Single(
            JsonRpcResponse::error(None, error),
        )));
    }

    // Parse and process as a request (single or batch). The serde error is
    // deliberately not surfaced: for an untagged enum it names the Rust type,
    // which has no business on the wire.
    let Ok(message) = serde_json::from_str::<JsonRpcMessage>(line) else {
        return Ok(Some(JsonRpcResponseMessage::Single(parse_error_response(
            "not a valid JSON-RPC request",
        ))));
    };
    let response = service.call_message(message).await?;
    Ok(Some(response))
}

/// Handle a JSON-RPC notification
fn handle_notification(router: &McpRouter, notification: JsonRpcNotification) -> Result<()> {
    let mcp_notification = McpNotification::from_jsonrpc(&notification)?;
    router.handle_notification(mcp_notification);
    Ok(())
}

/// Serialize a server notification to a JSON-RPC notification string.
/// Dispatch a validated `subscriptions/listen` request through the JSON-RPC
/// service, converting a service-level failure into a JSON-RPC error response
/// so [`StdioSubscriptions::complete_listen`] always receives an answer.
#[cfg(feature = "stateless")]
pub(crate) async fn dispatch_listen_request<S>(
    service: &mut JsonRpcService<S>,
    request: JsonRpcRequest,
    request_id: crate::protocol::RequestId,
) -> JsonRpcResponse
where
    S: tower_service::Service<
            crate::router::RouterRequest,
            Response = crate::router::RouterResponse,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    match service.call_single(request).await {
        Ok(response) => response,
        Err(error) => JsonRpcResponse::error(
            Some(request_id),
            crate::error::JsonRpcError::internal_error(error.to_string()),
        ),
    }
}

pub(crate) fn serialize_notification(notification: &ServerNotification) -> Option<String> {
    match notification {
        ServerNotification::Progress(params) => {
            let notif = JsonRpcNotification::new(notifications::PROGRESS)
                .with_params(serde_json::to_value(params).unwrap_or_default());
            serde_json::to_string(&notif).ok()
        }
        ServerNotification::LogMessage(params) => {
            let notif = JsonRpcNotification::new(notifications::MESSAGE)
                .with_params(serde_json::to_value(params).unwrap_or_default());
            serde_json::to_string(&notif).ok()
        }
        ServerNotification::ResourceUpdated { uri } => {
            let notif = JsonRpcNotification::new(notifications::RESOURCE_UPDATED)
                .with_params(serde_json::json!({ "uri": uri }));
            serde_json::to_string(&notif).ok()
        }
        ServerNotification::ResourcesListChanged => {
            let notif = JsonRpcNotification::new(notifications::RESOURCES_LIST_CHANGED);
            serde_json::to_string(&notif).ok()
        }
        ServerNotification::ToolsListChanged => {
            let notif = JsonRpcNotification::new(notifications::TOOLS_LIST_CHANGED);
            serde_json::to_string(&notif).ok()
        }
        ServerNotification::PromptsListChanged => {
            let notif = JsonRpcNotification::new(notifications::PROMPTS_LIST_CHANGED);
            serde_json::to_string(&notif).ok()
        }
        ServerNotification::TaskStatusChanged(params) => {
            let notif = JsonRpcNotification::new(notifications::TASK_STATUS_CHANGED)
                .with_params(serde_json::to_value(params).unwrap_or_default());
            serde_json::to_string(&notif).ok()
        }
        ServerNotification::FinalTaskStatusChanged(params) => {
            let notif = JsonRpcNotification::new(notifications::TASK_STATUS_CHANGED)
                .with_params(serde_json::to_value(params).ok()?);
            serde_json::to_string(&notif).ok()
        }
    }
}

/// Write a line to an async writer and flush.
async fn write_line_to_stdout<W>(stdout: &mut W, line: &str) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    stdout
        .write_all(line.as_bytes())
        .await
        .map_err(|e| Error::Transport(format!("Failed to write to stdout: {}", e)))?;
    stdout
        .write_all(b"\n")
        .await
        .map_err(|e| Error::Transport(format!("Failed to write newline: {}", e)))?;
    stdout
        .flush()
        .await
        .map_err(|e| Error::Transport(format!("Failed to flush stdout: {}", e)))?;
    Ok(())
}

// ============================================================================
// Async stdio transport
// ============================================================================

/// Stdio transport for MCP servers
///
/// Reads JSON-RPC messages from stdin and writes responses to stdout.
/// Supports single requests for every implemented revision and request
/// batches only for an exact negotiated `2025-03-26` connection. Later MCP
/// revisions reject top-level JSON-RPC arrays.
///
/// Server notifications (progress, logging, resource/tool/prompt list changes)
/// are automatically forwarded to stdout as JSON-RPC notifications.
///
/// # Example
///
/// ```rust,no_run
/// use tower_mcp::{BoxError, McpRouter, StdioTransport};
///
/// #[tokio::main]
/// async fn main() -> Result<(), BoxError> {
///     let router = McpRouter::new()
///         .server_info("my-server", "1.0.0");
///
///     let mut transport = StdioTransport::new(router);
///     transport.run().await?;
///     Ok(())
/// }
/// ```
pub struct StdioTransport {
    service: JsonRpcService<McpRouter>,
    router: McpRouter,
    notification_rx: NotificationReceiver,
    control_tx: mpsc::UnboundedSender<StdioControl>,
    control_rx: mpsc::UnboundedReceiver<StdioControl>,
    /// Fires when the read loop ends, before the drain (#1252).
    stopping: tokio::sync::watch::Sender<bool>,
    /// How long the drain waits for in-flight requests once input has ended.
    /// `None` waits for all of them, which is the historical behaviour.
    drain_timeout: Option<std::time::Duration>,
    /// Held only by [`StdioTransport::without_server_notifications`], to keep
    /// the unused outbound channel open rather than closed (#1250).
    ///
    /// Never read: the point is the sender's lifetime, not its value. A
    /// dropped sender would make the receiver return `None` on every poll,
    /// so the read loop would re-check a dead branch each iteration.
    #[allow(dead_code)]
    outbound_disabled: Option<NotificationSender>,
    /// Bound on requests in flight at once; `None` leaves it unbounded (#1231).
    max_concurrent_requests: Option<usize>,
}

impl StdioTransport {
    /// Create a new stdio transport wrapping an MCP router
    pub fn new(router: McpRouter) -> Self {
        let (notif_tx, notification_rx) = notification_channel(256);
        let (control_tx, control_rx) = stdio_control_channel();
        let router = router.with_notification_sender(notif_tx);
        let service = JsonRpcService::new(router.clone());
        Self {
            service,
            router,
            notification_rx,
            stopping: stopping_signal(),
            drain_timeout: None,
            outbound_disabled: None,
            control_tx,
            control_rx,
            max_concurrent_requests: None,
        }
    }

    /// Return a cloneable handle for graceful subscription closure or server
    /// shutdown while [`Self::run`] is active.
    pub fn handle(&self) -> StdioTransportHandle {
        StdioTransportHandle {
            control_tx: self.control_tx.clone(),
            stopping: self.stopping.subscribe(),
        }
    }

    /// Create a transport that receives client notifications but sends none.
    ///
    /// [`new`](Self::new) installs an outbound notification sender on the
    /// router, which is also what makes capability synthesis advertise MCP
    /// logging and set `listChanged`. A server that logs to stderr or OTLP
    /// and deliberately does not expose MCP logging had no way to keep
    /// inbound `notifications/cancelled` without also advertising features it
    /// does not offer (#1250).
    ///
    /// This routes inbound notifications exactly as [`new`](Self::new) does.
    /// It only declines to send: no progress, logging, or `listChanged`
    /// frames are emitted, and the router advertises neither.
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, StdioTransport};
    ///
    /// let router = McpRouter::new().server_info("my-server", "1.0.0");
    /// let transport = StdioTransport::without_server_notifications(router);
    /// ```
    pub fn without_server_notifications(router: McpRouter) -> Self {
        let (notification_tx, notification_rx) = notification_channel(256);
        let (control_tx, control_rx) = stdio_control_channel();
        // The router never learns about the sender, so it advertises no
        // notification-derived capabilities. Holding the sender here keeps
        // the receiver pending forever rather than immediately closed, which
        // would make the read loop poll a dead branch every iteration.
        let service = JsonRpcService::new(router.clone());
        Self {
            service,
            router,
            notification_rx,
            stopping: stopping_signal(),
            drain_timeout: None,
            outbound_disabled: Some(notification_tx),
            control_tx,
            control_rx,
            max_concurrent_requests: None,
        }
    }

    /// Bound how long the transport waits for in-flight requests after input
    /// has ended.
    ///
    /// By default it waits for all of them, so a call that is still running
    /// when stdin closes is still answered. That is the right default and it
    /// stays the default.
    ///
    /// Set a bound when a handler might not finish on its own. Reaching it
    /// abandons the remaining responses and returns, which is preferable to
    /// never returning. Pair it with
    /// [`StdioTransportHandle::stopping`] so the application can release
    /// those handlers rather than relying on the deadline (#1252).
    pub fn drain_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.drain_timeout = Some(timeout);
        self
    }

    /// Cap how many requests this transport handles at once.
    ///
    /// Requests run concurrently by default, matching the HTTP and WebSocket
    /// transports, so a slow tool does not block the rest of the connection.
    /// That is unbounded: a client can start as many requests as it can
    /// write. Set a cap when handlers are expensive enough that the number
    /// running at once matters.
    ///
    /// The cap bounds how many handlers run at once, not how fast input is
    /// read. Requests past the cap wait their turn in order. The transport
    /// keeps reading either way, because control traffic such as
    /// `notifications/cancelled` has to reach a running handler even when
    /// every execution slot is busy (#1251).
    ///
    /// `1` restores the strictly serial handling this transport used before
    /// 0.21, which is the escape hatch for handlers that assume no two
    /// requests overlap.
    ///
    /// ```rust
    /// use tower_mcp::{McpRouter, StdioTransport};
    ///
    /// let router = McpRouter::new().server_info("my-server", "1.0.0");
    /// let transport = StdioTransport::new(router).max_concurrent_requests(16);
    /// ```
    pub fn max_concurrent_requests(mut self, limit: usize) -> Self {
        self.max_concurrent_requests = Some(limit);
        self
    }

    /// Set the exact protocol versions this transport accepts and advertises.
    ///
    /// By default every implementation compiled into `tower-mcp` is enabled.
    /// Final 2026-07-28 requests carry their version and capabilities in each
    /// request's `_meta`; legacy initialize traffic may coexist on the same
    /// stdio stream.
    pub fn protocol_support(mut self, support: ProtocolSupport) -> Self {
        self.service = self.service.protocol_support(support);
        self
    }

    /// Construct and set an exact runtime protocol-version allow-list.
    pub fn protocol_versions<I, V>(
        mut self,
        versions: I,
    ) -> std::result::Result<Self, ProtocolSupportError>
    where
        I: IntoIterator<Item = V>,
        V: Into<String>,
    {
        self.service = self.service.protocol_versions(versions)?;
        Ok(self)
    }

    /// Apply a tower middleware layer to this transport.
    ///
    /// This converts the `StdioTransport` into a [`GenericStdioTransport`] with
    /// the middleware applied, while preserving notification forwarding.
    ///
    /// Use [`tower::ServiceBuilder`] to compose multiple layers:
    ///
    /// ```rust,no_run
    /// use std::time::Duration;
    /// use tower::ServiceBuilder;
    /// use tower::timeout::TimeoutLayer;
    /// use tower_mcp::{BoxError, McpRouter, StdioTransport};
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), BoxError> {
    ///     let router = McpRouter::new().server_info("my-server", "1.0.0");
    ///
    ///     let mut transport = StdioTransport::new(router)
    ///         .layer(
    ///             ServiceBuilder::new()
    ///                 .layer(TimeoutLayer::new(Duration::from_secs(5)))
    ///                 .concurrency_limit(10)
    ///                 .into_inner(),
    ///         );
    ///
    ///     transport.run().await?;
    ///     Ok(())
    /// }
    /// ```
    pub fn layer<L>(
        self,
        layer: L,
    ) -> GenericStdioTransport<InjectAnnotations<CatchError<L::Service>>>
    where
        L: tower::Layer<McpRouter>,
        L::Service: Service<RouterRequest, Response = RouterResponse> + Clone + Send + 'static,
        <L::Service as Service<RouterRequest>>::Error: std::fmt::Display + Send,
        <L::Service as Service<RouterRequest>>::Future: Send,
    {
        let protocol_support = self.service.configured_protocol_support().clone();
        let annotations = self.router.tool_annotations_map();
        // Taken before the layer consumes the router. A clone shares the
        // in-flight request registry, so cancellation routed through it
        // reaches requests dispatched by the layered service (#1250).
        let incoming_notifications = Some(router_notification_handler(self.router.clone()));
        #[cfg(feature = "stateless")]
        let subscription_observer = self.router.subscription_observer();
        let wrapped = layer.layer(self.router);
        let service = InjectAnnotations::new(CatchError::new(wrapped), annotations);
        GenericStdioTransport {
            service: JsonRpcService::new(service).protocol_support(protocol_support),
            notification_rx: Some(self.notification_rx),
            control_tx: self.control_tx,
            control_rx: self.control_rx,
            incoming_notifications,
            stopping: self.stopping,
            drain_timeout: self.drain_timeout,
            // Carried across the conversion so `.layer()` does not silently
            // discard a concurrency bound set before it.
            max_concurrent_requests: self.max_concurrent_requests,
            #[cfg(feature = "stateless")]
            subscription_observer,
        }
    }

    /// Run the transport, processing messages until EOF or error
    ///
    /// This is a thin wrapper around [`Self::run_with_streams`] that wires up
    /// `tokio::io::stdin()` and `tokio::io::stdout()`. Most users want this
    /// method; use [`Self::run_with_streams`] only for in-process testing.
    pub async fn run(&mut self) -> Result<()> {
        self.run_with_streams(tokio::io::stdin(), tokio::io::stdout())
            .await
    }

    /// Run the transport, reading from `reader` and writing to `writer`.
    ///
    /// This is the streams-generic counterpart of [`Self::run`]. The default
    /// `run()` calls this with `tokio::io::stdin()` / `tokio::io::stdout()`.
    ///
    /// Exposing this lets tests drive the full read-eval-write loop with
    /// `tokio::io::duplex()` and assert end-to-end behavior (parse-error
    /// frames, loop continuation across bad input, EOF handling).
    pub async fn run_with_streams<R, W>(&mut self, reader: R, mut writer: W) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin + Send,
        W: tokio::io::AsyncWrite + Unpin + Send,
    {
        // Keep the frame buffer across cancelled `select!` branches. A bare
        // `read_until` future can discard a partial JSON frame when a
        // notification or control message wins the race; `FrameReader` holds
        // that partial frame until the following poll.
        let mut frames = FrameReader::new(reader);
        #[cfg(feature = "stateless")]
        let mut subscriptions = StdioSubscriptions {
            server_info: Some(self.router.implementation()),
            ..StdioSubscriptions::default()
        }
        .with_observer(self.router.subscription_observer());

        // Requests run on their own tasks and hand their responses back
        // here, so this loop stays the only writer to the output stream
        // (#1231).
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let limit = request_limiter(self.max_concurrent_requests);
        // The reader hands work to the dispatcher and moves on, so a
        // saturated limit never stops it reading control traffic (#1251).
        let (work_tx, work_rx) = mpsc::unbounded_channel::<QueuedRequest>();
        spawn_request_dispatcher(work_rx, limit, out_tx.clone());

        tracing::info!("Stdio transport started, waiting for input");

        loop {
            tokio::select! {
                // Handle incoming messages from stdin
                result = frames.next_frame() => {
                    let Some(frame) = result? else {
                        // EOF
                        tracing::info!("Stdin closed, shutting down");
                        break;
                    };
                    let InputFrame::Line(line) = frame else {
                        write_line_to_stdout(&mut writer, &undecodable_frame_response()?).await?;
                        continue;
                    };

                    let trimmed = clean_input_line(&line);
                    if trimmed.is_empty() {
                        continue;
                    }

                    tracing::debug!(input = %trimmed, "Received message");

                    #[cfg(feature = "stateless")]
                    {
                        let parsed: serde_json::Value = match serde_json::from_str(trimmed) {
                            Ok(parsed) => parsed,
                            Err(_) => serde_json::Value::Null,
                        };
                        match subscriptions.handle_input(&self.service, &parsed)? {
                            StdioSubscriptionInput::Handled(frames) => {
                                for frame in frames {
                                    write_line_to_stdout(&mut writer, &frame).await?;
                                }
                                continue;
                            }
                            StdioSubscriptionInput::Dispatch(request) => {
                                let request_id = request.id.clone();
                                let response = dispatch_listen_request(
                                    &mut self.service,
                                    *request,
                                    request_id.clone(),
                                )
                                .await;
                                for frame in
                                    subscriptions.complete_listen(request_id, &response)?
                                {
                                    write_line_to_stdout(&mut writer, &frame).await?;
                                }
                                continue;
                            }
                            StdioSubscriptionInput::NotHandled => {}
                        }
                    }

                    let barrier = is_ordering_barrier(trimmed);
                    let mut service = self.service.clone();
                    let router = self.router.clone();
                    let owned = trimmed.to_string();
                    // The service clone shares the negotiated protocol
                    // revision through an `Arc`, so concurrent requests all
                    // see the same handshake state.
                    let work = async move {
                        match process_line(&mut service, &router, &owned).await {
                            Ok(Some(response)) => match serde_json::to_string(&response) {
                                Ok(json) => {
                                    tracing::debug!(output = %json, "Sending response");
                                    Some(json)
                                }
                                Err(e) => {
                                    tracing::error!(error = %e, "Failed to serialize response");
                                    None
                                }
                            },
                            Ok(None) => None, // Notification, no response needed
                            Err(e) => {
                                tracing::error!(error = %e, "Error processing message");
                                serde_json::to_string(&parse_error_response(e.to_string())).ok()
                            }
                        }
                    };

                    if barrier {
                        if let Some(frame) = work.await {
                            write_line_to_stdout(&mut writer, &frame).await?;
                        }
                    } else if expects_a_response(trimmed) {
                        let _ = work_tx.send(Box::pin(work));
                    } else {
                        // No permit and no queue: a notification must be able
                        // to overtake the requests it affects (#1251).
                        spawn_request(None, &out_tx, work);
                    }
                }

                // Responses from concurrently handled requests. Writing
                // them here keeps stdout framing correct with one writer.
                Some(frame) = out_rx.recv() => {
                    write_line_to_stdout(&mut writer, &frame).await?;
                }

                // Forward server notifications to stdout
                Some(notification) = self.notification_rx.recv() => {
                    #[cfg(feature = "stateless")]
                    if let Some(frames) = subscriptions.route_notification(&notification) {
                        for json in frames {
                            tracing::debug!(output = %json, "Sending subscription notification");
                            write_line_to_stdout(&mut writer, &json).await?;
                        }
                        continue;
                    }
                    if let Some(json) = serialize_notification(&notification) {
                        tracing::debug!(output = %json, "Sending notification");
                        write_line_to_stdout(&mut writer, &json).await?;
                    }
                }

                Some(control) = self.control_rx.recv() => {
                    match control {
                        #[cfg(feature = "stateless")]
                        StdioControl::CloseSubscription(request_id) => {
                            if let Some(json) = subscriptions.close(&request_id)? {
                                write_line_to_stdout(&mut writer, &json).await?;
                            }
                        }
                        StdioControl::Shutdown => {
                            #[cfg(feature = "stateless")]
                            for json in subscriptions.close_all()? {
                                write_line_to_stdout(&mut writer, &json).await?;
                            }
                            break;
                        }
                    }
                }
            }
        }

        // Input has stopped. Signalling before the drain is the point: a
        // server that only releases its handlers during its own shutdown
        // cannot start that shutdown if it is waiting on `run` (#1252).
        let _ = self.stopping.send(true);

        // Closing the queue lets the dispatcher finish what it has and drop
        // its own sender; dropping this loop's leaves only the in-flight
        // requests holding one, so the drain ends when the last finishes.
        // Without this their responses would be lost on shutdown.
        drop(work_tx);
        drop(out_tx);
        drain_responses(&mut out_rx, &mut writer, self.drain_timeout).await?;

        // The read loop is over: any streams still registered die with
        // the connection and cannot receive a terminal frame.
        #[cfg(feature = "stateless")]
        subscriptions.drain_disconnected();
        Ok(())
    }
}

// ============================================================================
// Generic stdio transport for middleware-wrapped services
// ============================================================================

/// Generic stdio transport that works with any tower service.
///
/// This transport accepts a middleware-wrapped service instead of an `McpRouter`
/// directly. Use this when you want to apply tower middleware layers like
/// rate limiting or bulkhead patterns.
///
/// # Server Notifications
///
/// Use [`GenericStdioTransport::with_notifications`] to enable server notification
/// forwarding. Without it, notifications from the router will not reach the client.
///
/// # Example
///
/// ```rust,no_run
/// use std::time::Duration;
/// use tower::ServiceBuilder;
/// use tower::timeout::TimeoutLayer;
/// use tower_mcp::{BoxError, CatchError, McpRouter, GenericStdioTransport};
/// use tower_mcp::context::notification_channel;
///
/// #[tokio::main]
/// async fn main() -> Result<(), BoxError> {
///     // Set up notification channel before wrapping in middleware
///     let (notif_tx, notif_rx) = notification_channel(256);
///     let router = McpRouter::new()
///         .server_info("my-server", "1.0.0")
///         .with_notification_sender(notif_tx);
///
///     let service = CatchError::new(
///         ServiceBuilder::new()
///             .layer(TimeoutLayer::new(Duration::from_secs(5)))
///             .concurrency_limit(10)
///             .service(router),
///     );
///
///     let mut transport = GenericStdioTransport::with_notifications(service, notif_rx);
///     transport.run().await?;
///     Ok(())
/// }
/// ```
pub struct GenericStdioTransport<S>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = std::convert::Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    service: JsonRpcService<S>,
    notification_rx: Option<NotificationReceiver>,
    control_tx: mpsc::UnboundedSender<StdioControl>,
    control_rx: mpsc::UnboundedReceiver<StdioControl>,
    /// Fires when the read loop ends, before the drain (#1252).
    stopping: tokio::sync::watch::Sender<bool>,
    /// How long the drain waits for in-flight requests once input has ended.
    drain_timeout: Option<std::time::Duration>,
    /// Where inbound client notifications go. `None` drops them, which is
    /// what a directly constructed generic transport did unconditionally
    /// before (#1250).
    incoming_notifications: Option<IncomingNotificationHandler>,
    /// Bound on requests in flight at once; `None` leaves it unbounded (#1231).
    max_concurrent_requests: Option<usize>,
    /// Close observer threaded from the router by [`StdioTransport::layer`];
    /// `None` for directly constructed generic transports, which have no
    /// router to read it from.
    #[cfg(feature = "stateless")]
    subscription_observer: Option<Arc<dyn SubscriptionObserver>>,
}

impl<S> GenericStdioTransport<S>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = std::convert::Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    /// Create a new generic stdio transport wrapping any compatible service.
    ///
    /// The service must implement `Service<RouterRequest, Response = RouterResponse>`.
    /// This is typically an `McpRouter` wrapped in tower middleware layers.
    ///
    /// **Note:** This constructor does not set up notification forwarding. Server
    /// notifications (progress, logging, list changes) will not reach the client.
    /// Use [`GenericStdioTransport::with_notifications`] instead to enable them.
    pub fn new(service: S) -> Self {
        let (control_tx, control_rx) = stdio_control_channel();
        Self {
            service: JsonRpcService::new(service),
            notification_rx: None,
            control_tx,
            control_rx,
            stopping: stopping_signal(),
            drain_timeout: None,
            incoming_notifications: None,
            max_concurrent_requests: None,
            #[cfg(feature = "stateless")]
            subscription_observer: None,
        }
    }

    /// Route inbound client notifications into `router`.
    ///
    /// A generic transport has no router of its own, so by default it drops
    /// every client-to-server notification, including
    /// `notifications/cancelled` for an in-flight request. Pass the router
    /// backing this service to restore that (#1250).
    ///
    /// [`StdioTransport::layer`] does this automatically, so this is for
    /// transports built directly from a service.
    pub fn route_notifications_to(mut self, router: McpRouter) -> Self {
        self.incoming_notifications = Some(router_notification_handler(router));
        self
    }

    /// Bound how long the transport waits for in-flight requests after input
    /// has ended.
    ///
    /// See [`StdioTransport::drain_timeout`].
    pub fn drain_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.drain_timeout = Some(timeout);
        self
    }

    /// Cap how many requests this transport handles at once.
    ///
    /// See [`StdioTransport::max_concurrent_requests`]. Requests run
    /// concurrently by default; `1` restores strictly serial handling.
    pub fn max_concurrent_requests(mut self, limit: usize) -> Self {
        self.max_concurrent_requests = Some(limit);
        self
    }

    /// Create a new generic stdio transport with notification forwarding.
    ///
    /// Pass a `NotificationReceiver` from [`notification_channel()`] to enable
    /// server notifications. Make sure to also call
    /// `router.with_notification_sender(tx)` before wrapping the router in middleware.
    ///
    /// [`notification_channel()`]: crate::context::notification_channel
    pub fn with_notifications(service: S, notification_rx: NotificationReceiver) -> Self {
        let (control_tx, control_rx) = stdio_control_channel();
        Self {
            service: JsonRpcService::new(service),
            notification_rx: Some(notification_rx),
            control_tx,
            control_rx,
            stopping: stopping_signal(),
            drain_timeout: None,
            incoming_notifications: None,
            max_concurrent_requests: None,
            #[cfg(feature = "stateless")]
            subscription_observer: None,
        }
    }

    /// Return a cloneable handle for graceful subscription closure or server
    /// shutdown while [`Self::run`] is active.
    pub fn handle(&self) -> StdioTransportHandle {
        StdioTransportHandle {
            control_tx: self.control_tx.clone(),
            stopping: self.stopping.subscribe(),
        }
    }

    /// Set the exact protocol versions this transport accepts and advertises.
    pub fn protocol_support(mut self, support: ProtocolSupport) -> Self {
        self.service = self.service.protocol_support(support);
        self
    }

    /// Construct and set an exact runtime protocol-version allow-list.
    pub fn protocol_versions<I, V>(
        mut self,
        versions: I,
    ) -> std::result::Result<Self, ProtocolSupportError>
    where
        I: IntoIterator<Item = V>,
        V: Into<String>,
    {
        self.service = self.service.protocol_versions(versions)?;
        Ok(self)
    }

    /// Run the transport, processing messages until EOF or error.
    ///
    /// Thin wrapper around [`Self::run_with_streams`] that wires up
    /// `tokio::io::stdin()` / `tokio::io::stdout()`.
    pub async fn run(&mut self) -> Result<()> {
        self.run_with_streams(tokio::io::stdin(), tokio::io::stdout())
            .await
    }

    /// Run the transport, reading from `reader` and writing to `writer`.
    ///
    /// Streams-generic counterpart of [`Self::run`]. Lets tests drive the
    /// read-eval-write loop with in-memory streams (e.g. `tokio::io::duplex()`).
    pub async fn run_with_streams<R, W>(&mut self, reader: R, mut writer: W) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin + Send,
        W: tokio::io::AsyncWrite + Unpin + Send,
    {
        let mut frames = FrameReader::new(reader);
        #[cfg(feature = "stateless")]
        let mut subscriptions =
            StdioSubscriptions::default().with_observer(self.subscription_observer.clone());

        // Requests run on their own tasks and hand their responses back
        // here, so this loop stays the only writer to the output stream
        // (#1231).
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let limit = request_limiter(self.max_concurrent_requests);
        // Same reasoning as the plain transport: reading must not stall on a
        // saturated limit (#1251).
        let (work_tx, work_rx) = mpsc::unbounded_channel::<QueuedRequest>();
        spawn_request_dispatcher(work_rx, limit, out_tx.clone());

        tracing::info!("Generic stdio transport started, waiting for input");

        loop {
            // Use select! if we have a notification receiver, otherwise just read
            if let Some(ref mut notif_rx) = self.notification_rx {
                tokio::select! {
                    result = frames.next_frame() => {
                        let Some(frame) = result? else {
                            tracing::info!("Stdin closed, shutting down");
                            break;
                        };
                        // Answered through `out_tx` like every other frame
                        // this loop produces, so the read loop stays the
                        // only writer.
                        let InputFrame::Line(line) = frame else {
                            let _ = out_tx.send(undecodable_frame_response()?);
                            continue;
                        };

                        Self::process_input(
                            &mut self.service,
                            &line,
                            &out_tx,
                            &work_tx,
                            self.incoming_notifications.as_ref(),
                            true,
                            #[cfg(feature = "stateless")]
                            &mut subscriptions,
                        ).await?;
                    }

                    Some(frame) = out_rx.recv() => {
                        write_line_to_stdout(&mut writer, &frame).await?;
                    }

                    Some(notification) = notif_rx.recv() => {
                        #[cfg(feature = "stateless")]
                        if let Some(frames) = subscriptions.route_notification(&notification) {
                            for json in frames {
                                tracing::debug!(output = %json, "Sending subscription notification");
                                write_line_to_stdout(&mut writer, &json).await?;
                            }
                            continue;
                        }
                        if let Some(json) = serialize_notification(&notification) {
                            tracing::debug!(output = %json, "Sending notification");
                            write_line_to_stdout(&mut writer, &json).await?;
                        }
                    }

                    Some(control) = self.control_rx.recv() => {
                        if Self::handle_control(
                            control,
                            &mut writer,
                            #[cfg(feature = "stateless")]
                            &mut subscriptions,
                        ).await? {
                            break;
                        }
                    }
                }
            } else {
                tokio::select! {
                    result = frames.next_frame() => {
                        let Some(frame) = result? else {
                            tracing::info!("Stdin closed, shutting down");
                            break;
                        };
                        let InputFrame::Line(line) = frame else {
                            let _ = out_tx.send(undecodable_frame_response()?);
                            continue;
                        };
                        Self::process_input(
                            &mut self.service,
                            &line,
                            &out_tx,
                            &work_tx,
                            self.incoming_notifications.as_ref(),
                            false,
                            #[cfg(feature = "stateless")]
                            &mut subscriptions,
                        ).await?;
                    }
                    Some(frame) = out_rx.recv() => {
                        write_line_to_stdout(&mut writer, &frame).await?;
                    }

                    Some(control) = self.control_rx.recv() => {
                        if Self::handle_control(
                            control,
                            &mut writer,
                            #[cfg(feature = "stateless")]
                            &mut subscriptions,
                        ).await? {
                            break;
                        }
                    }
                }
            }
        }

        // Input has stopped. Signalling before the drain is the point: a
        // server that only releases its handlers during its own shutdown
        // cannot start that shutdown if it is waiting on `run` (#1252).
        let _ = self.stopping.send(true);

        // Closing the queue lets the dispatcher finish what it has and drop
        // its own sender; dropping this loop's leaves only the in-flight
        // requests holding one, so the drain ends when the last finishes.
        // Without this their responses would be lost on shutdown.
        drop(work_tx);
        drop(out_tx);
        drain_responses(&mut out_rx, &mut writer, self.drain_timeout).await?;

        // The read loop is over: any streams still registered die with
        // the connection and cannot receive a terminal frame.
        #[cfg(feature = "stateless")]
        subscriptions.drain_disconnected();
        Ok(())
    }

    /// Handle one input line.
    ///
    /// Frames go out through `out_tx` rather than straight to the writer so
    /// that requests can be answered on their own tasks while the read loop
    /// stays the only writer (#1231).
    async fn process_input(
        service: &mut JsonRpcService<S>,
        line: &str,
        out_tx: &OutboundFrames,
        work_tx: &mpsc::UnboundedSender<QueuedRequest>,
        incoming_notifications: Option<&IncomingNotificationHandler>,
        subscriptions_enabled: bool,
        #[cfg(feature = "stateless")] subscriptions: &mut StdioSubscriptions,
    ) -> Result<()> {
        let trimmed = clean_input_line(line);
        if trimmed.is_empty() {
            return Ok(());
        }

        tracing::debug!(input = %trimmed, "Received message");

        // Check if it's a notification (no id field)
        let parsed: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let _ = out_tx.send(Self::error_frame(None, &e.to_string())?);
                return Ok(());
            }
        };

        #[cfg(feature = "stateless")]
        if subscriptions_enabled {
            match subscriptions.handle_input(service, &parsed)? {
                StdioSubscriptionInput::Handled(frames) => {
                    for frame in frames {
                        let _ = out_tx.send(frame);
                    }
                    return Ok(());
                }
                StdioSubscriptionInput::Dispatch(request) => {
                    let request_id = request.id.clone();
                    let response =
                        dispatch_listen_request(service, *request, request_id.clone()).await;
                    for frame in subscriptions.complete_listen(request_id, &response)? {
                        let _ = out_tx.send(frame);
                    }
                    return Ok(());
                }
                StdioSubscriptionInput::NotHandled => {}
            }
        }
        #[cfg(not(feature = "stateless"))]
        let _ = subscriptions_enabled;

        if let Err(error) =
            service.inspect_incoming_value(&parsed, crate::inspection::McpDirection::ClientToServer)
        {
            let response = JsonRpcResponse::error(None, error);
            let _ = out_tx.send(serde_json::to_string(&response)?);
            return Ok(());
        }

        if !parsed.is_array() && parsed.get("id").is_none() {
            // Routed when the transport was given somewhere to route to,
            // which is what keeps `notifications/cancelled` working after a
            // layer is applied (#1250). Without a handler there is still
            // nowhere for it to go.
            match incoming_notifications {
                Some(handler) => match serde_json::from_str::<JsonRpcNotification>(trimmed)
                    .ok()
                    .and_then(|n| McpNotification::from_jsonrpc(&n).ok())
                {
                    Some(notification) => handler(notification),
                    None => tracing::debug!(
                        method = parsed.get("method").and_then(|m| m.as_str()),
                        "Unrecognized notification"
                    ),
                },
                None => tracing::debug!(
                    method = parsed.get("method").and_then(|m| m.as_str()),
                    "Received notification but no handler is configured"
                ),
            }
            return Ok(());
        }

        // Parse and process as a request (single or batch)
        let message: JsonRpcMessage = match serde_json::from_str(trimmed) {
            Ok(m) => m,
            Err(e) => {
                let _ = out_tx.send(Self::error_frame(None, &e.to_string())?);
                return Ok(());
            }
        };

        // The service clone shares the negotiated protocol revision through
        // an `Arc`, so concurrent requests all see the same handshake state.
        let mut service = service.clone();
        let work = async move {
            match service.call_message(message).await {
                Ok(response) => match serde_json::to_string(&response) {
                    Ok(json) => {
                        tracing::debug!(output = %json, "Sending response");
                        Some(json)
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to serialize response");
                        None
                    }
                },
                Err(e) => {
                    tracing::error!(error = %e, "Error processing message");
                    Self::error_frame(None, &e.to_string()).ok()
                }
            }
        };

        if is_ordering_barrier(trimmed) {
            if let Some(frame) = work.await {
                let _ = out_tx.send(frame);
            }
        } else if expects_a_response(trimmed) {
            let _ = work_tx.send(Box::pin(work));
        } else {
            spawn_request(None, out_tx, work);
        }
        Ok(())
    }

    async fn handle_control<W>(
        control: StdioControl,
        writer: &mut W,
        #[cfg(feature = "stateless")] subscriptions: &mut StdioSubscriptions,
    ) -> Result<bool>
    where
        W: tokio::io::AsyncWrite + Unpin + Send,
    {
        #[cfg(not(feature = "stateless"))]
        let _ = &mut *writer;
        match control {
            #[cfg(feature = "stateless")]
            StdioControl::CloseSubscription(request_id) => {
                if let Some(json) = subscriptions.close(&request_id)? {
                    write_line_to_stdout(writer, &json).await?;
                }
                Ok(false)
            }
            StdioControl::Shutdown => {
                #[cfg(feature = "stateless")]
                for json in subscriptions.close_all()? {
                    write_line_to_stdout(writer, &json).await?;
                }
                Ok(true)
            }
        }
    }

    /// Serialize a parse-error response for the read loop to write.
    fn error_frame(id: Option<crate::protocol::RequestId>, message: &str) -> Result<String> {
        // `id` is currently always `None` from every call site (parse-error
        // path), so use the shared helper; preserve the parameter for callers
        // that may want to surface a known-id error in future.
        let error_response = if let Some(id) = id {
            JsonRpcResponse::error(Some(id), crate::error::JsonRpcError::parse_error(message))
        } else {
            parse_error_response(message)
        };
        serde_json::to_string(&error_response)
            .map_err(|e| Error::Transport(format!("Failed to serialize error: {}", e)))
    }
}

// ============================================================================
// Synchronous stdio transport
// ============================================================================

/// Synchronous stdio transport for simpler use cases
///
/// This version uses blocking I/O and is suitable for simple CLI tools.
///
/// **Note:** This transport does not support server notification forwarding
/// (progress, logging, list changes) because it uses blocking I/O. Use
/// [`StdioTransport`] for full notification support.
pub struct SyncStdioTransport {
    service: JsonRpcService<McpRouter>,
    router: McpRouter,
}

impl SyncStdioTransport {
    /// Create a new synchronous stdio transport
    pub fn new(router: McpRouter) -> Self {
        let service = JsonRpcService::new(router.clone());
        Self { service, router }
    }

    /// Set the exact protocol versions this transport accepts and advertises.
    pub fn protocol_support(mut self, support: ProtocolSupport) -> Self {
        self.service = self.service.protocol_support(support);
        self
    }

    /// Construct and set an exact runtime protocol-version allow-list.
    pub fn protocol_versions<I, V>(
        mut self,
        versions: I,
    ) -> std::result::Result<Self, ProtocolSupportError>
    where
        I: IntoIterator<Item = V>,
        V: Into<String>,
    {
        self.service = self.service.protocol_versions(versions)?;
        Ok(self)
    }

    /// Run the transport synchronously using a tokio runtime
    pub fn run_blocking(&mut self) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| Error::Transport(format!("Failed to create runtime: {}", e)))?;

        let stdin = io::stdin();
        let mut input = stdin.lock();
        let mut stdout = io::stdout();

        tracing::info!("Sync stdio transport started");

        while let Some(frame) = read_frame_blocking(&mut input)? {
            let line = match frame {
                InputFrame::Line(line) => line,
                InputFrame::Undecodable => {
                    let response_json = undecodable_frame_response()?;
                    writeln!(stdout, "{}", response_json)
                        .map_err(|e| Error::Transport(format!("Failed to write error: {}", e)))?;
                    stdout
                        .flush()
                        .map_err(|e| Error::Transport(format!("Failed to flush stdout: {}", e)))?;
                    continue;
                }
            };

            let trimmed = clean_input_line(&line);
            if trimmed.is_empty() {
                continue;
            }

            tracing::debug!(input = %trimmed, "Received message");

            match rt.block_on(process_line(&mut self.service, &self.router, trimmed)) {
                Ok(Some(response)) => {
                    let response_json = serde_json::to_string(&response).map_err(|e| {
                        Error::Transport(format!("Failed to serialize response: {}", e))
                    })?;
                    tracing::debug!(output = %response_json, "Sending response");
                    writeln!(stdout, "{}", response_json).map_err(|e| {
                        Error::Transport(format!("Failed to write to stdout: {}", e))
                    })?;
                    stdout
                        .flush()
                        .map_err(|e| Error::Transport(format!("Failed to flush stdout: {}", e)))?;
                }
                Ok(None) => {
                    // Notification, no response
                }
                Err(e) => {
                    tracing::error!(error = %e, "Error processing message");
                    let error_response = parse_error_response(e.to_string());
                    let response_json = serde_json::to_string(&error_response).map_err(|e| {
                        Error::Transport(format!("Failed to serialize error: {}", e))
                    })?;
                    writeln!(stdout, "{}", response_json)
                        .map_err(|e| Error::Transport(format!("Failed to write error: {}", e)))?;
                    stdout
                        .flush()
                        .map_err(|e| Error::Transport(format!("Failed to flush stdout: {}", e)))?;
                }
            }
        }

        tracing::info!("Stdin closed, shutting down");
        Ok(())
    }
}

// ============================================================================
// Bidirectional stdio transport (with sampling support)
// ============================================================================

/// Pending request waiting for a response
struct PendingRequest {
    response_tx: oneshot::Sender<Result<serde_json::Value>>,
}

/// Bidirectional stdio transport with sampling support
///
/// For legacy protocol requests, this transport supports both incoming
/// requests from clients and outgoing requests to clients (for sampling/LLM
/// requests). It multiplexes stdin/stdout to handle the bidirectional
/// communication. Final 2026-07-28 handlers cannot initiate requests.
///
/// Server notifications (progress, logging, resource/tool/prompt list changes)
/// are automatically forwarded to stdout as JSON-RPC notifications.
///
/// # Example
///
/// ```rust,no_run
/// use tower_mcp::{BoxError, McpRouter, ToolBuilder, CallToolResult};
/// use tower_mcp::transport::stdio::BidirectionalStdioTransport;
/// use tower_mcp::{CreateMessageParams, SamplingMessage};
/// use tower_mcp::extract::{Context, RawArgs};
///
/// #[tokio::main]
/// async fn main() -> Result<(), BoxError> {
///     let tool = ToolBuilder::new("ai-tool")
///         .description("A tool that uses LLM")
///         .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
///             // Request LLM completion from the client
///             let params = CreateMessageParams::new(
///                 vec![SamplingMessage::user("Help me with: ...")],
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
///     let mut transport = BidirectionalStdioTransport::new(router);
///     transport.run().await?;
///     Ok(())
/// }
/// ```
pub struct BidirectionalStdioTransport {
    /// Fires when the read loop ends (#1252).
    stopping: tokio::sync::watch::Sender<bool>,
    service: JsonRpcService<McpRouter>,
    router: McpRouter,
    /// Channel for receiving outgoing requests to send to the client
    request_rx: OutgoingRequestReceiver,
    /// Handle for handlers to send requests to the client
    client_requester: ClientRequesterHandle,
    /// Pending requests waiting for responses
    pending_requests: Arc<Mutex<HashMap<RequestId, PendingRequest>>>,
    /// Channel for receiving server notifications to forward to the client
    notification_rx: NotificationReceiver,
    control_tx: mpsc::UnboundedSender<StdioControl>,
    control_rx: mpsc::UnboundedReceiver<StdioControl>,
}

impl BidirectionalStdioTransport {
    /// Create a new bidirectional stdio transport
    pub fn new(router: McpRouter) -> Self {
        let (request_tx, request_rx) = outgoing_request_channel(32);
        let client_requester: ClientRequesterHandle =
            Arc::new(ChannelClientRequester::new(request_tx));

        let (notif_tx, notification_rx) = notification_channel(256);
        let (control_tx, control_rx) = stdio_control_channel();
        let router = router
            .with_notification_sender(notif_tx)
            .with_client_requester(client_requester.clone());

        let service = JsonRpcService::new(router.clone());

        Self {
            service,
            router,
            request_rx,
            client_requester,
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            notification_rx,
            stopping: stopping_signal(),
            control_tx,
            control_rx,
        }
    }

    /// Return a cloneable handle for graceful subscription closure or server
    /// shutdown while [`Self::run`] is active.
    pub fn handle(&self) -> StdioTransportHandle {
        StdioTransportHandle {
            control_tx: self.control_tx.clone(),
            stopping: self.stopping.subscribe(),
        }
    }

    /// Set the exact protocol versions this transport accepts and advertises.
    ///
    /// Final handlers never receive the legacy client requester, because the
    /// 2026-07-28 protocol forbids servers from initiating JSON-RPC requests.
    pub fn protocol_support(mut self, support: ProtocolSupport) -> Self {
        self.service = self.service.protocol_support(support);
        self
    }

    /// Construct and set an exact runtime protocol-version allow-list.
    pub fn protocol_versions<I, V>(
        mut self,
        versions: I,
    ) -> std::result::Result<Self, ProtocolSupportError>
    where
        I: IntoIterator<Item = V>,
        V: Into<String>,
    {
        self.service = self.service.protocol_versions(versions)?;
        Ok(self)
    }

    /// Get the client requester handle
    ///
    /// The requester is already wired into the router's request context by
    /// [`Self::new`], so legacy handlers can elicit and sample without further
    /// setup. Final 2026-07-28 handlers do not receive it. This getter exposes
    /// the same handle for advanced callers that want to issue
    /// server-to-client requests directly on legacy connections.
    pub fn client_requester(&self) -> ClientRequesterHandle {
        self.client_requester.clone()
    }

    /// Run the transport, processing messages until EOF or error
    ///
    /// Thin wrapper around [`Self::run_with_streams`] that wires up
    /// `tokio::io::stdin()` / `tokio::io::stdout()`.
    pub async fn run(&mut self) -> Result<()> {
        self.run_with_streams(tokio::io::stdin(), tokio::io::stdout())
            .await
    }

    /// Run the transport, reading from `reader` and writing to `writer`.
    ///
    /// Streams-generic counterpart of [`Self::run`]. The writer is held
    /// behind an `Arc<Mutex<_>>` so the outgoing-request and notification
    /// paths can share it with the incoming-message branch -- the same
    /// concurrency model `run()` has always used, just with the streams
    /// supplied by the caller.
    pub async fn run_with_streams<R, W>(&mut self, reader: R, writer: W) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin + Send,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let writer = Arc::new(Mutex::new(writer));
        let mut frames = FrameReader::new(reader);
        #[cfg(feature = "stateless")]
        let mut subscriptions = StdioSubscriptions {
            server_info: Some(self.router.implementation()),
            ..StdioSubscriptions::default()
        }
        .with_observer(self.router.subscription_observer());

        tracing::info!("Bidirectional stdio transport started, waiting for input");

        loop {
            tokio::select! {
                // Handle incoming messages from stdin
                result = frames.next_frame() => {
                    let Some(frame) = result? else {
                        tracing::info!("Stdin closed, shutting down");
                        break;
                    };
                    let InputFrame::Line(line) = frame else {
                        self.write_line(&undecodable_frame_response()?, writer.clone()).await?;
                        continue;
                    };

                    let trimmed = clean_input_line(&line);
                    if trimmed.is_empty() {
                        continue;
                    }

                    self.handle_incoming_message(
                        trimmed,
                        writer.clone(),
                        #[cfg(feature = "stateless")]
                        &mut subscriptions,
                    ).await?;
                }

                // Handle outgoing requests to send to the client
                Some(outgoing) = self.request_rx.recv() => {
                    self.send_outgoing_request(outgoing, writer.clone()).await?;
                }

                // Forward server notifications to the client
                Some(notification) = self.notification_rx.recv() => {
                    #[cfg(feature = "stateless")]
                    if let Some(frames) = subscriptions.route_notification(&notification) {
                        for json in frames {
                            tracing::debug!(output = %json, "Sending subscription notification");
                            self.write_line(&json, writer.clone()).await?;
                        }
                        continue;
                    }
                    if let Some(json) = serialize_notification(&notification) {
                        tracing::debug!(output = %json, "Sending notification");
                        self.write_line(&json, writer.clone()).await?;
                    }
                }

                Some(control) = self.control_rx.recv() => {
                    match control {
                        #[cfg(feature = "stateless")]
                        StdioControl::CloseSubscription(request_id) => {
                            if let Some(json) = subscriptions.close(&request_id)? {
                                self.write_line(&json, writer.clone()).await?;
                            }
                        }
                        StdioControl::Shutdown => {
                            #[cfg(feature = "stateless")]
                            for json in subscriptions.close_all()? {
                                self.write_line(&json, writer.clone()).await?;
                            }
                            break;
                        }
                    }
                }
            }
        }

        let _ = self.stopping.send(true);

        // The read loop is over: any streams still registered die with
        // the connection and cannot receive a terminal frame.
        #[cfg(feature = "stateless")]
        subscriptions.drain_disconnected();
        Ok(())
    }

    /// Handle an incoming message from stdin
    async fn handle_incoming_message<W>(
        &mut self,
        line: &str,
        writer: Arc<Mutex<W>>,
        #[cfg(feature = "stateless")] subscriptions: &mut StdioSubscriptions,
    ) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        tracing::debug!(input = %line, "Received message");

        // Malformed JSON must produce a JSON-RPC parse error response, not
        // tear down the run loop. Per the spec, id is null when the request
        // can't be parsed at all.
        let parsed: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "Malformed JSON on stdin");
                return self.write_parse_error(&e.to_string(), writer).await;
            }
        };

        if let Err(error) = self
            .service
            .inspect_incoming_value(&parsed, crate::inspection::McpDirection::ClientToServer)
        {
            let response = JsonRpcResponse::error(None, error);
            return self
                .write_line(&serde_json::to_string(&response)?, writer)
                .await;
        }

        // Check if this is a response to one of our pending requests
        if parsed.get("method").is_none()
            && (parsed.get("result").is_some() || parsed.get("error").is_some())
        {
            return self.handle_response(&parsed).await;
        }

        #[cfg(feature = "stateless")]
        match subscriptions.handle_input(&self.service, &parsed)? {
            StdioSubscriptionInput::Handled(frames) => {
                for frame in frames {
                    self.write_line(&frame, writer.clone()).await?;
                }
                return Ok(());
            }
            StdioSubscriptionInput::Dispatch(request) => {
                let request_id = request.id.clone();
                let response =
                    dispatch_listen_request(&mut self.service, *request, request_id.clone()).await;
                for frame in subscriptions.complete_listen(request_id, &response)? {
                    self.write_line(&frame, writer.clone()).await?;
                }
                return Ok(());
            }
            StdioSubscriptionInput::NotHandled => {}
        }

        // Check if it's a notification (no id field)
        if !parsed.is_array() && parsed.get("id").is_none() {
            if let Ok(notification) = serde_json::from_str::<JsonRpcNotification>(line) {
                handle_notification(&self.router, notification)?;
            }
            return Ok(());
        }

        // Process as a request. The shape parse can also fail (e.g. id of
        // wrong type); treat it the same way so the loop keeps running.
        let message: JsonRpcMessage = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "JSON did not match JSON-RPC request shape");
                return self.write_parse_error(&e.to_string(), writer).await;
            }
        };
        // Dispatch the request on a spawned task so the run loop stays free to
        // service outgoing server-to-client requests (elicitation/create,
        // sampling/createMessage) and the client's responses to them while the
        // handler is in flight. Handling the request inline here would deadlock
        // any handler that calls `ctx.elicit_form()` / `ctx.sample()`: the
        // handler awaits the client's response, but that response can only be
        // read by this same loop (#923).
        let mut service = self.service.clone();
        tokio::spawn(async move {
            let response_json = match service.call_message(message).await {
                Ok(response) => serde_json::to_string(&response),
                Err(e) => {
                    tracing::error!(error = %e, "Error processing message");
                    serde_json::to_string(&parse_error_response(e.to_string()))
                }
            };
            match response_json {
                Ok(json) => {
                    tracing::debug!(output = %json, "Sending response");
                    if let Err(e) = write_line_locked(&writer, &json).await {
                        tracing::error!(error = %e, "Failed to write response to stdout");
                    }
                }
                Err(e) => tracing::error!(error = %e, "Failed to serialize response"),
            }
        });

        Ok(())
    }

    async fn write_parse_error<W>(&self, message: &str, writer: Arc<Mutex<W>>) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin + Send,
    {
        let error_response = parse_error_response(message);
        let response_json = serde_json::to_string(&error_response)
            .map_err(|e| Error::Transport(format!("Failed to serialize error: {}", e)))?;
        self.write_line(&response_json, writer).await
    }

    /// Handle a response to one of our pending requests
    async fn handle_response(&self, parsed: &serde_json::Value) -> Result<()> {
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
            let mut pending_requests = self.pending_requests.lock().await;
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
    async fn send_outgoing_request<W>(
        &mut self,
        outgoing: OutgoingRequest,
        writer: Arc<Mutex<W>>,
    ) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin + Send,
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
            let mut pending_requests = self.pending_requests.lock().await;
            pending_requests.insert(
                outgoing.id,
                PendingRequest {
                    response_tx: outgoing.response_tx,
                },
            );
        }

        // Send the request
        self.write_line(&request_json, writer).await?;

        Ok(())
    }

    /// Write a line to the shared writer
    async fn write_line<W>(&self, line: &str, writer: Arc<Mutex<W>>) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin + Send,
    {
        write_line_locked(&writer, line).await
    }
}

/// Write a single newline-terminated line to a shared writer and flush it.
///
/// Free-standing counterpart to [`BidirectionalStdioTransport::write_line`] so
/// spawned request-dispatch tasks can write their responses without borrowing
/// the transport.
async fn write_line_locked<W>(writer: &Arc<Mutex<W>>, line: &str) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    let mut writer = writer.lock().await;
    writer
        .write_all(line.as_bytes())
        .await
        .map_err(|e| Error::Transport(format!("Failed to write to stdout: {}", e)))?;
    writer
        .write_all(b"\n")
        .await
        .map_err(|e| Error::Transport(format!("Failed to write newline: {}", e)))?;
    writer
        .flush()
        .await
        .map_err(|e| Error::Transport(format!("Failed to flush stdout: {}", e)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ServerNotification;
    use crate::protocol::{
        LogLevel, LoggingMessageParams, ProgressParams, ProgressToken, TaskStatus, TaskStatusParams,
    };
    use tower_mcp_types::testing::assert_jsonrpc_error_response;

    // =========================================================================
    // parse_error_response tests -- wire-format invariants on the stdio
    // parse-error path (regression coverage for #802 / #803).
    // =========================================================================

    #[test]
    fn parse_error_response_has_null_id_and_code_neg_32700() {
        let resp = parse_error_response("expected value at line 1");
        let json = serde_json::to_value(&resp).unwrap();
        assert_jsonrpc_error_response(&json);
        assert!(
            json["id"].is_null(),
            "id must be null on parse error, got: {json}"
        );
        assert_eq!(json["error"]["code"].as_i64().unwrap(), -32700);
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("expected value"),
            "error.message should carry the parser detail, got: {json}"
        );
    }

    #[test]
    fn parse_error_response_serializes_to_single_line_json() {
        // The stdio loop writes responses line-delimited; the body itself
        // must not contain embedded newlines or it would split the frame.
        let resp = parse_error_response("oops\nstill oops");
        let s = serde_json::to_string(&resp).unwrap();
        assert!(
            !s.contains('\n'),
            "serialized parse-error response must be single-line, got: {s:?}"
        );
    }

    // =========================================================================
    // Frame reading -- the reader itself is covered in `crate::framing`; what
    // belongs here is the server's answer to a frame that will not decode
    // (#1271).
    // =========================================================================

    #[test]
    fn an_undecodable_frame_is_answered_with_a_parse_error() {
        let json: serde_json::Value =
            serde_json::from_str(&undecodable_frame_response().unwrap()).unwrap();
        assert_jsonrpc_error_response(&json);
        assert!(json["id"].is_null(), "nothing to correlate against: {json}");
        assert_eq!(json["error"]["code"].as_i64().unwrap(), -32700);
    }

    // =========================================================================
    // serialize_notification tests
    // =========================================================================

    #[test]
    fn test_serialize_progress_notification() {
        let notification = ServerNotification::Progress(ProgressParams {
            progress_token: ProgressToken::String("tok-1".to_string()),
            progress: 50.0,
            total: Some(100.0),
            message: Some("Halfway there".to_string()),
            meta: None,
        });
        let json = serialize_notification(&notification).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["method"], "notifications/progress");
        assert_eq!(parsed["params"]["progressToken"], "tok-1");
        assert_eq!(parsed["params"]["progress"], 50.0);
        assert_eq!(parsed["params"]["total"], 100.0);
        assert!(parsed.get("id").is_none());
    }

    #[test]
    fn test_serialize_log_message_notification() {
        let notification = ServerNotification::LogMessage(LoggingMessageParams {
            level: LogLevel::Warning,
            logger: Some("test-logger".to_string()),
            data: serde_json::json!("something happened"),
            meta: None,
        });
        let json = serialize_notification(&notification).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["method"], "notifications/message");
        assert_eq!(parsed["params"]["level"], "warning");
        assert_eq!(parsed["params"]["logger"], "test-logger");
    }

    #[test]
    fn test_serialize_resource_updated_notification() {
        let notification = ServerNotification::ResourceUpdated {
            uri: "file:///data.json".to_string(),
        };
        let json = serialize_notification(&notification).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["method"], "notifications/resources/updated");
        assert_eq!(parsed["params"]["uri"], "file:///data.json");
    }

    #[test]
    fn test_serialize_resources_list_changed_notification() {
        let notification = ServerNotification::ResourcesListChanged;
        let json = serialize_notification(&notification).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["method"], "notifications/resources/list_changed");
        assert!(parsed.get("params").is_none());
    }

    #[test]
    fn test_serialize_tools_list_changed_notification() {
        let notification = ServerNotification::ToolsListChanged;
        let json = serialize_notification(&notification).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["method"], "notifications/tools/list_changed");
    }

    #[test]
    fn test_serialize_prompts_list_changed_notification() {
        let notification = ServerNotification::PromptsListChanged;
        let json = serialize_notification(&notification).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["method"], "notifications/prompts/list_changed");
    }

    #[test]
    fn test_serialize_task_status_changed_notification() {
        let notification = ServerNotification::TaskStatusChanged(TaskStatusParams {
            task_id: "task-42".to_string(),
            status: TaskStatus::Working,
            status_message: Some("Processing...".to_string()),
            created_at: "2025-01-01T00:00:00Z".to_string(),
            last_updated_at: "2025-01-01T00:01:00Z".to_string(),
            ttl: None,
            poll_interval: None,
            meta: None,
        });
        let json = serialize_notification(&notification).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["method"], "notifications/tasks");
        assert_eq!(parsed["params"]["taskId"], "task-42");
        assert_eq!(parsed["params"]["status"], "working");
    }

    // =========================================================================
    // process_line tests
    // =========================================================================

    fn make_router() -> McpRouter {
        McpRouter::new().server_info("test-server", "1.0.0")
    }

    async fn init_service(router: &McpRouter) -> JsonRpcService<McpRouter> {
        init_service_for_revision(router, "2025-11-25").await
    }

    async fn init_service_for_revision(
        router: &McpRouter,
        revision: &str,
    ) -> JsonRpcService<McpRouter> {
        let mut service = JsonRpcService::new(router.clone());

        // Initialize the session
        let init_msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": revision,
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "1.0.0" }
            }
        });
        let msg: JsonRpcMessage = serde_json::from_value(init_msg).unwrap();
        let _ = service.call_message(msg).await.unwrap();

        // Send initialized notification
        let notif_line = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let notif = serde_json::from_str::<JsonRpcNotification>(notif_line).unwrap();
        handle_notification(router, notif).unwrap();

        service
    }

    #[tokio::test]
    async fn test_process_line_valid_request() {
        let router = make_router();
        let mut service = init_service(&router).await;

        let line = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let result = process_line(&mut service, &router, line).await;

        let response = result.unwrap().unwrap();
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 1);
        assert!(json.get("result").is_some());
    }

    #[tokio::test]
    async fn test_process_line_notification_returns_none() {
        let router = make_router();
        let mut service = init_service(&router).await;

        let line = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let result = process_line(&mut service, &router, line).await;

        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn stdio_accepts_batch_for_2025_03() {
        let router = make_router();
        let mut service = init_service_for_revision(&router, "2025-03-26").await;
        let line = serde_json::json!([
            {"jsonrpc": "2.0", "id": 1, "method": "ping"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}
        ])
        .to_string();

        let response = process_line(&mut service, &router, &line)
            .await
            .unwrap()
            .unwrap();
        let JsonRpcResponseMessage::Batch(responses) = response else {
            panic!("2025-03-26 stdio batch should return a batch");
        };
        assert_eq!(responses.len(), 2);
    }

    #[tokio::test]
    async fn stdio_rejects_batch_for_2025_11() {
        let router = make_router();
        let mut service = init_service(&router).await;
        let line = serde_json::json!([
            {"jsonrpc": "2.0", "id": 1, "method": "ping"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}
        ])
        .to_string();

        let response = process_line(&mut service, &router, &line)
            .await
            .unwrap()
            .unwrap();
        let JsonRpcResponseMessage::Single(JsonRpcResponse::Error(error)) = response else {
            panic!("2025-11-25 stdio batch should return one error");
        };
        assert_eq!(error.error.code, -32600);
    }

    #[tokio::test]
    async fn test_process_line_malformed_json() {
        let router = make_router();
        let mut service = init_service(&router).await;

        let line = r#"not valid json at all"#;
        let result = process_line(&mut service, &router, line).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_process_line_with_bom_stripped_input_parses() {
        // After clean_input_line, a BOM-prefixed request should parse like
        // any other request and return a normal response.
        let router = make_router();
        let mut service = init_service(&router).await;

        let raw = "\u{feff}{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/list\",\"params\":{}}";
        let cleaned = clean_input_line(raw);
        let result = process_line(&mut service, &router, cleaned).await;

        let response = result.unwrap().unwrap();
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["id"], 7);
        assert!(json["result"]["tools"].is_array());
    }

    #[tokio::test]
    async fn test_process_line_tools_list() {
        let router = make_router();
        let mut service = init_service(&router).await;

        let line = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;
        let result = process_line(&mut service, &router, line).await;

        let response = result.unwrap().unwrap();
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["id"], 2);
        assert!(json["result"]["tools"].is_array());
    }

    #[tokio::test]
    async fn test_process_line_unknown_method() {
        let router = make_router();
        let mut service = init_service(&router).await;

        let line = r#"{"jsonrpc":"2.0","id":3,"method":"nonexistent/method"}"#;
        let result = process_line(&mut service, &router, line).await;

        let response = result.unwrap().unwrap();
        let json = serde_json::to_value(&response).unwrap();
        assert!(json.get("error").is_some());
        assert_eq!(json["error"]["code"], -32601); // Method not found
    }

    // =========================================================================
    // handle_notification tests
    // =========================================================================

    #[test]
    fn test_handle_notification_initialized() {
        let router = make_router();
        let notif_json = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let notif: JsonRpcNotification = serde_json::from_str(notif_json).unwrap();

        let result = handle_notification(&router, notif);
        assert!(result.is_ok());
    }

    #[test]
    fn test_handle_notification_cancelled() {
        let router = make_router();
        let notif_json = r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1,"reason":"timeout"}}"#;
        let notif: JsonRpcNotification = serde_json::from_str(notif_json).unwrap();

        let result = handle_notification(&router, notif);
        assert!(result.is_ok());
    }
}
