//! Final-protocol subscription that keeps the interactive command surface live.
//!
//! Stable connections receive ordinary `list_changed` notifications. The
//! final protocol delivers those notifications only through a long-lived
//! `subscriptions/listen` request, so the interactive REPL owns one stream
//! for tools, prompts, and resources. A session reconnect invalidates that
//! stream even when its old transport has not closed yet; unexpected endings
//! retry with bounded exponential backoff.

use std::sync::Arc;
use std::time::Duration;

use tower_mcp::protocol::SubscriptionFilter;

use crate::output::AsyncOutput;
use crate::session::Session;

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Owns the background subscription task for one interactive REPL session.
/// Dropping it stops the task and best-effort cancels its active stream.
pub struct SurfaceSubscription {
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl SurfaceSubscription {
    pub fn start(session: Arc<Session>, output: AsyncOutput) -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(run(session, output, shutdown_rx));
        Self { shutdown_tx, task }
    }
}

impl Drop for SurfaceSubscription {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        self.task.abort();
    }
}

fn requested_filter() -> SubscriptionFilter {
    SubscriptionFilter {
        tools_list_changed: Some(true),
        prompts_list_changed: Some(true),
        resources_list_changed: Some(true),
        ..Default::default()
    }
}

fn honored_names(filter: &SubscriptionFilter) -> Vec<&'static str> {
    let mut names = Vec::new();
    if filter.tools_list_changed == Some(true) {
        names.push("tools");
    }
    if filter.prompts_list_changed == Some(true) {
        names.push("prompts");
    }
    if filter.resources_list_changed == Some(true) {
        names.push("resources");
    }
    names
}

fn missing_names(filter: &SubscriptionFilter) -> Vec<&'static str> {
    let mut names = Vec::new();
    if filter.tools_list_changed != Some(true) {
        names.push("tools");
    }
    if filter.prompts_list_changed != Some(true) {
        names.push("prompts");
    }
    if filter.resources_list_changed != Some(true) {
        names.push("resources");
    }
    names
}

async fn wait_before_retry(
    delay: Duration,
    generations: &mut tokio::sync::watch::Receiver<u64>,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => true,
        changed = generations.changed() => changed.is_ok(),
        _ = shutdown.changed() => false,
    }
}

async fn run(
    session: Arc<Session>,
    output: AsyncOutput,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut generations = session.subscribe_generation();
    let mut backoff = INITIAL_BACKOFF;
    let mut last_acknowledgment_warning = None;

    'subscribe: loop {
        if *shutdown.borrow() {
            return;
        }

        // Mark the current generation observed before opening work on its
        // client. A concurrent reconnect either wakes `changed()` below or is
        // caught by the atomic comparison before an ending is retried.
        let generation = session.generation();
        generations.borrow_and_update();
        let client = session.client();

        let opened = tokio::select! {
            result = client.listen_subscriptions(requested_filter()) => result,
            changed = generations.changed() => {
                if changed.is_err() {
                    return;
                }
                continue 'subscribe;
            },
            _ = shutdown.changed() => return,
        };
        let mut handle = match opened {
            Ok(handle) => handle,
            Err(error) => {
                output.line(format!(
                    "warning: final surface subscription could not open: {error}; retrying in {}s",
                    backoff.as_secs()
                ));
                if !wait_before_retry(backoff, &mut generations, &mut shutdown).await {
                    return;
                }
                backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
                continue;
            }
        };

        let accepted = tokio::select! {
            result = handle.acknowledged() => match result {
                Ok(accepted) => accepted,
                Err(error) => {
                    output.line(format!(
                        "warning: final surface subscription ended before acknowledgment: {error}; retrying in {}s",
                        backoff.as_secs()
                    ));
                    if !wait_before_retry(backoff, &mut generations, &mut shutdown).await {
                        return;
                    }
                    backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
                    continue 'subscribe;
                }
            },
            changed = generations.changed() => {
                if changed.is_err() {
                    return;
                }
                continue 'subscribe;
            },
            _ = shutdown.changed() => return,
        };

        let honored = honored_names(&accepted);
        let missing = missing_names(&accepted);
        if honored.is_empty() {
            output.line(
                "warning: server declined tools, prompts, and resources list-change notifications; final surface auto-refresh is disabled"
            );
            let _ = handle.cancel().await;
            return;
        }
        if !missing.is_empty() {
            let warning = format!(
                "warning: server accepted final surface notifications for {}; {} will not refresh automatically",
                honored.join(", "),
                missing.join(", ")
            );
            if last_acknowledgment_warning.as_deref() != Some(warning.as_str()) {
                output.line(warning.clone());
                last_acknowledgment_warning = Some(warning);
            }
        } else {
            last_acknowledgment_warning = None;
        }
        backoff = INITIAL_BACKOFF;

        let ended = tokio::select! {
            result = handle.wait() => Some(result),
            changed = generations.changed() => {
                if changed.is_err() {
                    return;
                }
                None
            },
            _ = shutdown.changed() => return,
        };
        let Some(ended) = ended else {
            continue;
        };

        // If reconnect won a race with stream completion, move straight to
        // the replacement client rather than sleeping first.
        if session.generation() != generation {
            generations.borrow_and_update();
            continue;
        }

        let reason = match ended {
            Ok(_) => "the server completed it".to_string(),
            Err(error) => error.to_string(),
        };
        output.line(format!(
            "warning: final surface subscription ended ({reason}); retrying in {}s",
            backoff.as_secs()
        ));
        if !wait_before_retry(backoff, &mut generations, &mut shutdown).await {
            return;
        }
        backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use tower_mcp::client::{ChannelTransport, ClientTransport, McpClient, NotificationHandler};
    use tower_mcp::context::{ServerNotification, notification_channel};
    use tower_mcp::{McpRouter, ProtocolSupport, ToolBuilder};

    use crate::session::Connector;

    struct CountingTransport {
        inner: ChannelTransport,
        listens: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ClientTransport for CountingTransport {
        async fn send(&mut self, message: &str) -> tower_mcp::Result<()> {
            let value: serde_json::Value = serde_json::from_str(message)?;
            if value.get("method").and_then(|method| method.as_str())
                == Some("subscriptions/listen")
            {
                self.listens.fetch_add(1, Ordering::SeqCst);
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

    fn router() -> McpRouter {
        McpRouter::new()
            .server_info("subscription-test", "1.0.0")
            .tool(
                ToolBuilder::new("ping")
                    .description("Return pong")
                    .handler(|_: serde_json::Value| async {
                        Ok(tower_mcp::CallToolResult::text("pong"))
                    })
                    .build(),
            )
    }

    async fn final_client(listens: Arc<AtomicUsize>, handler: NotificationHandler) -> McpClient {
        let transport = CountingTransport {
            inner: ChannelTransport::new(router()),
            listens,
        };
        let client = McpClient::builder()
            .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
            .connect(transport, handler)
            .await
            .unwrap();
        client.discover("mcp-repl-test", "0").await.unwrap();
        client
    }

    async fn wait_for_listens(listens: &AtomicUsize, expected: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while listens.load(Ordering::SeqCst) < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("surface subscription was not opened");
    }

    #[test]
    fn acknowledgment_reports_only_honored_notification_classes() {
        let accepted = SubscriptionFilter {
            tools_list_changed: Some(true),
            resources_list_changed: Some(true),
            ..Default::default()
        };
        assert_eq!(honored_names(&accepted), ["tools", "resources"]);
        assert_eq!(missing_names(&accepted), ["prompts"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_surface_stream_allows_requests_and_routes_notifications() {
        let listens = Arc::new(AtomicUsize::new(0));
        let (notification_tx, notification_rx) = notification_channel(8);
        let router = router().with_notification_sender(notification_tx.clone());
        let (changed_tx, mut changed_rx) = tokio::sync::mpsc::unbounded_channel();
        let handler = NotificationHandler::new().on_tools_changed(move || {
            let _ = changed_tx.send(());
        });
        let transport = CountingTransport {
            inner: ChannelTransport::with_notifications(router, notification_rx),
            listens: listens.clone(),
        };
        let client = McpClient::builder()
            .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
            .connect(transport, handler)
            .await
            .unwrap();
        client.discover("mcp-repl-test", "0").await.unwrap();

        let mut subscription = client
            .listen_subscriptions(requested_filter())
            .await
            .unwrap();
        let accepted = subscription.acknowledged().await.unwrap();
        assert!(missing_names(&accepted).is_empty());

        let tools = tokio::time::timeout(Duration::from_secs(1), client.list_all_tools())
            .await
            .expect("ordinary request was blocked by subscription")
            .unwrap();
        assert_eq!(tools.len(), 1);

        notification_tx
            .send(ServerNotification::ToolsListChanged)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), changed_rx.recv())
            .await
            .expect("subscription notification was not routed")
            .expect("notification handler closed");
        subscription.cancel().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reconnect_reopens_the_surface_stream_on_the_fresh_client() {
        let listens = Arc::new(AtomicUsize::new(0));
        let connector_listens = listens.clone();
        let connector: Connector = Box::new(move || {
            let listens = connector_listens.clone();
            Box::pin(async move { Ok(final_client(listens, NotificationHandler::new()).await) })
        });
        let session = Arc::new(Session::new(
            final_client(listens.clone(), NotificationHandler::new()).await,
            Some(connector),
        ));
        let output = AsyncOutput::new(Arc::new(std::sync::atomic::AtomicBool::new(false)), false);
        let subscription = SurfaceSubscription::start(session.clone(), output);

        wait_for_listens(&listens, 1).await;
        assert_eq!(listens.load(Ordering::SeqCst), 1);
        session.reconnect(0).await.unwrap();
        wait_for_listens(&listens, 2).await;
        assert_eq!(listens.load(Ordering::SeqCst), 2);
        assert_eq!(session.generation(), 1);

        drop(subscription);
    }
}
