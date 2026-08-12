//! Unit tests for [`McpClient`](super::McpClient) and its message loop.
//!
//! Moved out in #1256. A sibling module rather than an integration
//! test because these reach private items.

use super::*;
use async_trait::async_trait;
use std::sync::Mutex;

/// Mock transport for testing that auto-responds to requests.
///
/// When the client sends a request via `send()`, the mock extracts the
/// request ID, pairs it with the next preconfigured response, and feeds
/// it back through a channel that `recv()` awaits on. This ensures
/// `recv()` blocks when no messages are available (instead of returning
/// EOF), keeping the background message loop alive.
struct MockTransport {
    /// Pre-configured result or error replies (not full envelopes).
    responses: Arc<Mutex<Vec<MockReply>>>,
    /// Index of the next response to use.
    response_idx: Arc<std::sync::atomic::AtomicUsize>,
    /// Channel sender for feeding responses back to `recv()`.
    incoming_tx: mpsc::Sender<String>,
    /// Channel receiver for `recv()` to await on.
    incoming_rx: mpsc::Receiver<String>,
    /// Collected outgoing messages from `send()`.
    outgoing: Arc<Mutex<Vec<String>>>,
    connected: Arc<AtomicBool>,
    /// When set, `send()` fails for notifications (messages without an
    /// `id`), simulating a transport that could not deliver them.
    fail_notification_sends: Arc<AtomicBool>,
}

enum MockReply {
    Result(serde_json::Value),
    Error(JsonRpcError),
}

#[allow(dead_code)]
impl MockTransport {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel(32);
        Self {
            responses: Arc::new(Mutex::new(Vec::new())),
            response_idx: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            incoming_tx: tx,
            incoming_rx: rx,
            outgoing: Arc::new(Mutex::new(Vec::new())),
            connected: Arc::new(AtomicBool::new(true)),
            fail_notification_sends: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Create a mock that auto-responds with the given result payloads.
    ///
    /// When `send()` receives a JSON-RPC request, it extracts the request
    /// ID and pairs it with the next response from this list, sending the
    /// complete JSON-RPC response through the channel for `recv()`.
    fn with_responses(responses: Vec<serde_json::Value>) -> Self {
        let (tx, rx) = mpsc::channel(32);
        Self {
            responses: Arc::new(Mutex::new(
                responses.into_iter().map(MockReply::Result).collect(),
            )),
            response_idx: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            incoming_tx: tx,
            incoming_rx: rx,
            outgoing: Arc::new(Mutex::new(Vec::new())),
            connected: Arc::new(AtomicBool::new(true)),
            fail_notification_sends: Arc::new(AtomicBool::new(false)),
        }
    }

    fn with_replies(responses: Vec<MockReply>) -> Self {
        let (tx, rx) = mpsc::channel(32);
        Self {
            responses: Arc::new(Mutex::new(responses)),
            response_idx: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            incoming_tx: tx,
            incoming_rx: rx,
            outgoing: Arc::new(Mutex::new(Vec::new())),
            connected: Arc::new(AtomicBool::new(true)),
            fail_notification_sends: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl ClientTransport for MockTransport {
    async fn send(&mut self, message: &str) -> Result<()> {
        self.outgoing.lock().unwrap().push(message.to_string());

        // Parse the outgoing message to extract the request ID
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(message) {
            if parsed.get("id").is_none() && self.fail_notification_sends.load(Ordering::Relaxed) {
                return Err(Error::Transport(
                    "mock transport dropped the notification".to_string(),
                ));
            }
            // Only respond to requests (messages with an id and method)
            if let Some(id) = parsed.get("id") {
                let idx = self
                    .response_idx
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let responses = self.responses.lock().unwrap();
                if let Some(reply) = responses.get(idx) {
                    let response = match reply {
                        MockReply::Result(result) => serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": result
                        }),
                        MockReply::Error(error) => serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": error
                        }),
                    };
                    let _ = self.incoming_tx.try_send(response.to_string());
                }
            }
        }

        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<String>> {
        // Await on the channel -- blocks until a message is available
        // or the sender is dropped (returns None = EOF).
        match self.incoming_rx.recv().await {
            Some(msg) => Ok(Some(msg)),
            None => Ok(None),
        }
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    async fn close(&mut self) -> Result<()> {
        self.connected.store(false, Ordering::Relaxed);
        Ok(())
    }
}

fn mock_initialize_response() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": "2025-11-25",
        "serverInfo": {
            "name": "test-server",
            "version": "1.0.0"
        },
        "capabilities": {
            "tools": {}
        }
    })
}

/// #1174: a notification the transport fails to deliver must fail
/// `initialize()`. Reporting success left the handshake incomplete and a
/// strict server rejecting every subsequent request with -32600.
#[tokio::test]
async fn initialize_fails_when_initialized_notification_is_not_delivered() {
    let transport = MockTransport::with_responses(vec![mock_initialize_response()]);
    let fail_notifications = transport.fail_notification_sends.clone();
    let outgoing = transport.outgoing.clone();
    // The initialize request itself succeeds; only the follow-up
    // notification is dropped.
    fail_notifications.store(true, Ordering::Relaxed);

    let client = McpClient::connect(transport).await.unwrap();
    let error = client
        .initialize("test-client", "1.0.0")
        .await
        .expect_err("undelivered notifications/initialized must fail initialize");

    assert!(
        error
            .to_string()
            .contains("failed to deliver notifications/initialized"),
        "error should name the handshake step, got: {error}"
    );
    // The notification was attempted, not skipped.
    assert!(
        outgoing
            .lock()
            .unwrap()
            .iter()
            .any(|message| message.contains("notifications/initialized")),
        "the client must have tried to send the notification"
    );
    // The client must not consider itself initialized.
    assert!(!client.is_initialized());
}

#[tokio::test]
async fn notification_delivery_errors_reach_the_caller() {
    let transport = MockTransport::with_responses(vec![mock_initialize_response()]);
    let fail_notifications = transport.fail_notification_sends.clone();

    let client = McpClient::connect(transport).await.unwrap();
    client.initialize("test-client", "1.0.0").await.unwrap();

    // Healthy so far; now the transport starts dropping notifications.
    fail_notifications.store(true, Ordering::Relaxed);
    let error = client
        .notify("notifications/progress", &serde_json::json!({}))
        .await
        .expect_err("a dropped notification must surface as an error");
    assert!(error.to_string().contains("dropped the notification"));
}

#[tokio::test]
async fn test_client_not_initialized() {
    let client = McpClient::connect(MockTransport::with_responses(vec![]))
        .await
        .unwrap();

    let result = client.list_tools().await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not initialized"));
}

#[tokio::test]
async fn test_client_initialize() {
    let client = McpClient::connect(MockTransport::with_responses(vec![
        mock_initialize_response(),
    ]))
    .await
    .unwrap();

    assert!(!client.is_initialized());

    let result = client.initialize("test-client", "1.0.0").await;
    assert!(result.is_ok());
    assert!(client.is_initialized());

    let server_info = client.server_info().await.unwrap();
    assert_eq!(server_info.server_info.name, "test-server");
}

#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
#[tokio::test]
async fn final_discover_injects_metadata_on_every_request() {
    let transport = MockTransport::with_responses(vec![
        serde_json::json!({
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {}
        }),
        serde_json::json!({
            "resultType": "complete",
            "tools": [],
            "ttlMs": 0,
            "cacheScope": "private"
        }),
    ]);
    let outgoing = transport.outgoing.clone();
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .with_elicitation()
        .connect_simple(transport)
        .await
        .unwrap();

    client.discover("test-client", "1.0.0").await.unwrap();
    client.list_tools().await.unwrap();
    assert_eq!(
        client.selected_protocol_version().await.as_deref(),
        Some("2026-07-28")
    );

    let messages: Vec<serde_json::Value> = outgoing
        .lock()
        .unwrap()
        .iter()
        .map(|message| serde_json::from_str(message).unwrap())
        .collect();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["method"], "server/discover");
    assert_eq!(messages[1]["method"], "tools/list");
    for message in messages {
        let meta = &message["params"]["_meta"];
        assert_eq!(
            meta["io.modelcontextprotocol/protocolVersion"],
            "2026-07-28"
        );
        assert_eq!(
            meta["io.modelcontextprotocol/clientInfo"]["name"],
            "test-client"
        );
        assert!(meta["io.modelcontextprotocol/clientCapabilities"].is_object());
    }
}

#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
fn final_discover_result() -> serde_json::Value {
    serde_json::json!({
        "resultType": "complete",
        "supportedVersions": ["2026-07-28"],
        "capabilities": {},
        "ttlMs": 0,
        "cacheScope": "private"
    })
}

#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
struct SubscriptionTestTransport {
    incoming_tx: mpsc::Sender<String>,
    incoming_rx: mpsc::Receiver<String>,
    outgoing: Arc<Mutex<Vec<String>>>,
    connected: Arc<AtomicBool>,
}

#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
impl SubscriptionTestTransport {
    fn new() -> Self {
        let (incoming_tx, incoming_rx) = mpsc::channel(32);
        Self {
            incoming_tx,
            incoming_rx,
            outgoing: Arc::new(Mutex::new(Vec::new())),
            connected: Arc::new(AtomicBool::new(true)),
        }
    }
}

#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
#[async_trait]
impl ClientTransport for SubscriptionTestTransport {
    async fn send(&mut self, message: &str) -> Result<()> {
        self.outgoing.lock().unwrap().push(message.to_string());
        let value: serde_json::Value =
            serde_json::from_str(message).map_err(|error| Error::Transport(error.to_string()))?;
        let Some(id) = value.get("id").cloned() else {
            return Ok(());
        };
        match value
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
        {
            "server/discover" => {
                self.incoming_tx
                    .send(
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": final_discover_result()
                        })
                        .to_string(),
                    )
                    .await
                    .map_err(|_| Error::Transport("test channel closed".to_string()))?;
            }
            "subscriptions/listen" => {
                let notifications = value["params"]["notifications"].clone();
                self.incoming_tx
                    .send(
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": notifications::SUBSCRIPTIONS_ACKNOWLEDGED,
                            "params": {
                                "_meta": {
                                    "io.modelcontextprotocol/subscriptionId": id
                                },
                                "notifications": notifications
                            }
                        })
                        .to_string(),
                    )
                    .await
                    .map_err(|_| Error::Transport("test channel closed".to_string()))?;

                if value["params"]["notifications"]["promptsListChanged"]
                    == serde_json::Value::Bool(true)
                {
                    self.incoming_tx
                        .send(
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "resultType": "complete",
                                    "_meta": {
                                        "io.modelcontextprotocol/subscriptionId": id
                                    }
                                }
                            })
                            .to_string(),
                        )
                        .await
                        .map_err(|_| Error::Transport("test channel closed".to_string()))?;
                } else {
                    self.incoming_tx
                        .send(
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": notifications::TOOLS_LIST_CHANGED,
                                "params": {
                                    "_meta": {
                                        "io.modelcontextprotocol/subscriptionId": id
                                    }
                                }
                            })
                            .to_string(),
                        )
                        .await
                        .map_err(|_| Error::Transport("test channel closed".to_string()))?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<String>> {
        Ok(self.incoming_rx.recv().await)
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    async fn close(&mut self) -> Result<()> {
        self.connected.store(false, Ordering::Release);
        Ok(())
    }
}

#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
#[tokio::test]
async fn final_subscriptions_correlate_and_cancel_over_message_transport() {
    let transport = SubscriptionTestTransport::new();
    let outgoing = transport.outgoing.clone();
    let incoming = transport.incoming_tx.clone();
    let received = Arc::new(Mutex::new(Vec::new()));

    struct RecordingHandler(Arc<Mutex<Vec<ServerNotification>>>);

    #[async_trait]
    impl ClientHandler for RecordingHandler {
        async fn on_notification(&self, notification: ServerNotification) {
            self.0.lock().unwrap().push(notification);
        }
    }

    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .connect(transport, RecordingHandler(received.clone()))
        .await
        .unwrap();
    client.discover("test-client", "1.0.0").await.unwrap();

    let requested = SubscriptionFilter {
        tools_list_changed: Some(true),
        ..Default::default()
    };
    let mut first = client
        .listen_subscriptions(requested.clone())
        .await
        .unwrap();
    let mut second = client.listen_subscriptions(requested).await.unwrap();
    assert_ne!(first.id(), second.id());
    assert_eq!(
        first.acknowledged().await.unwrap().tools_list_changed,
        Some(true)
    );
    assert_eq!(
        second.acknowledged().await.unwrap().tools_list_changed,
        Some(true)
    );

    for _ in 0..100 {
        if received
            .lock()
            .unwrap()
            .iter()
            .filter(|notification| {
                matches!(
                    notification,
                    ServerNotification::Subscription {
                        notification,
                        ..
                    } if matches!(notification.as_ref(), ServerNotification::ToolsListChanged)
                )
            })
            .count()
            == 2
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    let subscription_ids: Vec<RequestId> = received
        .lock()
        .unwrap()
        .iter()
        .filter_map(|notification| match notification {
            ServerNotification::Subscription {
                subscription_id,
                notification,
            } if matches!(notification.as_ref(), ServerNotification::ToolsListChanged) => {
                Some(subscription_id.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(subscription_ids, [first.id().clone(), second.id().clone()]);

    incoming
        .send(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": notifications::CANCELLED,
                "params": {
                    "requestId": 999,
                    "reason": "not a subscription"
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
    tokio::task::yield_now().await;
    assert!(
        !received.lock().unwrap().iter().any(|notification| matches!(
            notification,
            ServerNotification::SubscriptionCancelled { subscription_id, .. }
                if subscription_id == &RequestId::Number(999)
        ))
    );

    let first_id = first.id().clone();
    let second_id = second.id().clone();
    first.cancel().await.unwrap();
    second.cancel().await.unwrap();
    let cancellation_ids: Vec<RequestId> = outgoing
        .lock()
        .unwrap()
        .iter()
        .filter_map(|message| {
            let value: serde_json::Value = serde_json::from_str(message).unwrap();
            (value["method"] == notifications::CANCELLED)
                .then(|| serde_json::from_value(value["params"]["requestId"].clone()).unwrap())
        })
        .collect();
    assert_eq!(cancellation_ids, [first_id, second_id]);
}

#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
#[tokio::test]
async fn final_subscription_observes_graceful_completion() {
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .connect_simple(SubscriptionTestTransport::new())
        .await
        .unwrap();
    client.discover("test-client", "1.0.0").await.unwrap();

    let mut handle = client
        .listen_subscriptions(SubscriptionFilter {
            prompts_list_changed: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        handle.acknowledged().await.unwrap().prompts_list_changed,
        Some(true)
    );
    let expected_id = handle.id().clone();
    let result = handle.wait().await.unwrap();
    assert!(result.result_type.is_complete());
    assert_eq!(result.meta.subscription_id, expected_id);
}

#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
#[tokio::test]
async fn final_subscription_accepts_server_cancellation_only_for_active_listen() {
    let transport = SubscriptionTestTransport::new();
    let incoming = transport.incoming_tx.clone();
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .connect_simple(transport)
        .await
        .unwrap();
    client.discover("test-client", "1.0.0").await.unwrap();

    let mut handle = client
        .listen_subscriptions(SubscriptionFilter {
            tools_list_changed: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();
    handle.acknowledged().await.unwrap();
    let id = handle.id().clone();
    incoming
        .send(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": notifications::CANCELLED,
                "params": {
                    "requestId": id,
                    "reason": "server shutdown"
                }
            })
            .to_string(),
        )
        .await
        .unwrap();

    let error = handle.wait().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("subscription cancelled by server: server shutdown")
    );
}

#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
fn cacheable_tools_result(name: &str) -> serde_json::Value {
    serde_json::json!({
        "resultType": "complete",
        "tools": [{
            "name": name,
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }],
        "ttlMs": 60_000,
        "cacheScope": "private"
    })
}

#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
#[tokio::test]
async fn final_cache_serves_a_fresh_list_without_a_round_trip() {
    let transport = MockTransport::with_responses(vec![
        final_discover_result(),
        cacheable_tools_result("cached"),
    ]);
    let outgoing = transport.outgoing.clone();
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .connect_simple(transport)
        .await
        .unwrap();
    client.discover("test-client", "1.0.0").await.unwrap();

    let first = client.list_tools().await.unwrap();
    let second = client.list_tools().await.unwrap();

    assert_eq!(first.tools[0].name, "cached");
    assert_eq!(second.tools[0].name, "cached");
    assert_eq!(outgoing.lock().unwrap().len(), 2);
    assert_eq!(client.response_cache_len().await, 1);
}

#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
#[tokio::test]
async fn disabling_final_cache_forces_each_list_request() {
    let transport = MockTransport::with_responses(vec![
        final_discover_result(),
        cacheable_tools_result("first"),
        cacheable_tools_result("second"),
    ]);
    let outgoing = transport.outgoing.clone();
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .disable_response_cache()
        .connect_simple(transport)
        .await
        .unwrap();
    client.discover("test-client", "1.0.0").await.unwrap();

    assert_eq!(client.list_tools().await.unwrap().tools[0].name, "first");
    assert_eq!(client.list_tools().await.unwrap().tools[0].name, "second");
    assert_eq!(outgoing.lock().unwrap().len(), 3);
    assert_eq!(client.response_cache_len().await, 0);
}

#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
#[tokio::test]
async fn list_changed_notification_invalidates_a_fresh_entry() {
    let transport = MockTransport::with_responses(vec![
        final_discover_result(),
        cacheable_tools_result("first"),
        cacheable_tools_result("second"),
    ]);
    let incoming = transport.incoming_tx.clone();
    let outgoing = transport.outgoing.clone();
    let notification_seen = Arc::new(AtomicBool::new(false));
    let handler = NotificationHandler::new().on_tools_changed({
        let notification_seen = notification_seen.clone();
        move || notification_seen.store(true, Ordering::Release)
    });
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .connect(transport, handler)
        .await
        .unwrap();
    client.discover("test-client", "1.0.0").await.unwrap();
    assert_eq!(client.list_tools().await.unwrap().tools[0].name, "first");

    incoming
        .send(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": notifications::TOOLS_LIST_CHANGED,
                "params": {}
            })
            .to_string(),
        )
        .await
        .unwrap();
    for _ in 0..100 {
        if notification_seen.load(Ordering::Acquire) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(notification_seen.load(Ordering::Acquire));

    assert_eq!(client.list_tools().await.unwrap().tools[0].name, "second");
    assert_eq!(outgoing.lock().unwrap().len(), 3);
}

#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
#[tokio::test]
async fn rotating_private_partition_refetches_resource() {
    let resource_result = |text: &str| {
        serde_json::json!({
            "resultType": "complete",
            "contents": [{
                "uri": "config://app",
                "text": text
            }],
            "ttlMs": 60_000,
            "cacheScope": "private"
        })
    };
    let transport = MockTransport::with_responses(vec![
        final_discover_result(),
        resource_result("principal-a"),
        resource_result("principal-b"),
    ]);
    let outgoing = transport.outgoing.clone();
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .response_cache(ClientCacheConfig::default().with_partition("principal-a"))
        .connect_simple(transport)
        .await
        .unwrap();
    client.discover("test-client", "1.0.0").await.unwrap();

    assert_eq!(
        client
            .read_resource("config://app")
            .await
            .unwrap()
            .first_text(),
        Some("principal-a")
    );
    client.set_cache_partition("principal-b").await;
    assert_eq!(
        client
            .read_resource("config://app")
            .await
            .unwrap()
            .first_text(),
        Some("principal-b")
    );
    assert_eq!(outgoing.lock().unwrap().len(), 3);
}

#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
#[tokio::test]
async fn final_tool_call_refreshes_stale_schema_and_retries_once() {
    let transport = MockTransport::with_replies(vec![
        MockReply::Result(final_discover_result()),
        MockReply::Result(cacheable_tools_result("changing-tool")),
        MockReply::Error(JsonRpcError::header_mismatch("stale x-mcp-header mapping")),
        MockReply::Result(cacheable_tools_result("changing-tool")),
        MockReply::Result(serde_json::json!({
            "resultType": "complete",
            "content": [{"type": "text", "text": "retried"}]
        })),
    ]);
    let outgoing = transport.outgoing.clone();
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .connect_simple(transport)
        .await
        .unwrap();
    client.discover("test-client", "1.0.0").await.unwrap();
    client.list_tools().await.unwrap();

    let result = client
        .call_tool("changing-tool", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(result.first_text(), Some("retried"));

    let methods: Vec<String> = outgoing
        .lock()
        .unwrap()
        .iter()
        .map(|message| {
            serde_json::from_str::<serde_json::Value>(message).unwrap()["method"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        methods,
        [
            "server/discover",
            "tools/list",
            "tools/call",
            "tools/list",
            "tools/call"
        ]
    );
}

#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
#[tokio::test]
async fn final_call_tool_as_task_sends_no_legacy_task_parameter() {
    let transport = MockTransport::with_responses(vec![
        final_discover_result(),
        serde_json::json!({
            "resultType": "task",
            "taskId": "task-final",
            "status": "working",
            "createdAt": "2026-07-31T00:00:00Z",
            "lastUpdatedAt": "2026-07-31T00:00:00Z",
            "ttlMs": null,
            "pollIntervalMs": 50
        }),
    ]);
    let outgoing = transport.outgoing.clone();
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .with_tasks()
        .connect_simple(transport)
        .await
        .unwrap();
    client.discover("test-client", "1.0.0").await.unwrap();

    let created = client
        .call_tool_as_task("long-tool", serde_json::json!({}), None)
        .await
        .unwrap();
    assert_eq!(created.task.task_id, "task-final");

    let messages = outgoing.lock().unwrap();
    let call: serde_json::Value = serde_json::from_str(&messages[1]).unwrap();
    assert_eq!(call["method"], "tools/call");
    assert!(
        call["params"].get("task").is_none(),
        "final tools/call leaked the legacy task parameter: {call}"
    );
    assert!(
        call["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"]["extensions"]
            .get(crate::protocol::TASKS_EXTENSION_ID)
            .is_some()
    );
}

#[cfg(any(feature = "protocol-2026-07-28", feature = "stateless"))]
#[tokio::test]
async fn final_stale_schema_retry_is_bounded() {
    let transport = MockTransport::with_replies(vec![
        MockReply::Result(final_discover_result()),
        MockReply::Result(cacheable_tools_result("changing-tool")),
        MockReply::Error(JsonRpcError::header_mismatch("first rejection")),
        MockReply::Result(cacheable_tools_result("changing-tool")),
        MockReply::Error(JsonRpcError::header_mismatch("second rejection")),
    ]);
    let outgoing = transport.outgoing.clone();
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .connect_simple(transport)
        .await
        .unwrap();
    client.discover("test-client", "1.0.0").await.unwrap();
    client.list_tools().await.unwrap();

    let error = client
        .call_tool("changing-tool", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        Error::JsonRpc(error) if error.code == McpErrorCode::HeaderMismatch.code()
    ));
    assert_eq!(outgoing.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn legacy_tool_call_does_not_retry_a_header_mismatch() {
    let transport = MockTransport::with_replies(vec![
        MockReply::Result(mock_initialize_response()),
        MockReply::Error(JsonRpcError::header_mismatch("legacy rejection")),
    ]);
    let outgoing = transport.outgoing.clone();
    let client = McpClient::connect(transport).await.unwrap();
    client.initialize("test-client", "1.0.0").await.unwrap();

    let error = client
        .call_tool("changing-tool", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        Error::JsonRpc(error) if error.code == McpErrorCode::HeaderMismatch.code()
    ));
    let messages = outgoing.lock().unwrap();
    assert_eq!(messages.len(), 3);
    assert!(
        !messages
            .iter()
            .any(|message| message.contains("tools/list"))
    );
}

#[test]
fn stale_tool_schema_errors_are_pre_execution_protocol_errors() {
    for error in [
        Error::JsonRpc(JsonRpcError::header_mismatch("mismatch")),
        Error::JsonRpc(JsonRpcError::method_not_found("tool")),
        Error::JsonRpc(JsonRpcError::invalid_params("arguments")),
    ] {
        assert!(is_stale_tool_schema_error(&error));
    }
    assert!(!is_stale_tool_schema_error(&Error::JsonRpc(
        JsonRpcError::internal_error("executed")
    )));
}

#[tokio::test]
async fn test_list_tools() {
    let client = McpClient::connect(MockTransport::with_responses(vec![
        mock_initialize_response(),
        serde_json::json!({
            "tools": [
                {
                    "name": "test_tool",
                    "description": "A test tool",
                    "inputSchema": {
                        "type": "object",
                        "properties": {}
                    }
                }
            ]
        }),
    ]))
    .await
    .unwrap();

    client.initialize("test-client", "1.0.0").await.unwrap();
    let tools = client.list_tools().await.unwrap();

    assert_eq!(tools.tools.len(), 1);
    assert_eq!(tools.tools[0].name, "test_tool");
}

#[tokio::test]
async fn test_call_tool() {
    let client = McpClient::connect(MockTransport::with_responses(vec![
        mock_initialize_response(),
        serde_json::json!({
            "content": [
                {
                    "type": "text",
                    "text": "Tool result"
                }
            ]
        }),
    ]))
    .await
    .unwrap();

    client.initialize("test-client", "1.0.0").await.unwrap();
    let result = client
        .call_tool("test_tool", serde_json::json!({"arg": "value"}))
        .await
        .unwrap();

    assert!(!result.content.is_empty());
}

#[tokio::test]
async fn test_list_resources() {
    let client = McpClient::connect(MockTransport::with_responses(vec![
        mock_initialize_response(),
        serde_json::json!({
            "resources": [
                {
                    "uri": "file://test.txt",
                    "name": "Test File"
                }
            ]
        }),
    ]))
    .await
    .unwrap();

    client.initialize("test-client", "1.0.0").await.unwrap();
    let resources = client.list_resources().await.unwrap();

    assert_eq!(resources.resources.len(), 1);
    assert_eq!(resources.resources[0].uri, "file://test.txt");
}

#[tokio::test]
async fn test_read_resource() {
    let client = McpClient::connect(MockTransport::with_responses(vec![
        mock_initialize_response(),
        serde_json::json!({
            "contents": [
                {
                    "uri": "file://test.txt",
                    "text": "File contents"
                }
            ]
        }),
    ]))
    .await
    .unwrap();

    client.initialize("test-client", "1.0.0").await.unwrap();
    let result = client.read_resource("file://test.txt").await.unwrap();

    assert_eq!(result.contents.len(), 1);
    assert_eq!(result.contents[0].text.as_deref(), Some("File contents"));
}

#[tokio::test]
async fn test_list_prompts() {
    let client = McpClient::connect(MockTransport::with_responses(vec![
        mock_initialize_response(),
        serde_json::json!({
            "prompts": [
                {
                    "name": "test_prompt",
                    "description": "A test prompt"
                }
            ]
        }),
    ]))
    .await
    .unwrap();

    client.initialize("test-client", "1.0.0").await.unwrap();
    let prompts = client.list_prompts().await.unwrap();

    assert_eq!(prompts.prompts.len(), 1);
    assert_eq!(prompts.prompts[0].name, "test_prompt");
}

#[tokio::test]
async fn test_get_prompt() {
    let client = McpClient::connect(MockTransport::with_responses(vec![
        mock_initialize_response(),
        serde_json::json!({
            "messages": [
                {
                    "role": "user",
                    "content": {
                        "type": "text",
                        "text": "Prompt message"
                    }
                }
            ]
        }),
    ]))
    .await
    .unwrap();

    client.initialize("test-client", "1.0.0").await.unwrap();
    let result = client.get_prompt("test_prompt", None).await.unwrap();

    assert_eq!(result.messages.len(), 1);
}

#[tokio::test]
async fn test_ping() {
    let client = McpClient::connect(MockTransport::with_responses(vec![
        mock_initialize_response(),
        serde_json::json!({}),
    ]))
    .await
    .unwrap();

    client.initialize("test-client", "1.0.0").await.unwrap();
    let result = client.ping().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_with_roots() {
    let roots = vec![Root::new("file:///test")];
    let client = McpClient::builder()
        .with_roots(roots)
        .connect_simple(MockTransport::with_responses(vec![]))
        .await
        .unwrap();

    let current_roots = client.roots().await;
    assert_eq!(current_roots.len(), 1);
}

#[tokio::test]
async fn test_roots_management() {
    let client = McpClient::connect(MockTransport::with_responses(vec![
        mock_initialize_response(),
    ]))
    .await
    .unwrap();

    // Initially no roots
    assert!(client.roots().await.is_empty());

    // Add a root before initialization (no notification sent)
    client.add_root(Root::new("file:///project")).await.unwrap();
    assert_eq!(client.roots().await.len(), 1);

    // Initialize
    client.initialize("test-client", "1.0.0").await.unwrap();

    // Remove a root
    let removed = client.remove_root("file:///project").await.unwrap();
    assert!(removed);
    assert!(client.roots().await.is_empty());

    // Try to remove non-existent root
    let not_removed = client.remove_root("file:///nonexistent").await.unwrap();
    assert!(!not_removed);
}

#[tokio::test]
async fn test_list_roots() {
    let roots = vec![
        Root::new("file:///project1"),
        Root::with_name("file:///project2", "Project 2"),
    ];
    let client = McpClient::builder()
        .with_roots(roots)
        .connect_simple(MockTransport::with_responses(vec![]))
        .await
        .unwrap();

    let result = client.list_roots().await;
    assert_eq!(result.roots.len(), 2);
    assert_eq!(result.roots[1].name, Some("Project 2".to_string()));
}

#[test]
fn test_builder_with_sampling() {
    let builder = McpClientBuilder::new().with_sampling();
    assert!(builder.capabilities.sampling.is_some());
}

#[test]
fn test_builder_with_elicitation() {
    let builder = McpClientBuilder::new().with_elicitation();
    assert!(builder.capabilities.elicitation.is_some());
}

#[test]
fn builder_adds_protocol_extension_without_replacing_other_capabilities() {
    let extension = crate::ExtensionDeclaration::new(
        "com.example/rendering",
        serde_json::json!({"formats": ["html"]}),
    )
    .unwrap();
    let builder = McpClientBuilder::new()
        .with_sampling()
        .with_protocol_extension(extension);

    assert!(builder.capabilities.sampling.is_some());
    assert_eq!(
        builder.capabilities.extensions.as_ref().unwrap()["com.example/rendering"]["formats"][0],
        "html"
    );
}

#[test]
fn test_builder_chaining() {
    let builder = McpClientBuilder::new()
        .with_sampling()
        .with_elicitation()
        .with_roots(vec![Root::new("file:///project")]);
    assert!(builder.capabilities.sampling.is_some());
    assert!(builder.capabilities.elicitation.is_some());
    assert!(builder.capabilities.roots.is_some());
}

#[tokio::test]
async fn test_bidirectional_sampling_round_trip() {
    use crate::protocol::{
        ContentRole, CreateMessageParams, CreateMessageResult, SamplingContent,
        SamplingContentOrArray,
    };

    // A handler that records whether handle_create_message was called
    struct RecordingHandler {
        called: Arc<AtomicBool>,
    }

    #[async_trait]
    impl ClientHandler for RecordingHandler {
        async fn handle_create_message(
            &self,
            _params: CreateMessageParams,
        ) -> std::result::Result<CreateMessageResult, tower_mcp_types::JsonRpcError> {
            self.called.store(true, Ordering::SeqCst);
            Ok(CreateMessageResult {
                content: SamplingContentOrArray::Single(SamplingContent::Text {
                    text: "test response".to_string(),
                    annotations: None,
                    meta: None,
                }),
                model: "test-model".to_string(),
                role: ContentRole::Assistant,
                stop_reason: Some("end_turn".to_string()),
                meta: None,
            })
        }
    }

    let called = Arc::new(AtomicBool::new(false));
    let handler = RecordingHandler {
        called: called.clone(),
    };

    // Build a mock transport, keeping a clone of incoming_tx so we can
    // inject a server-initiated request after the transport is consumed.
    let (inject_tx, rx) = mpsc::channel::<String>(32);
    let responses = vec![mock_initialize_response()];
    let inject_tx_clone = inject_tx.clone();

    let transport = MockTransport {
        responses: Arc::new(Mutex::new(
            responses.into_iter().map(MockReply::Result).collect(),
        )),
        response_idx: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        incoming_tx: inject_tx,
        incoming_rx: rx,
        outgoing: Arc::new(Mutex::new(Vec::new())),
        connected: Arc::new(AtomicBool::new(true)),
        fail_notification_sends: Arc::new(AtomicBool::new(false)),
    };

    let client = McpClient::builder()
        .with_sampling()
        .connect(transport, handler)
        .await
        .unwrap();

    // Initialize the client (this sends initialize request + notification)
    client.initialize("test-client", "1.0.0").await.unwrap();

    // Inject a server-initiated sampling/createMessage request
    let sampling_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 100,
        "method": "sampling/createMessage",
        "params": {
            "messages": [
                {
                    "role": "user",
                    "content": {
                        "type": "text",
                        "text": "Hello"
                    }
                }
            ],
            "maxTokens": 100
        }
    });
    inject_tx_clone
        .send(sampling_request.to_string())
        .await
        .unwrap();

    // Give the background loop time to process
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Verify the handler was called
    assert!(
        called.load(Ordering::SeqCst),
        "handle_create_message should have been called"
    );
}

#[tokio::test]
async fn test_list_resource_templates() {
    let client = McpClient::connect(MockTransport::with_responses(vec![
        mock_initialize_response(),
        serde_json::json!({
            "resourceTemplates": [
                {
                    "uriTemplate": "file:///{path}",
                    "name": "File Template",
                    "description": "A file template"
                }
            ]
        }),
    ]))
    .await
    .unwrap();

    client.initialize("test-client", "1.0.0").await.unwrap();
    let result = client.list_resource_templates().await.unwrap();

    assert_eq!(result.resource_templates.len(), 1);
    assert_eq!(result.resource_templates[0].name, "File Template");
}

#[tokio::test]
async fn test_list_all_tools_single_page() {
    let client = McpClient::connect(MockTransport::with_responses(vec![
        mock_initialize_response(),
        serde_json::json!({
            "tools": [
                {
                    "name": "tool_a",
                    "description": "Tool A",
                    "inputSchema": { "type": "object", "properties": {} }
                },
                {
                    "name": "tool_b",
                    "description": "Tool B",
                    "inputSchema": { "type": "object", "properties": {} }
                }
            ]
        }),
    ]))
    .await
    .unwrap();

    client.initialize("test-client", "1.0.0").await.unwrap();
    let tools = client.list_all_tools().await.unwrap();

    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "tool_a");
    assert_eq!(tools[1].name, "tool_b");
}

#[tokio::test]
async fn test_list_all_tools_paginated() {
    let client = McpClient::connect(MockTransport::with_responses(vec![
        mock_initialize_response(),
        // First page with a next_cursor
        serde_json::json!({
            "tools": [
                {
                    "name": "tool_a",
                    "description": "Tool A",
                    "inputSchema": { "type": "object", "properties": {} }
                }
            ],
            "nextCursor": "page2"
        }),
        // Second page with no next_cursor
        serde_json::json!({
            "tools": [
                {
                    "name": "tool_b",
                    "description": "Tool B",
                    "inputSchema": { "type": "object", "properties": {} }
                }
            ]
        }),
    ]))
    .await
    .unwrap();

    client.initialize("test-client", "1.0.0").await.unwrap();
    let tools = client.list_all_tools().await.unwrap();

    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "tool_a");
    assert_eq!(tools[1].name, "tool_b");
}

#[tokio::test]
async fn test_call_tool_text_success() {
    let client = McpClient::connect(MockTransport::with_responses(vec![
        mock_initialize_response(),
        serde_json::json!({
            "content": [
                { "type": "text", "text": "Hello " },
                { "type": "text", "text": "World" }
            ]
        }),
    ]))
    .await
    .unwrap();

    client.initialize("test-client", "1.0.0").await.unwrap();
    let text = client
        .call_tool_text("test_tool", serde_json::json!({}))
        .await
        .unwrap();

    assert_eq!(text, "Hello World");
}

#[tokio::test]
async fn test_call_tool_text_error() {
    let client = McpClient::connect(MockTransport::with_responses(vec![
        mock_initialize_response(),
        serde_json::json!({
            "content": [
                { "type": "text", "text": "something went wrong" }
            ],
            "isError": true
        }),
    ]))
    .await
    .unwrap();

    client.initialize("test-client", "1.0.0").await.unwrap();
    let result = client
        .call_tool_text("test_tool", serde_json::json!({}))
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("something went wrong"),
        "Error message should contain tool error text, got: {}",
        err
    );
}

#[tokio::test]
async fn test_server_notification_parsing() {
    let notification = parse_server_notification("notifications/tools/list_changed", None);
    assert!(matches!(notification, ServerNotification::ToolsListChanged));

    let notification = parse_server_notification("notifications/resources/list_changed", None);
    assert!(matches!(
        notification,
        ServerNotification::ResourcesListChanged
    ));

    let notification = parse_server_notification(
        "notifications/resources/updated",
        Some(serde_json::json!({"uri": "file:///test"})),
    );
    match notification {
        ServerNotification::ResourceUpdated { uri } => {
            assert_eq!(uri, "file:///test");
        }
        _ => panic!("Expected ResourceUpdated"),
    }

    let notification =
        parse_server_notification("custom/notification", Some(serde_json::json!({"data": 42})));
    match notification {
        ServerNotification::Unknown { method, params } => {
            assert_eq!(method, "custom/notification");
            assert!(params.is_some());
        }
        _ => panic!("Expected Unknown"),
    }

    let notification = parse_server_notification(
        notifications::SUBSCRIPTIONS_ACKNOWLEDGED,
        Some(serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/subscriptionId": 7
            },
            "notifications": {
                "toolsListChanged": true
            }
        })),
    );
    assert!(matches!(
        notification,
        ServerNotification::SubscriptionAcknowledged {
            subscription_id: RequestId::Number(7),
            ..
        }
    ));

    let notification = parse_server_notification(
        notifications::TOOLS_LIST_CHANGED,
        Some(serde_json::json!({
            "_meta": {
                "io.modelcontextprotocol/subscriptionId": "stream-a"
            }
        })),
    );
    assert!(matches!(
        notification,
        ServerNotification::Subscription {
            subscription_id: RequestId::String(id),
            notification,
        } if id == "stream-a"
            && matches!(notification.as_ref(), ServerNotification::ToolsListChanged)
    ));

    let notification = parse_server_notification(
        notifications::CANCELLED,
        Some(serde_json::json!({
            "requestId": 7,
            "reason": "done"
        })),
    );
    assert!(matches!(
        notification,
        ServerNotification::SubscriptionCancelled {
            subscription_id: RequestId::Number(7),
            reason: Some(reason),
        } if reason == "done"
    ));

    let notification = parse_server_notification(
        notifications::TASK_STATUS_CHANGED,
        Some(serde_json::json!({
            "taskId": "legacy-task",
            "status": "completed",
            "createdAt": "2026-08-02T00:00:00Z",
            "lastUpdatedAt": "2026-08-02T00:00:01Z",
            "ttl": null
        })),
    );
    assert!(matches!(
        notification,
        ServerNotification::TaskStatusChanged(TaskStatusParams {
            task_id,
            status: crate::protocol::TaskStatus::Completed,
            ..
        }) if task_id == "legacy-task"
    ));

    let notification = parse_server_notification(
        notifications::TASK_STATUS_CHANGED,
        Some(serde_json::json!({
            "taskId": "final-task",
            "status": "cancelled",
            "createdAt": "2026-08-02T00:00:00Z",
            "lastUpdatedAt": "2026-08-02T00:00:01Z",
            "ttlMs": null,
            "_meta": {
                "io.modelcontextprotocol/subscriptionId": "task-stream"
            }
        })),
    );
    assert!(matches!(
        notification,
        ServerNotification::Subscription {
            subscription_id: RequestId::String(id),
            notification,
        } if id == "task-stream"
            && matches!(
                notification.as_ref(),
                ServerNotification::FinalTaskStatusChanged(params)
                    if params.task.task_id() == "final-task"
                        && params.task.status() == crate::protocol::TaskStatus::Cancelled
            )
    ));
}

// =========================================================================
// handle_response ID correlation
// =========================================================================

fn pending_with(
    ids: &[RequestId],
) -> (
    HashMap<RequestId, PendingRequest>,
    Vec<oneshot::Receiver<Result<serde_json::Value>>>,
) {
    let mut map = HashMap::new();
    let mut rxs = Vec::new();
    for id in ids {
        let (tx, rx) = oneshot::channel();
        map.insert(
            id.clone(),
            PendingRequest {
                method: "test".to_string(),
                response_tx: tx,
                acknowledgment_tx: None,
            },
        );
        rxs.push(rx);
    }
    (map, rxs)
}

#[tokio::test]
async fn test_stringified_numeric_response_id_correlates() {
    // rmcp #1021 analog: a numeric request id 42 answered with a
    // stringified id "42" still correlates.
    let (mut pending, mut rxs) = pending_with(&[RequestId::Number(42)]);

    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "42",
        "result": {"ok": true}
    });
    handle_response(&response, &mut pending);

    assert!(pending.is_empty(), "pending request should be resolved");
    let result = rxs.remove(0).await.unwrap().unwrap();
    assert_eq!(result, serde_json::json!({"ok": true}));
}

#[tokio::test]
async fn test_exact_string_id_takes_precedence() {
    // A genuine string id "42" must match exactly and win over the
    // numeric interpretation when both are pending.
    let (mut pending, mut rxs) =
        pending_with(&[RequestId::String("42".to_string()), RequestId::Number(42)]);

    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "42",
        "result": {"which": "string"}
    });
    handle_response(&response, &mut pending);

    // The string entry resolved; the numeric entry is still pending.
    assert_eq!(pending.len(), 1);
    assert!(pending.contains_key(&RequestId::Number(42)));
    let result = rxs.remove(0).await.unwrap().unwrap();
    assert_eq!(result, serde_json::json!({"which": "string"}));
}

#[tokio::test]
async fn test_non_numeric_string_id_does_not_correlate() {
    // A string id that is not the string form of the pending numeric
    // id must not resolve it.
    let (mut pending, _rxs) = pending_with(&[RequestId::Number(42)]);

    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "not-a-number",
        "result": {}
    });
    handle_response(&response, &mut pending);

    assert_eq!(pending.len(), 1, "numeric request should stay pending");
}
