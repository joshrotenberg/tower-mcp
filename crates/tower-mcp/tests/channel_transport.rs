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
