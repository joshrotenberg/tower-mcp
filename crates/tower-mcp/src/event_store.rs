//! Pluggable storage for SSE events enabling stream resumption.
//!
//! Mirrors the shape of [`crate::session_store`]: a trait, a serializable
//! record type, an error enum, an in-memory default, and a caching wrapper.
//!
//! SSE streams buffer events per session so clients can replay them after a
//! disconnect via the `Last-Event-ID` header (SEP-1699). When session
//! metadata lives in an external store ([`crate::session_store`]), the event
//! buffer needs to move along with it -- otherwise stream resumption only
//! works if the client reconnects to the exact instance that saw the
//! original events. An external [`EventStore`] fixes this.
//!
//! # Example
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use tower_mcp::event_store::{EventStore, MemoryEventStore};
//!
//! let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::new());
//! // `HttpTransport::new(router).event_store(store)` once wired in.
//! ```

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Default capacity of the in-memory per-session ring buffer.
pub const DEFAULT_MAX_EVENTS_PER_SESSION: usize = 1000;

/// Serializable SSE event record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EventRecord {
    /// Monotonically increasing event ID within a session.
    pub id: u64,
    /// Serialized JSON-RPC payload (notification, response, or request).
    pub data: String,
    /// When the event was produced.
    pub timestamp: SystemTime,
}

impl EventRecord {
    /// Create a new record stamped with the current time.
    pub fn new(id: u64, data: impl Into<String>) -> Self {
        Self {
            id,
            data: data.into(),
            timestamp: SystemTime::now(),
        }
    }
}

/// Errors returned by [`EventStore`] implementations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EventStoreError {
    /// Failed to encode a record.
    #[error("encode error: {0}")]
    Encode(String),
    /// Failed to decode a record.
    #[error("decode error: {0}")]
    Decode(String),
    /// Backend error (e.g. connection failure, transient storage error).
    #[error("backend error: {0}")]
    Backend(String),
}

/// Result alias for event store operations.
pub type Result<T> = std::result::Result<T, EventStoreError>;

/// Storage backend for per-session SSE event logs.
///
/// Implementations persist [`EventRecord`]s keyed by session ID. The default
/// implementation is [`MemoryEventStore`]; external stores (Redis, etc.)
/// typically live in separate crates.
#[async_trait]
pub trait EventStore: Send + Sync + 'static {
    /// Append an event to a session's log.
    async fn append(&self, session_id: &str, event: EventRecord) -> Result<()>;

    /// Return events with IDs strictly greater than `after_id`, in order.
    async fn replay_after(&self, session_id: &str, after_id: u64) -> Result<Vec<EventRecord>>;

    /// Remove all events for a session. Idempotent.
    async fn purge_session(&self, session_id: &str) -> Result<()>;
}

/// In-memory [`EventStore`] with a per-session ring buffer.
///
/// Each session keeps up to `max_events_per_session` events; the oldest is
/// evicted when the buffer is full. This is the default store; external
/// implementations are only needed for cross-instance replay.
#[derive(Debug, Clone)]
pub struct MemoryEventStore {
    inner: Arc<RwLock<HashMap<String, VecDeque<EventRecord>>>>,
    max_events_per_session: usize,
}

impl Default for MemoryEventStore {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_MAX_EVENTS_PER_SESSION)
    }
}

impl MemoryEventStore {
    /// Create a new store using [`DEFAULT_MAX_EVENTS_PER_SESSION`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a store with the given per-session buffer capacity.
    pub fn with_capacity(max_events_per_session: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            max_events_per_session,
        }
    }

    /// Total number of events currently buffered across all sessions.
    pub async fn total_events(&self) -> usize {
        self.inner.read().await.values().map(|v| v.len()).sum()
    }

    /// Number of sessions with at least one buffered event.
    pub async fn session_count(&self) -> usize {
        self.inner.read().await.len()
    }
}

#[async_trait]
impl EventStore for MemoryEventStore {
    async fn append(&self, session_id: &str, event: EventRecord) -> Result<()> {
        let mut map = self.inner.write().await;
        let buf = map.entry(session_id.to_string()).or_default();
        if buf.len() >= self.max_events_per_session {
            buf.pop_front();
        }
        buf.push_back(event);
        Ok(())
    }

    async fn replay_after(&self, session_id: &str, after_id: u64) -> Result<Vec<EventRecord>> {
        let map = self.inner.read().await;
        Ok(match map.get(session_id) {
            Some(buf) => buf.iter().filter(|e| e.id > after_id).cloned().collect(),
            None => Vec::new(),
        })
    }

    async fn purge_session(&self, session_id: &str) -> Result<()> {
        self.inner.write().await.remove(session_id);
        Ok(())
    }
}

/// Two-tier [`EventStore`] composed of a cache frontend and a store backend.
///
/// Writes go to both tiers; replays read from the cache first and fall
/// through to the backend on miss (populating the cache with the missing
/// events). Mirrors [`crate::session_store::CachingSessionStore`].
#[derive(Debug, Clone)]
pub struct CachingEventStore<Cache, Store> {
    cache: Cache,
    store: Store,
}

impl<Cache, Store> CachingEventStore<Cache, Store> {
    /// Create a new caching store with the given cache and backend.
    pub fn new(cache: Cache, store: Store) -> Self {
        Self { cache, store }
    }
}

#[async_trait]
impl<Cache, Store> EventStore for CachingEventStore<Cache, Store>
where
    Cache: EventStore,
    Store: EventStore,
{
    async fn append(&self, session_id: &str, event: EventRecord) -> Result<()> {
        // Write the backend first so durability is established, then mirror.
        self.store.append(session_id, event.clone()).await?;
        self.cache.append(session_id, event).await?;
        Ok(())
    }

    async fn replay_after(&self, session_id: &str, after_id: u64) -> Result<Vec<EventRecord>> {
        let cached = self.cache.replay_after(session_id, after_id).await?;
        if !cached.is_empty() {
            return Ok(cached);
        }
        let from_store = self.store.replay_after(session_id, after_id).await?;
        // Warm the cache best-effort; failures are non-fatal.
        for event in &from_store {
            let _ = self.cache.append(session_id, event.clone()).await;
        }
        Ok(from_store)
    }

    async fn purge_session(&self, session_id: &str) -> Result<()> {
        let cache_result = self.cache.purge_session(session_id).await;
        let store_result = self.store.purge_session(session_id).await;
        cache_result.and(store_result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_store_append_and_replay() {
        let store = MemoryEventStore::new();
        for i in 0..3 {
            store
                .append("s", EventRecord::new(i, format!("e{i}")))
                .await
                .unwrap();
        }

        let all = store.replay_after("s", 0).await.unwrap();
        assert_eq!(all.len(), 2); // ids 1, 2

        let none = store.replay_after("s", 5).await.unwrap();
        assert!(none.is_empty());

        let after_first = store.replay_after("s", 0).await.unwrap();
        assert_eq!(after_first[0].id, 1);
        assert_eq!(after_first[1].id, 2);
    }

    #[tokio::test]
    async fn memory_store_respects_capacity() {
        let store = MemoryEventStore::with_capacity(3);
        for i in 0..5 {
            store
                .append("s", EventRecord::new(i, format!("e{i}")))
                .await
                .unwrap();
        }

        // Oldest two events evicted (ids 0 and 1), leaving 2, 3, 4.
        let events = store.replay_after("s", 0).await.unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].id, 2);
        assert_eq!(events[2].id, 4);
    }

    #[tokio::test]
    async fn memory_store_isolates_sessions() {
        let store = MemoryEventStore::new();
        store.append("a", EventRecord::new(0, "a0")).await.unwrap();
        store.append("b", EventRecord::new(0, "b0")).await.unwrap();

        let a = store.replay_after("a", 0).await.unwrap();
        let b = store.replay_after("b", 0).await.unwrap();
        assert!(a.is_empty() && b.is_empty(), "after_id filters out id 0");

        // Replay from just before 0 (using -1 isn't possible with u64, so
        // we verify by appending a second event and replaying after 0).
        store.append("a", EventRecord::new(1, "a1")).await.unwrap();
        let a1 = store.replay_after("a", 0).await.unwrap();
        assert_eq!(a1.len(), 1);
        assert_eq!(a1[0].data, "a1");
    }

    #[tokio::test]
    async fn memory_store_purge_removes_session() {
        let store = MemoryEventStore::new();
        store.append("s", EventRecord::new(0, "x")).await.unwrap();
        assert_eq!(store.session_count().await, 1);

        store.purge_session("s").await.unwrap();
        assert_eq!(store.session_count().await, 0);
    }

    #[tokio::test]
    async fn memory_store_purge_is_idempotent() {
        MemoryEventStore::new()
            .purge_session("nonexistent")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn dyn_event_store_object_safe() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::new());
        store.append("s", EventRecord::new(0, "x")).await.unwrap();
    }

    #[tokio::test]
    async fn caching_store_writes_to_both_tiers() {
        let cache = MemoryEventStore::new();
        let backend = MemoryEventStore::new();
        let store = CachingEventStore::new(cache.clone(), backend.clone());

        store.append("s", EventRecord::new(0, "x")).await.unwrap();

        assert_eq!(cache.total_events().await, 1);
        assert_eq!(backend.total_events().await, 1);
    }

    #[tokio::test]
    async fn caching_store_reads_from_cache_first() {
        let cache = MemoryEventStore::new();
        let backend = MemoryEventStore::new();

        // Only prime the backend.
        backend
            .append("s", EventRecord::new(0, "b0"))
            .await
            .unwrap();
        backend
            .append("s", EventRecord::new(1, "b1"))
            .await
            .unwrap();

        let store = CachingEventStore::new(cache.clone(), backend);

        // First replay goes to the backend and warms the cache.
        let first = store.replay_after("s", 0).await.unwrap();
        assert_eq!(first.len(), 1); // only id 1

        // Cache should now contain the warmed event.
        assert_eq!(cache.total_events().await, 1);
    }

    #[tokio::test]
    async fn caching_store_purge_clears_both() {
        let cache = MemoryEventStore::new();
        let backend = MemoryEventStore::new();
        let store = CachingEventStore::new(cache.clone(), backend.clone());

        store.append("s", EventRecord::new(0, "x")).await.unwrap();
        store.purge_session("s").await.unwrap();

        assert_eq!(cache.total_events().await, 0);
        assert_eq!(backend.total_events().await, 0);
    }
}
