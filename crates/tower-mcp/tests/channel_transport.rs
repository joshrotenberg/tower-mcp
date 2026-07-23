//! ChannelTransport behavior tests (#955): server-notification delivery to
//! the in-process client, and concurrent request processing.

use std::sync::Arc;
use std::time::Duration;

use tower_mcp::client::{ChannelTransport, McpClient, NotificationHandler};
use tower_mcp::context::{ServerNotification, notification_channel};
use tower_mcp::extract::RawArgs;
use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

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
