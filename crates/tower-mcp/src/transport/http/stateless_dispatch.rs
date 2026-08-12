//! The 2026-07-28 stateless dispatch path for [`HttpTransport`](super::HttpTransport).
//!
//! Per-request `_meta` extraction, modern-protocol classification, the
//! process-local `subscriptions/listen` subscription registry, and the SSE
//! plumbing that upgrades a sessionless POST response when a handler emits a
//! notification ahead of its terminal response. Gated on the whole module
//! (`#[cfg(feature = "stateless")]` on the `mod` declaration in `http.rs`)
//! rather than per item, since every item here needs the feature -- that
//! also means none of these carry their own `#[cfg]` any more, which removes
//! the risk of one going out of sync with a neighbor after a future edit.
//!
//! Split out of `http.rs` in #1256 (phase 3).

use super::*;

/// SEP-2575 per-request `_meta` extraction. Pulls `StatelessRequestMeta` from
/// the parsed request params and inserts it into the per-request `Extensions`
/// so handlers can read it via `ctx.per_request_meta()`. No-op if the request
/// has no `_meta`, params aren't an object, or the meta can't deserialize.
pub(super) fn stash_per_request_meta(req: &JsonRpcRequest, ext: &mut crate::router::Extensions) {
    if let Some(params) = req.params.as_ref()
        && let Some(meta) = crate::stateless::StatelessRequestMeta::from_params(params)
    {
        ext.insert(meta);
    }
}

pub(super) struct ModernSubscription {
    subscription_id: RequestId,
    filter: SubscriptionFilter,
    tx: mpsc::UnboundedSender<String>,
    started: std::time::Instant,
}

/// Process-local registry for sessionless final-protocol subscriptions.
///
/// The 2026-07-28 transport deliberately has no session or replay state.
/// Each listen POST owns one sender, removed when its response stream drops.
pub(super) struct ModernSubscriptionRegistry {
    next_key: AtomicU64,
    pub(super) subscriptions: std::sync::Mutex<HashMap<u64, ModernSubscription>>,
    server_info: Option<Implementation>,
    observer: Option<Arc<dyn crate::transport::subscriptions::SubscriptionObserver>>,
}

impl ModernSubscriptionRegistry {
    pub(super) fn new(
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

    pub(super) fn register(
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
    pub(super) fn publish(&self, notification: &ServerNotification) -> bool {
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

    pub(super) fn len(&self) -> usize {
        self.subscriptions.lock().unwrap().len()
    }

    /// Gracefully finish every active HTTP listen stream.
    pub(super) fn close_all(&self) -> usize {
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

impl Default for ModernSubscriptionRegistry {
    fn default() -> Self {
        Self::new(None, None)
    }
}

pub(super) struct ModernSubscriptionGuard {
    key: u64,
    registry: Arc<ModernSubscriptionRegistry>,
}

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

/// Map protocol errors whose final Streamable HTTP binding assigns a
/// non-success status. Errors emitted after an SSE stream has opened remain
/// in-band because the HTTP status is already committed.
pub(super) fn modern_response_status(response: &JsonRpcResponse) -> StatusCode {
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

/// Returns `true` when the given protocol version string enables stateless
/// (sessionless) mode for the HTTP transport.
///
/// Stateless mode is introduced in the 2026-07-28 protocol (SEP-2575 /
/// SEP-2567). Only the exact, compiled-and-enabled version opts in; unknown
/// future dates must not silently inherit revision-specific behavior.
pub(super) fn is_stateless_protocol_version(version: &str) -> bool {
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
pub(super) fn stamp_server_info(response: &mut JsonRpcResponse, implementation: &Implementation) {
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
pub(super) struct CancelOnDisconnect(Option<crate::context::CancellationToken>);

impl CancelOnDisconnect {
    pub(super) fn arm(token: crate::context::CancellationToken) -> Self {
        Self(Some(token))
    }

    pub(super) fn disarm(&mut self) {
        self.0 = None;
    }
}

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
pub(super) struct StatelessSseContext {
    pub(super) version: String,
    pub(super) method: String,
    pub(super) cancel_guard: CancelOnDisconnect,
    pub(super) server_identity: Option<Implementation>,
    pub(super) subscriptions: Arc<ModernSubscriptionRegistry>,
}

pub(super) fn stateless_sse_with_notifications(
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
pub(super) async fn handle_modern_subscriptions_listen_sse(
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
            ephemeral.session().mark_preinitialized();
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
