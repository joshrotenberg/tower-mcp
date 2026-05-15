//! External notification push example.
//!
//! Demonstrates [`HttpTransport::with_notifications`], the supported path
//! for pushing MCP notifications from outside any request handler --
//! background tasks, lifecycle hooks, anything async that needs to notify
//! subscribed clients without being driven by an incoming request.
//!
//! This example:
//!
//! 1. Creates a [`notification_channel`].
//! 2. Spawns a background task that fires
//!    `notifications/resources/updated` every two seconds.
//! 3. Hands the receiver to [`HttpTransport`], which drains it and
//!    fans the items out to every live session's SSE stream.
//!
//! Run with: `cargo run --example external_notifications --features http`
//!
//! Connect with any MCP client that subscribes to the SSE stream
//! (`GET /` with `Accept: text/event-stream`) -- the client will receive
//! a `notifications/resources/updated` event on every tick.

use std::time::Duration;

use tower_mcp::{
    BoxError, HttpTransport, McpRouter,
    context::{ServerNotification, notification_channel},
};

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tower_mcp=info".parse()?)
                .add_directive("external_notifications=info".parse()?),
        )
        .init();

    let (notif_tx, notif_rx) = notification_channel(64);

    // Background task that pushes a notification on a timer. In a real
    // application this might be a domain event ("chat turn completed"),
    // a database change, a file watcher firing, etc.
    let pusher = notif_tx.clone();
    tokio::spawn(async move {
        let mut counter = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            counter += 1;
            let uri = format!("example://chats/{counter}");
            tracing::info!(uri = %uri, "Pushing resources/updated notification");
            if pusher
                .send(ServerNotification::ResourceUpdated { uri })
                .await
                .is_err()
            {
                tracing::info!("Receiver dropped; pusher exiting");
                break;
            }
        }
    });

    let router = McpRouter::new().server_info("external-notifications-demo", "1.0.0");

    let transport = HttpTransport::with_notifications(router, notif_rx);

    tracing::info!("Listening on 127.0.0.1:3000");
    transport.serve("127.0.0.1:3000").await?;
    Ok(())
}
