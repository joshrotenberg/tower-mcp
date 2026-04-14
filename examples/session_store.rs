//! Custom SessionStore example.
//!
//! Demonstrates plugging a custom [`SessionStore`] into [`HttpTransport`] so
//! session metadata is persisted alongside the default in-memory runtime
//! state. Swap the `LoggingStore` here for a real Redis/Postgres-backed
//! implementation in production.
//!
//! Run with: `cargo run --example session_store --features http`

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use tower_mcp::{
    CallToolResult, HttpTransport, McpRouter, ToolBuilder,
    session_store::{
        CachingSessionStore, MemorySessionStore, Result as StoreResult, SessionRecord, SessionStore,
    },
};

/// A [`SessionStore`] wrapper that logs every operation and counts calls.
///
/// In production you'd replace this with a store that writes to Redis,
/// Postgres, DynamoDB, etc.
#[derive(Debug)]
struct LoggingStore<Inner> {
    inner: Inner,
    creates: AtomicUsize,
    saves: AtomicUsize,
    loads: AtomicUsize,
    deletes: AtomicUsize,
}

impl<Inner> LoggingStore<Inner> {
    fn new(inner: Inner) -> Self {
        Self {
            inner,
            creates: AtomicUsize::new(0),
            saves: AtomicUsize::new(0),
            loads: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl<Inner: SessionStore> SessionStore for LoggingStore<Inner> {
    async fn create(&self, record: &mut SessionRecord) -> StoreResult<()> {
        self.creates.fetch_add(1, Ordering::Relaxed);
        tracing::info!(session_id = %record.id, "store.create");
        self.inner.create(record).await
    }

    async fn save(&self, record: &SessionRecord) -> StoreResult<()> {
        self.saves.fetch_add(1, Ordering::Relaxed);
        tracing::info!(session_id = %record.id, "store.save");
        self.inner.save(record).await
    }

    async fn load(&self, id: &str) -> StoreResult<Option<SessionRecord>> {
        self.loads.fetch_add(1, Ordering::Relaxed);
        tracing::info!(session_id = %id, "store.load");
        self.inner.load(id).await
    }

    async fn delete(&self, id: &str) -> StoreResult<()> {
        self.deletes.fetch_add(1, Ordering::Relaxed);
        tracing::info!(session_id = %id, "store.delete");
        self.inner.delete(id).await
    }
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tower_mcp=debug".parse()?)
                .add_directive("session_store=info".parse()?),
        )
        .init();

    // Layer an in-memory cache in front of a logging "backend". In real
    // deployments the backend would be Redis/Postgres/DynamoDB so multiple
    // server instances share session metadata.
    let backend = LoggingStore::new(MemorySessionStore::new());
    let store: Arc<dyn SessionStore> =
        Arc::new(CachingSessionStore::new(MemorySessionStore::new(), backend));

    let tool = ToolBuilder::new("ping")
        .description("Returns pong")
        .handler(|_: tower_mcp::NoParams| async move { Ok(CallToolResult::text("pong")) })
        .build();

    let router = McpRouter::new()
        .server_info("session-store-example", "1.0.0")
        .tool(tool);

    let transport = HttpTransport::new(router)
        .disable_origin_validation()
        .session_store(store);

    tracing::info!("Starting HTTP MCP server with custom session store on http://127.0.0.1:3000");
    tracing::info!("Each initialize/terminate will appear in the logs as store.create/delete");
    transport.serve("127.0.0.1:3000").await?;

    Ok(())
}
