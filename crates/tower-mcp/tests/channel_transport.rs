//! ChannelTransport behavior tests (#955): server-notification delivery to
//! the in-process client, and concurrent request processing.

use std::sync::Arc;
use std::time::Duration;

use tower_mcp::client::{ChannelTransport, McpClient, NotificationHandler};
use tower_mcp::context::{ServerNotification, notification_channel};
use tower_mcp::extract::RawArgs;
use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

#[cfg(feature = "dynamic-tools")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "dynamic-tools")]
use tower_mcp::PromptBuilder;

fn slow_tool(name: &str, delay_ms: u64) -> tower_mcp::Tool {
    ToolBuilder::new(name)
        .description("Sleeps, then answers")
        .extractor_handler((), move |RawArgs(_): RawArgs| async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            Ok(CallToolResult::text("done"))
        })
        .build()
}

/// A host-pushed notification arrives client-side while a request is still
/// in flight.
#[tokio::test]
async fn notification_delivered_while_request_in_flight() {
    let (notif_tx, notif_rx) = notification_channel(64);
    let router = McpRouter::new()
        .server_info("channel-test", "1.0.0")
        .tool(slow_tool("slow", 400))
        .with_notification_sender(notif_tx.clone());

    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let handler = NotificationHandler::new().on_tools_changed(move || {
        let _ = seen_tx.send(());
    });

    let transport = ChannelTransport::with_notifications(router, notif_rx);
    let client = Arc::new(
        McpClient::connect_with_handler(transport, handler)
            .await
            .expect("connect"),
    );
    client
        .initialize("test", "1.0.0")
        .await
        .expect("initialize");

    // Start a slow call, then push a notification while it is in flight.
    let call_client = client.clone();
    let call = tokio::spawn(async move {
        call_client
            .call_tool("slow", serde_json::json!({}))
            .await
            .expect("slow call")
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    notif_tx
        .send(ServerNotification::ToolsListChanged)
        .await
        .expect("push notification");

    // The notification must arrive before the slow call completes.
    tokio::time::timeout(Duration::from_millis(200), seen_rx.recv())
        .await
        .expect("notification not delivered while request was in flight")
        .expect("handler channel closed");
    assert!(!call.is_finished(), "slow call should still be in flight");

    let result = call.await.expect("join");
    assert!(!result.is_error);
}

/// Two concurrent calls complete independently: the slow call does not block
/// the fast one.
#[tokio::test]
async fn concurrent_requests_do_not_serialize() {
    let router = McpRouter::new()
        .server_info("channel-test", "1.0.0")
        .tool(slow_tool("slow", 500))
        .tool(slow_tool("fast", 10));

    let transport = ChannelTransport::new(router);
    let client = Arc::new(McpClient::connect(transport).await.expect("connect"));
    client
        .initialize("test", "1.0.0")
        .await
        .expect("initialize");

    let start = std::time::Instant::now();

    let slow_client = client.clone();
    let slow = tokio::spawn(async move {
        slow_client
            .call_tool("slow", serde_json::json!({}))
            .await
            .expect("slow call");
        start.elapsed()
    });
    // Give the slow call a head start so serial processing would block us.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let fast_client = client.clone();
    let fast = tokio::spawn(async move {
        fast_client
            .call_tool("fast", serde_json::json!({}))
            .await
            .expect("fast call");
        start.elapsed()
    });

    let (slow_elapsed, fast_elapsed) = (slow.await.expect("join"), fast.await.expect("join"));

    assert!(
        fast_elapsed < Duration::from_millis(250),
        "fast call should not wait behind the slow call, took {fast_elapsed:?}"
    );
    assert!(
        slow_elapsed >= Duration::from_millis(500),
        "slow call should actually be slow, took {slow_elapsed:?}"
    );
}

#[cfg(feature = "dynamic-tools")]
#[tokio::test]
async fn dynamic_prompt_initializer_runs_only_when_prompts_are_accessed() {
    let calls = Arc::new(AtomicU64::new(0));
    let (router, prompts) = McpRouter::new()
        .server_info("lazy-prompts", "1.0.0")
        .with_dynamic_prompts();
    let initializer_calls = calls.clone();
    let router = router.dynamic_prompt_initializer(move || {
        initializer_calls.fetch_add(1, Ordering::SeqCst);
        if !prompts.contains("lazy") {
            prompts.register(PromptBuilder::new("lazy").user_message("loaded"));
        }
        Ok(())
    });
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .expect("connect");
    client.initialize("test", "1.0.0").await.unwrap();
    client.list_tools().await.unwrap();
    client.list_resources().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let listed = client.list_prompts().await.unwrap();
    assert_eq!(listed.prompts[0].name, "lazy");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let loaded = client.get_prompt("lazy", None).await.unwrap();
    assert_eq!(loaded.messages.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// The default constructor also delivers router-emitted notifications (the
/// receiver is no longer discarded).
#[tokio::test]
async fn default_constructor_delivers_router_notifications() {
    let router = McpRouter::new().server_info("channel-test", "1.0.0").tool(
        ToolBuilder::new("notifier")
            .description("Emits a log notification via context")
            .extractor_handler(
                (),
                |ctx: tower_mcp::extract::Context, RawArgs(_): RawArgs| async move {
                    ctx.send_log(tower_mcp::protocol::LoggingMessageParams::new(
                        tower_mcp::protocol::LogLevel::Info,
                        serde_json::json!("hello from the tool"),
                    ));
                    Ok(CallToolResult::text("ok"))
                },
            )
            .build(),
    );

    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let handler = NotificationHandler::new().on_log_message(move |msg| {
        let _ = seen_tx.send(format!("{:?}", msg.data));
    });

    let transport = ChannelTransport::new(router);
    let client = McpClient::connect_with_handler(transport, handler)
        .await
        .expect("connect");
    client
        .initialize("test", "1.0.0")
        .await
        .expect("initialize");

    client
        .call_tool("notifier", serde_json::json!({}))
        .await
        .expect("call");

    let msg = tokio::time::timeout(Duration::from_millis(500), seen_rx.recv())
        .await
        .expect("log notification not delivered")
        .expect("handler channel closed");
    assert!(msg.contains("hello from the tool"), "got: {msg}");
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn final_typed_tool_calls_follow_request_log_level_updates() {
    use std::sync::Mutex;

    use tower_mcp::ProtocolSupport;
    use tower_mcp::extract::{Context, State};
    use tower_mcp::protocol::LogLevel as ProtocolLogLevel;
    use tower_mcp::stateless::LogLevel as RequestLogLevel;

    #[derive(Clone, Default)]
    struct SeenLevels(Arc<Mutex<Vec<Option<RequestLogLevel>>>>);

    let seen = SeenLevels::default();
    let tool = ToolBuilder::new("inspect_log_level")
        .description("Records the per-request log threshold")
        .extractor_handler(
            seen.clone(),
            |State(seen): State<SeenLevels>, ctx: Context, RawArgs(_): RawArgs| async move {
                seen.0
                    .lock()
                    .unwrap()
                    .push(ctx.per_request_meta().and_then(|meta| meta.log_level));
                Ok(CallToolResult::text("ok"))
            },
        )
        .build();
    let router = McpRouter::new()
        .server_info("log-level-test", "1.0.0")
        .tool(tool);
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .connect_simple(ChannelTransport::new(router))
        .await
        .expect("connect");
    client
        .discover("log-level-client", "1.0.0")
        .await
        .expect("discover");

    client
        .call_tool("inspect_log_level", serde_json::json!({}))
        .await
        .expect("unset call");
    client
        .set_request_log_level(Some(ProtocolLogLevel::Warning))
        .await;
    client
        .call_tool("inspect_log_level", serde_json::json!({}))
        .await
        .expect("updated call");
    client.set_request_log_level(None).await;
    client
        .call_tool("inspect_log_level", serde_json::json!({}))
        .await
        .expect("cleared call");

    assert_eq!(
        *seen.0.lock().unwrap(),
        [None, Some(RequestLogLevel::Warning), None]
    );
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn final_subscription_listen_filters_channel_transport_notifications() {
    use tower_mcp::ProtocolSupport;
    use tower_mcp::protocol::SubscriptionFilter;

    let (notification_tx, notification_rx) = notification_channel(64);
    let router = McpRouter::new()
        .server_info("channel-test", "1.0.0")
        .with_notification_sender(notification_tx.clone());
    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
    let handler = NotificationHandler::new().on_resource_updated(move |uri| {
        let _ = seen_tx.send(uri);
    });
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .connect(
            ChannelTransport::with_notifications(router, notification_rx),
            handler,
        )
        .await
        .expect("connect");
    client
        .discover("channel-subscription-test", "1.0.0")
        .await
        .expect("discover");

    let mut subscription = client
        .listen_subscriptions(SubscriptionFilter {
            resource_subscriptions: Some(vec!["test://watched".to_string()]),
            ..Default::default()
        })
        .await
        .expect("subscriptions/listen");
    let accepted = subscription.acknowledged().await.expect("acknowledgment");
    assert_eq!(
        accepted.resource_subscriptions,
        Some(vec!["test://watched".to_string()])
    );

    notification_tx
        .send(ServerNotification::ResourceUpdated {
            uri: "test://watched".to_string(),
        })
        .await
        .expect("notification send");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), seen_rx.recv())
            .await
            .expect("notification timeout"),
        Some("test://watched".to_string())
    );
    subscription.cancel().await.expect("subscription cancel");
}

// =============================================================================
// Layered dispatch (#1181)
// =============================================================================

mod layered {
    use super::*;
    use std::sync::Mutex;
    use std::task::{Context, Poll};

    use tower_mcp::router::{RouterRequest, RouterResponse};

    /// Records the method name of every request that passes through.
    #[derive(Clone)]
    struct RecordingLayer {
        seen: Arc<Mutex<Vec<String>>>,
    }

    #[derive(Clone)]
    struct RecordingService<S> {
        inner: S,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl<S> tower::Layer<S> for RecordingLayer {
        type Service = RecordingService<S>;

        fn layer(&self, inner: S) -> Self::Service {
            RecordingService {
                inner,
                seen: self.seen.clone(),
            }
        }
    }

    impl<S> tower_service::Service<RouterRequest> for RecordingService<S>
    where
        S: tower_service::Service<RouterRequest, Response = RouterResponse>,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = S::Future;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

        fn call(&mut self, request: RouterRequest) -> Self::Future {
            self.seen
                .lock()
                .unwrap()
                .push(request.inner.method_name().to_string());
            self.inner.call(request)
        }
    }

    /// The acceptance criterion of #1181: middleware on the in-process
    /// transport observes the same requests it would see over stdio or HTTP.
    #[tokio::test]
    async fn layer_observes_requests_made_through_the_client() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let router = McpRouter::new()
            .server_info("layered-channel", "1.0.0")
            .tool(slow_tool("quick", 0));

        let transport = ChannelTransport::layer(router, RecordingLayer { seen: seen.clone() });
        let client = McpClient::connect(transport).await.expect("connect");
        client
            .initialize("test", "1.0.0")
            .await
            .expect("initialize");
        client.list_tools().await.expect("list tools");
        client
            .call_tool("quick", serde_json::json!({}))
            .await
            .expect("call tool");

        let observed = seen.lock().unwrap().clone();
        assert_eq!(observed, vec!["initialize", "tools/list", "tools/call"]);
    }

    /// Middleware errors surface as JSON-RPC errors through CatchError, and a
    /// budget generous enough for the handler lets the call through.
    #[tokio::test]
    async fn layer_errors_become_jsonrpc_errors() {
        let router = McpRouter::new()
            .server_info("layered-channel", "1.0.0")
            .tool(slow_tool("slow", 200));

        let transport = ChannelTransport::layer(
            router,
            tower::timeout::TimeoutLayer::new(Duration::from_millis(20)),
        );
        let client = McpClient::connect(transport).await.expect("connect");
        client
            .initialize("test", "1.0.0")
            .await
            .expect("fast requests pass the timeout layer");

        let error = client
            .call_tool("slow", serde_json::json!({}))
            .await
            .expect_err("the slow call must exceed the layer budget");
        assert!(
            error.to_string().to_lowercase().contains("timed out")
                || error.to_string().to_lowercase().contains("timeout"),
            "error should come from the timeout layer, got: {error}"
        );
    }

    /// The layered constructor with a caller-owned notification channel keeps
    /// host-pushed notifications flowing.
    #[tokio::test]
    async fn layered_transport_still_delivers_notifications() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (notif_tx, notif_rx) = notification_channel(64);
        let router = McpRouter::new()
            .server_info("layered-channel", "1.0.0")
            .with_notification_sender(notif_tx.clone());

        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let handler = NotificationHandler::new().on_tools_changed(move || {
            let _ = seen_tx.send(());
        });

        let transport = ChannelTransport::layer_with_notifications(
            router,
            RecordingLayer { seen: seen.clone() },
            notif_rx,
        );
        let client = McpClient::connect_with_handler(transport, handler)
            .await
            .expect("connect");
        client
            .initialize("test", "1.0.0")
            .await
            .expect("initialize");

        notif_tx
            .send(ServerNotification::ToolsListChanged)
            .await
            .expect("push notification");
        tokio::time::timeout(Duration::from_secs(2), seen_rx.recv())
            .await
            .expect("notification must arrive through the layered transport")
            .expect("handler channel open");

        assert_eq!(*seen.lock().unwrap(), vec!["initialize"]);
    }
}

// =============================================================================
// Server-initiated requests (#1191)
// =============================================================================

mod bidirectional {
    use super::*;
    use async_trait::async_trait;

    use tower_mcp::client::ClientHandler;
    use tower_mcp::error::JsonRpcError;
    use tower_mcp::extract::Context;
    use tower_mcp::protocol::{
        ContentRole, CreateMessageParams, CreateMessageResult, ElicitAction, ElicitFormParams,
        ElicitFormSchema, ElicitResult, SamplingContent, SamplingContentOrArray,
    };

    struct AnsweringHandler;

    #[async_trait]
    impl ClientHandler for AnsweringHandler {
        async fn handle_elicit(
            &self,
            _params: tower_mcp::protocol::ElicitRequestParams,
        ) -> Result<ElicitResult, JsonRpcError> {
            Ok(ElicitResult {
                action: ElicitAction::Accept,
                content: Some(
                    [(
                        "answer".to_string(),
                        tower_mcp::protocol::ElicitFieldValue::String("yes".to_string()),
                    )]
                    .into_iter()
                    .collect(),
                ),
                meta: None,
            })
        }

        async fn handle_create_message(
            &self,
            _params: CreateMessageParams,
        ) -> Result<CreateMessageResult, JsonRpcError> {
            Ok(CreateMessageResult {
                role: ContentRole::Assistant,
                content: SamplingContentOrArray::Single(SamplingContent::Text {
                    text: "sampled".to_string(),
                    annotations: None,
                    meta: None,
                }),
                model: "test-model".to_string(),
                stop_reason: None,
                meta: None,
            })
        }
    }

    /// #1191: ChannelTransport wired no client requester, so any handler
    /// calling back to the client failed with "no client requester
    /// configured", making elicitation and sampling structurally impossible
    /// in-process.
    #[tokio::test]
    async fn elicitation_reaches_the_client_handler() {
        let ask = ToolBuilder::new("ask")
            .description("Ask the client a question")
            .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
                let result = ctx
                    .elicit_form(ElicitFormParams {
                        mode: None,
                        message: "confirm?".to_string(),
                        requested_schema: ElicitFormSchema::default(),
                        meta: None,
                    })
                    .await?;
                Ok(CallToolResult::text(format!("{:?}", result.action)))
            })
            .build();

        let router = McpRouter::new()
            .server_info("bidi-channel", "1.0.0")
            .tool(ask);
        let client =
            McpClient::connect_with_handler(ChannelTransport::new(router), AnsweringHandler)
                .await
                .expect("connect");
        client
            .initialize("test", "1.0.0")
            .await
            .expect("initialize");

        let result = client
            .call_tool("ask", serde_json::json!({}))
            .await
            .expect("the elicitation must round trip to the client handler");
        assert_eq!(result.all_text(), "Accept");
    }

    #[tokio::test]
    async fn sampling_reaches_the_client_handler() {
        let summarize = ToolBuilder::new("summarize")
            .description("Ask the client's model")
            .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
                let result = ctx
                    .sample(CreateMessageParams {
                        messages: Vec::new(),
                        max_tokens: 16,
                        system_prompt: None,
                        temperature: None,
                        stop_sequences: Vec::new(),
                        model_preferences: None,
                        include_context: None,
                        metadata: None,
                        tools: None,
                        tool_choice: None,
                        task: None,
                        meta: None,
                    })
                    .await?;
                Ok(CallToolResult::text(result.model))
            })
            .build();

        let router = McpRouter::new()
            .server_info("bidi-channel", "1.0.0")
            .tool(summarize);
        let client =
            McpClient::connect_with_handler(ChannelTransport::new(router), AnsweringHandler)
                .await
                .expect("connect");
        client
            .initialize("test", "1.0.0")
            .await
            .expect("initialize");

        let result = client
            .call_tool("summarize", serde_json::json!({}))
            .await
            .expect("the sampling request must round trip to the client handler");
        assert_eq!(result.all_text(), "test-model");
    }
}

// =============================================================================
// Client-requested progress (#1190)
// =============================================================================

mod progress {
    use super::*;
    use std::sync::Mutex;

    use tower_mcp::extract::Context;
    use tower_mcp::protocol::ProgressParams;

    fn reporting_router() -> McpRouter {
        let scan = ToolBuilder::new("scan")
            .description("Reports progress as it goes")
            .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
                for step in 1..=3u32 {
                    ctx.report_progress(step as f64, Some(3.0), Some("scanning"))
                        .await;
                }
                Ok(CallToolResult::text("done"))
            })
            .build();
        McpRouter::new()
            .server_info("progress-server", "1.0.0")
            .tool(scan)
    }

    fn collecting_handler() -> (NotificationHandler, Arc<Mutex<Vec<ProgressParams>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let handler = NotificationHandler::new().on_progress(move |params| {
            sink.lock().unwrap().push(params);
        });
        (handler, seen)
    }

    /// #1190: a server only emits progress when the request carries a token,
    /// and the client had no way to send one, so `report_progress` was
    /// silently discarded for every consumer of the typed API.
    #[tokio::test]
    async fn opting_in_delivers_progress_notifications() {
        let (handler, seen) = collecting_handler();
        let client = McpClient::builder()
            .request_progress()
            .connect(ChannelTransport::new(reporting_router()), handler)
            .await
            .expect("connect");
        client
            .initialize("test", "1.0.0")
            .await
            .expect("initialize");

        client
            .call_tool("scan", serde_json::json!({}))
            .await
            .expect("call tool");

        // The notifications race the response, so allow the loop to drain.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let progress = seen.lock().unwrap();
        assert_eq!(progress.len(), 3, "every step must arrive: {progress:?}");
        assert_eq!(progress[0].progress, 1.0);
        assert_eq!(progress[2].progress, 3.0);
        assert_eq!(progress[0].total, Some(3.0));
    }

    /// Off by default: a client that never asked for progress must not add
    /// wire traffic or receive frames.
    #[tokio::test]
    async fn progress_is_off_by_default() {
        let (handler, seen) = collecting_handler();
        let client =
            McpClient::connect_with_handler(ChannelTransport::new(reporting_router()), handler)
                .await
                .expect("connect");
        client
            .initialize("test", "1.0.0")
            .await
            .expect("initialize");

        client
            .call_tool("scan", serde_json::json!({}))
            .await
            .expect("call tool");

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            seen.lock().unwrap().is_empty(),
            "a client that did not opt in receives no progress"
        );
    }

    /// Each request gets its own token, so a server can attribute progress to
    /// the call that produced it.
    #[tokio::test]
    async fn each_request_carries_a_distinct_token() {
        let (handler, seen) = collecting_handler();
        let client = McpClient::builder()
            .request_progress()
            .connect(ChannelTransport::new(reporting_router()), handler)
            .await
            .expect("connect");
        client
            .initialize("test", "1.0.0")
            .await
            .expect("initialize");

        client
            .call_tool("scan", serde_json::json!({}))
            .await
            .unwrap();
        client
            .call_tool("scan", serde_json::json!({}))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;
        let progress = seen.lock().unwrap();
        assert_eq!(progress.len(), 6, "both calls report: {progress:?}");
        let tokens: std::collections::BTreeSet<String> = progress
            .iter()
            .map(|p| format!("{:?}", p.progress_token))
            .collect();
        assert_eq!(tokens.len(), 2, "one distinct token per call: {tokens:?}");
    }
}

// =============================================================================
// Client-side request cancellation (#1202)
// =============================================================================

mod cancellation {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration as StdDuration;

    use tower_mcp::extract::Context;

    /// Records every frame the client sends, so a `notifications/cancelled`
    /// can be observed with the id it carried.
    #[derive(Clone, Default)]
    struct FrameLog(Arc<Mutex<Vec<serde_json::Value>>>);

    impl FrameLog {
        fn cancelled_ids(&self) -> Vec<serde_json::Value> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|f| f["method"] == "notifications/cancelled")
                .map(|f| f["params"]["requestId"].clone())
                .collect()
        }
    }

    /// Wraps ChannelTransport to observe outbound frames.
    struct LoggingTransport {
        inner: ChannelTransport,
        log: FrameLog,
    }

    #[async_trait::async_trait]
    impl tower_mcp::client::ClientTransport for LoggingTransport {
        async fn send(&mut self, message: &str) -> tower_mcp::Result<()> {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(message) {
                self.log.0.lock().unwrap().push(v);
            }
            self.inner.send(message).await
        }
        async fn recv(&mut self) -> tower_mcp::Result<Option<String>> {
            self.inner.recv().await
        }
        fn is_connected(&self) -> bool {
            self.inner.is_connected()
        }
        async fn close(&mut self) -> tower_mcp::Result<()> {
            self.inner.close().await
        }
    }

    fn slow_router() -> McpRouter {
        let slow = ToolBuilder::new("slow")
            .description("Runs long enough to be abandoned")
            .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                tokio::time::sleep(StdDuration::from_secs(30)).await;
                Ok(CallToolResult::text("finished"))
            })
            .build();
        McpRouter::new()
            .server_info("cancel-test", "1.0.0")
            .tool(slow)
    }

    async fn connected(log: FrameLog) -> McpClient {
        let transport = LoggingTransport {
            inner: ChannelTransport::new(slow_router()),
            log,
        };
        let client = McpClient::connect(transport).await.expect("connect");
        client
            .initialize("test", "1.0.0")
            .await
            .expect("initialize");
        client
    }

    /// #1202: abandoning a call left the server working on an answer nobody
    /// was waiting for. Dropping the future is what a Rust caller does on
    /// Ctrl-C or a timeout, so that is the trigger.
    #[tokio::test]
    async fn dropping_a_call_cancels_it_on_the_server() {
        let log = FrameLog::default();
        let client = connected(log.clone()).await;

        // Abandon the call the way a timeout or a select! loser would.
        let abandoned = tokio::time::timeout(
            StdDuration::from_millis(100),
            client.call_tool("slow", serde_json::json!({})),
        )
        .await;
        assert!(abandoned.is_err(), "the call must still be running");

        // The guard runs on drop; give the loop a moment to send.
        tokio::time::sleep(StdDuration::from_millis(100)).await;

        let cancelled = log.cancelled_ids();
        assert_eq!(
            cancelled.len(),
            1,
            "exactly one cancellation must be sent: {cancelled:?}"
        );
        assert!(
            cancelled[0].is_number(),
            "the id must keep its original JSON type, not become a string: {:?}",
            cancelled[0]
        );
    }

    /// A completed call must never be cancelled: the guard disarms on the
    /// response, so a normal call emits nothing.
    #[tokio::test]
    async fn a_completed_call_is_not_cancelled() {
        let quick = ToolBuilder::new("quick")
            .description("Returns immediately")
            .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                Ok(CallToolResult::text("done"))
            })
            .build();
        let log = FrameLog::default();
        let transport = LoggingTransport {
            inner: ChannelTransport::new(
                McpRouter::new()
                    .server_info("cancel-test", "1.0.0")
                    .tool(quick),
            ),
            log: log.clone(),
        };
        let client = McpClient::connect(transport).await.unwrap();
        client.initialize("test", "1.0.0").await.unwrap();

        client
            .call_tool("quick", serde_json::json!({}))
            .await
            .expect("call completes");
        tokio::time::sleep(StdDuration::from_millis(100)).await;

        assert!(
            log.cancelled_ids().is_empty(),
            "a completed call must not be cancelled: {:?}",
            log.cancelled_ids()
        );
    }

    /// Cancelling one call must not disturb another in flight, which is the
    /// risk that made matching ids out of a wire trace unworkable downstream.
    #[tokio::test]
    async fn cancelling_one_call_leaves_another_running() {
        let log = FrameLog::default();
        let client = Arc::new(connected(log.clone()).await);

        // A long call we intend to keep.
        let keeper = {
            let client = client.clone();
            tokio::spawn(async move { client.call_tool("slow", serde_json::json!({})).await })
        };
        tokio::time::sleep(StdDuration::from_millis(50)).await;

        // A second call we abandon.
        let _ = tokio::time::timeout(
            StdDuration::from_millis(100),
            client.call_tool("slow", serde_json::json!({})),
        )
        .await;
        tokio::time::sleep(StdDuration::from_millis(100)).await;

        let cancelled = log.cancelled_ids();
        assert_eq!(cancelled.len(), 1, "only the abandoned call: {cancelled:?}");
        assert!(
            !keeper.is_finished(),
            "the call that was not abandoned must still be pending"
        );
        keeper.abort();
    }
}

// =============================================================================
// Panic containment (#1230)
// =============================================================================

mod panics {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;
    use tower_mcp::PanicPolicy;
    use tower_mcp::extract::Context;

    #[derive(Clone, Default)]
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CaptureWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("capture lock")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl CaptureWriter {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().expect("capture lock").clone())
                .expect("tracing output is UTF-8")
        }
    }

    fn panicking_router(catch: bool) -> McpRouter {
        let boom = ToolBuilder::new("boom")
            .description("Panics")
            .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                panic!("overflow when adding duration to `SystemTime`");
                #[allow(unreachable_code)]
                Ok(CallToolResult::text("unreachable"))
            })
            .build();
        let fine = ToolBuilder::new("fine")
            .description("Works")
            .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                Ok(CallToolResult::text("ok"))
            })
            .build();

        let router = McpRouter::new()
            .server_info("panic-test", "1.0.0")
            .tool(boom)
            .tool(fine);
        if catch { router.catch_panics() } else { router }
    }

    /// #1230: a panicking handler took down the whole server. The blast
    /// radius is the point: a bug in one tool should fail that call, not
    /// disconnect every client on the process.
    #[tokio::test]
    async fn a_panicking_tool_does_not_take_down_the_server() {
        let client = McpClient::connect(ChannelTransport::new(panicking_router(true)))
            .await
            .expect("connect");
        client
            .initialize("test", "1.0.0")
            .await
            .expect("initialize");

        let result = client
            .call_tool("boom", serde_json::json!({}))
            .await
            .expect("the call must return, not kill the connection");
        assert!(result.is_error, "a caught panic is an error result");
        assert_eq!(
            result.all_text(),
            "tool 'boom' panicked: overflow when adding duration to `SystemTime`",
            "the compatibility builder keeps its detailed response"
        );

        // The decisive assertion: the connection survives and other tools
        // still work.
        let after = client
            .call_tool("fine", serde_json::json!({}))
            .await
            .expect("the server must still be serving");
        assert_eq!(after.all_text(), "ok");
    }

    /// #1306: strict servers can isolate a panic without copying handler
    /// data into either the client response or Tower's tracing event.
    #[tokio::test(flavor = "current_thread")]
    async fn a_redacted_policy_omits_string_and_non_string_panic_payloads() {
        const TOOL_NAME: &str = "private.provider.operation";
        const PAYLOAD: &str = "secret path /private/provider/home";
        const SAFE_MESSAGE: &str = "internal tool failure";

        let string_panic = ToolBuilder::new(TOOL_NAME)
            .description("Panics with sensitive text")
            .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                panic!("{PAYLOAD}");
                #[allow(unreachable_code)]
                Ok(CallToolResult::text("unreachable"))
            })
            .build();
        let odd_panic = ToolBuilder::new("private.provider.non-string")
            .description("Panics with an opaque payload")
            .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                std::panic::panic_any(42_u8);
                #[allow(unreachable_code)]
                Ok(CallToolResult::text("unreachable"))
            })
            .build();
        let fine = ToolBuilder::new("fine")
            .description("Works")
            .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                Ok(CallToolResult::text("ok"))
            })
            .build();

        let captured = CaptureWriter::default();
        let trace_output = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::ERROR)
            .with_writer(move || captured.clone())
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);

        let router = McpRouter::new()
            .server_info("panic-test", "1.0.0")
            .tool(string_panic)
            .tool(odd_panic)
            .tool(fine)
            .catch_panics_with(PanicPolicy::redacted(SAFE_MESSAGE));
        let client = McpClient::connect(ChannelTransport::new(router))
            .await
            .expect("connect");
        client
            .initialize("test", "1.0.0")
            .await
            .expect("initialize");

        for name in [TOOL_NAME, "private.provider.non-string"] {
            let result = client
                .call_tool(name, serde_json::json!({}))
                .await
                .expect("caught panic returns a result");
            assert!(result.is_error);
            assert_eq!(result.all_text(), SAFE_MESSAGE);
        }

        let after = client
            .call_tool("fine", serde_json::json!({}))
            .await
            .expect("call");
        assert_eq!(after.all_text(), "ok");

        let traces = trace_output.contents();
        assert!(traces.contains("tool handler panicked"), "{traces}");
        assert!(!traces.contains(TOOL_NAME), "tool name leaked: {traces}");
        assert!(!traces.contains(PAYLOAD), "panic payload leaked: {traces}");
        assert!(
            !traces.contains("non-string payload"),
            "opaque panic payload was inspected: {traces}"
        );
    }

    /// Client and tracing tool-name disclosure are separate switches.
    #[tokio::test(flavor = "current_thread")]
    async fn panic_policy_tool_name_disclosures_are_independent() {
        const TOOL_NAME: &str = "private.provider.named";
        const PAYLOAD: &str = "private panic payload";

        let boom = ToolBuilder::new(TOOL_NAME)
            .description("Panics")
            .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                panic!("{PAYLOAD}");
                #[allow(unreachable_code)]
                Ok(CallToolResult::text("unreachable"))
            })
            .build();
        let captured = CaptureWriter::default();
        let trace_output = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::ERROR)
            .with_writer(move || captured.clone())
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);

        let router = McpRouter::new()
            .server_info("panic-test", "1.0.0")
            .tool(boom)
            .catch_panics_with(
                PanicPolicy::redacted("internal tool failure")
                    .client_tool_name("client-safe-tool")
                    .log_tool_name("log-safe-tool")
                    .include_payload_in_logs(true),
            );
        let client = McpClient::connect(ChannelTransport::new(router))
            .await
            .expect("connect");
        client
            .initialize("test", "1.0.0")
            .await
            .expect("initialize");

        let result = client
            .call_tool(TOOL_NAME, serde_json::json!({}))
            .await
            .expect("caught panic returns a result");
        assert_eq!(
            result.all_text(),
            "tool 'client-safe-tool': internal tool failure"
        );

        let traces = trace_output.contents();
        assert!(
            traces.contains("log-safe-tool"),
            "log alias absent: {traces}"
        );
        assert!(
            !traces.contains("client-safe-tool"),
            "client alias crossed into tracing: {traces}"
        );
        assert!(
            !traces.contains(TOOL_NAME),
            "raw tool name leaked: {traces}"
        );
        assert!(
            traces.contains(PAYLOAD),
            "the independently enabled log payload is absent: {traces}"
        );
    }

    /// Off by default: opting in is a statement that availability matters
    /// more than failing fast, and that is the caller's call to make.
    #[tokio::test]
    async fn panics_are_not_caught_by_default() {
        let client = McpClient::connect(ChannelTransport::new(panicking_router(false)))
            .await
            .expect("connect");
        client
            .initialize("test", "1.0.0")
            .await
            .expect("initialize");

        // The panic unwinds out of the spawned dispatch task rather than
        // becoming a tidy error result, so the caller does not receive one.
        let result = tokio::time::timeout(
            Duration::from_millis(500),
            client.call_tool("boom", serde_json::json!({})),
        )
        .await;
        let caught_as_error =
            matches!(&result, Ok(Ok(r)) if r.is_error && r.all_text().contains("panicked"));
        assert!(
            !caught_as_error,
            "without catch_panics the panic must not be converted: {result:?}"
        );
    }

    /// A tool that returns an error normally is unaffected by the wrapper.
    #[tokio::test]
    async fn catching_panics_does_not_disturb_ordinary_results() {
        let client = McpClient::connect(ChannelTransport::new(panicking_router(true)))
            .await
            .expect("connect");
        client
            .initialize("test", "1.0.0")
            .await
            .expect("initialize");

        let result = client
            .call_tool("fine", serde_json::json!({}))
            .await
            .expect("call");
        assert!(!result.is_error);
        assert_eq!(result.all_text(), "ok");
    }

    /// #1340: the panic boundary above (`a_panicking_tool_does_not_take_down_the_server`)
    /// only exercises a plain, extractor-based handler. `Tool::call_outcome_with_context`
    /// picks `mrtr_handler` over `service` when both could apply, so an MRTR
    /// handler reaches `invoke_tool`'s boundary through a different branch,
    /// and nothing had put a panic through it.
    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn a_panicking_mrtr_tool_is_caught_on_a_plain_call() {
        use tower_mcp::RequestContext;
        use tower_mcp::protocol::RequestOutcome;

        let boom = ToolBuilder::new("mrtr_boom")
            .description("Panics before ever asking for input")
            .mrtr_handler::<serde_json::Value, _, _>(|_ctx: RequestContext, _input| async move {
                panic!("mrtr handler exploded");
                #[allow(unreachable_code)]
                Ok(RequestOutcome::Complete(CallToolResult::text(
                    "unreachable",
                )))
            })
            .build();
        let fine = ToolBuilder::new("fine")
            .description("Works")
            .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                Ok(CallToolResult::text("ok"))
            })
            .build();

        let router = McpRouter::new()
            .server_info("panic-mrtr", "1.0.0")
            .tool(boom)
            .tool(fine)
            .catch_panics();
        let client = McpClient::connect(ChannelTransport::new(router))
            .await
            .expect("connect");
        client
            .initialize("test", "1.0.0")
            .await
            .expect("initialize");

        let result = client
            .call_tool("mrtr_boom", serde_json::json!({}))
            .await
            .expect("the call must return, not kill the connection");
        assert!(result.is_error, "a caught panic is an error result");
        assert!(result.all_text().contains("panicked"));

        let after = client
            .call_tool("fine", serde_json::json!({}))
            .await
            .expect("the server must still be serving");
        assert_eq!(after.all_text(), "ok");
    }
}
