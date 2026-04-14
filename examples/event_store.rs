//! Custom EventStore example.
//!
//! Demonstrates plugging a custom [`EventStore`] into [`HttpTransport`] so
//! buffered SSE events are persisted alongside the default in-memory ring
//! buffer. Swap the `LoggingStore` here for a Redis-backed implementation
//! in production to enable cross-instance SSE resumption.
//!
//! Run with: `cargo run --example event_store --features http`
//!
//! Once running, call the `slow_task` tool and then disconnect/reconnect
//! with a `Last-Event-ID` header to see events replayed from the store.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, HttpTransport, McpRouter, ToolBuilder,
    event_store::{
        CachingEventStore, EventRecord, EventStore, MemoryEventStore, Result as StoreResult,
    },
    extract::{Context, Json},
};

#[derive(Debug, Deserialize, JsonSchema)]
struct SlowTaskInput {
    #[serde(default = "default_steps")]
    steps: u32,
}

fn default_steps() -> u32 {
    5
}

/// An [`EventStore`] wrapper that logs every operation and counts calls.
///
/// In production you'd replace this with a store that writes to Redis or
/// another shared backend.
#[derive(Debug)]
struct LoggingEventStore<Inner> {
    inner: Inner,
    appends: AtomicUsize,
    replays: AtomicUsize,
    purges: AtomicUsize,
}

impl<Inner> LoggingEventStore<Inner> {
    fn new(inner: Inner) -> Self {
        Self {
            inner,
            appends: AtomicUsize::new(0),
            replays: AtomicUsize::new(0),
            purges: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl<Inner: EventStore> EventStore for LoggingEventStore<Inner> {
    async fn append(&self, session_id: &str, event: EventRecord) -> StoreResult<()> {
        self.appends.fetch_add(1, Ordering::Relaxed);
        tracing::info!(session_id, event_id = event.id, "event.append");
        self.inner.append(session_id, event).await
    }

    async fn replay_after(&self, session_id: &str, after_id: u64) -> StoreResult<Vec<EventRecord>> {
        self.replays.fetch_add(1, Ordering::Relaxed);
        let events = self.inner.replay_after(session_id, after_id).await?;
        tracing::info!(
            session_id,
            after_id,
            replayed = events.len(),
            "event.replay_after"
        );
        Ok(events)
    }

    async fn purge_session(&self, session_id: &str) -> StoreResult<()> {
        self.purges.fetch_add(1, Ordering::Relaxed);
        tracing::info!(session_id, "event.purge_session");
        self.inner.purge_session(session_id).await
    }
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tower_mcp=debug".parse()?)
                .add_directive("event_store=info".parse()?),
        )
        .init();

    // Cache in front of a logging "backend". In production the backend
    // would be Redis or similar so multiple server instances share the
    // event log and clients can resume SSE streams from any instance.
    let backend = LoggingEventStore::new(MemoryEventStore::new());
    let store: Arc<dyn EventStore> =
        Arc::new(CachingEventStore::new(MemoryEventStore::new(), backend));

    // A tool that emits progress notifications so there's something to
    // buffer in the event log.
    let slow_task = ToolBuilder::new("slow_task")
        .description("Simulate a slow task that reports progress")
        .extractor_handler(
            (),
            |ctx: Context, Json(input): Json<SlowTaskInput>| async move {
                let steps = input.steps.min(10);
                for i in 0..steps {
                    ctx.report_progress(
                        i as f64,
                        Some(steps as f64),
                        Some(&format!("step {}/{}", i + 1, steps)),
                    )
                    .await;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Ok(CallToolResult::text(format!("finished {steps} steps")))
            },
        )
        .build();

    let router = McpRouter::new()
        .server_info("event-store-example", "1.0.0")
        .tool(slow_task);

    let transport = HttpTransport::new(router)
        .disable_origin_validation()
        .event_store(store);

    tracing::info!("Starting HTTP MCP server with custom event store on http://127.0.0.1:3000");
    tracing::info!("Call slow_task and reconnect with Last-Event-ID to trigger replays");
    transport.serve("127.0.0.1:3000").await?;

    Ok(())
}
