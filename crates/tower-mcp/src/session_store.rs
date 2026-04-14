//! Pluggable session storage for HTTP and WebSocket transports.
//!
//! Session state is split into two layers:
//! - **Persistent metadata** ([`SessionRecord`]) -- serializable, can be stored
//!   in Redis, Postgres, etc. Persisted via the [`SessionStore`] trait.
//! - **Runtime state** -- broadcast channels, pending request handles, service
//!   instances. Held in-memory per server instance; cannot be serialized.
//!
//! By default transports use [`MemorySessionStore`], which keeps metadata in an
//! in-process `HashMap` (behavior identical to earlier versions). External
//! stores (Redis, Postgres, etc.) can be plugged in to share session metadata
//! across server instances behind a load balancer.
//!
//! # Example
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use tower_mcp::session_store::{MemorySessionStore, SessionStore};
//!
//! let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
//! // `HttpTransport::new(router).session_store(store)` (once wired in).
//! ```
//!
//! This API follows the shape of
//! [`tower-sessions`](https://docs.rs/tower-sessions), adapted for MCP's session
//! model (header-based `mcp-session-id`, structured metadata instead of
//! arbitrary `HashMap<String, Value>`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::protocol::{ClientCapabilities, Implementation};

/// Serializable session metadata persisted by a [`SessionStore`].
///
/// Contains everything needed to reconstruct a session after a restart or
/// across server instances. Does **not** contain runtime state (channels,
/// pending requests) -- those are rebuilt locally on restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionRecord {
    /// Session ID (as used in the `mcp-session-id` header).
    pub id: String,
    /// Negotiated MCP protocol version (e.g. `"2025-11-25"`).
    pub protocol_version: String,
    /// Client implementation info from the `initialize` request.
    pub client_info: Option<Implementation>,
    /// Client capabilities from the `initialize` request.
    pub client_capabilities: Option<ClientCapabilities>,
    /// When this session was created.
    pub created_at: SystemTime,
    /// When this session was last accessed.
    pub last_accessed: SystemTime,
    /// When this session expires. Implementations may remove expired records.
    pub expires_at: SystemTime,
}

impl SessionRecord {
    /// Create a new record with a generated ID and timestamps.
    ///
    /// `ttl` is used to derive `expires_at` from `now`.
    pub fn new(id: impl Into<String>, protocol_version: impl Into<String>, ttl: Duration) -> Self {
        let now = SystemTime::now();
        Self {
            id: id.into(),
            protocol_version: protocol_version.into(),
            client_info: None,
            client_capabilities: None,
            created_at: now,
            last_accessed: now,
            expires_at: now + ttl,
        }
    }

    /// Refresh `last_accessed` and `expires_at` using the given TTL.
    pub fn touch(&mut self, ttl: Duration) {
        let now = SystemTime::now();
        self.last_accessed = now;
        self.expires_at = now + ttl;
    }

    /// Returns `true` if `expires_at` is in the past.
    pub fn is_expired(&self) -> bool {
        SystemTime::now() >= self.expires_at
    }
}

/// Errors returned by [`SessionStore`] implementations.
///
/// Mirrors the three-variant shape used by `tower-sessions`: encode/decode
/// errors from the record (de)serialization, and catch-all backend errors
/// from the storage layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SessionStoreError {
    /// Failed to encode a [`SessionRecord`] (e.g. serde serialization error).
    #[error("encode error: {0}")]
    Encode(String),
    /// Failed to decode a [`SessionRecord`] (e.g. corrupt data in the backend).
    #[error("decode error: {0}")]
    Decode(String),
    /// Backend error (e.g. connection failure, transient storage error).
    #[error("backend error: {0}")]
    Backend(String),
}

/// Result alias for session store operations.
pub type Result<T> = std::result::Result<T, SessionStoreError>;

/// Storage backend for MCP session metadata.
///
/// Implementations persist [`SessionRecord`]s keyed by session ID. The default
/// implementation is [`MemorySessionStore`]; external stores (Redis, Postgres,
/// etc.) typically live in separate crates.
///
/// # Semantics
///
/// - [`create`](Self::create) must ensure the ID in the record is unique,
///   retrying ID generation if necessary.
/// - [`save`](Self::save) trusts the caller's ID and performs an upsert.
/// - [`load`](Self::load) returns `None` for unknown or expired sessions.
///   Implementations may choose to return expired records and let the caller
///   decide, or filter them out.
/// - [`delete`](Self::delete) is idempotent -- removing a non-existent ID is
///   not an error.
#[async_trait]
pub trait SessionStore: Send + Sync + 'static {
    /// Create a new session record.
    ///
    /// The implementation must ensure `record.id` does not collide with an
    /// existing session, regenerating the ID if needed.
    async fn create(&self, record: &mut SessionRecord) -> Result<()>;

    /// Persist an existing session record. The ID is trusted.
    async fn save(&self, record: &SessionRecord) -> Result<()>;

    /// Load a session record by ID. Returns `None` if unknown or expired.
    async fn load(&self, id: &str) -> Result<Option<SessionRecord>>;

    /// Remove a session record. Idempotent.
    async fn delete(&self, id: &str) -> Result<()>;
}

/// In-memory [`SessionStore`] backed by a `HashMap`.
///
/// This is the default store. Suitable for single-instance deployments. For
/// horizontal scaling, use an external store that shares state across
/// instances.
#[derive(Debug, Default, Clone)]
pub struct MemorySessionStore {
    inner: Arc<RwLock<HashMap<String, SessionRecord>>>,
}

impl MemorySessionStore {
    /// Create a new empty in-memory session store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of records currently in the store.
    ///
    /// Useful for metrics endpoints; not part of the [`SessionStore`] trait.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    /// Returns `true` if the store has no records.
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }

    /// Remove all expired records. Returns the number removed.
    pub async fn cleanup_expired(&self) -> usize {
        let mut map = self.inner.write().await;
        let before = map.len();
        map.retain(|_, record| !record.is_expired());
        before - map.len()
    }
}

#[async_trait]
impl SessionStore for MemorySessionStore {
    async fn create(&self, record: &mut SessionRecord) -> Result<()> {
        let mut map = self.inner.write().await;
        // Ensure uniqueness. UUIDs collide with negligible probability, but
        // retry to be safe and to let callers pass a preferred ID.
        while map.contains_key(&record.id) {
            record.id = uuid::Uuid::new_v4().to_string();
        }
        map.insert(record.id.clone(), record.clone());
        Ok(())
    }

    async fn save(&self, record: &SessionRecord) -> Result<()> {
        self.inner
            .write()
            .await
            .insert(record.id.clone(), record.clone());
        Ok(())
    }

    async fn load(&self, id: &str) -> Result<Option<SessionRecord>> {
        let map = self.inner.read().await;
        Ok(map.get(id).filter(|r| !r.is_expired()).cloned())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.inner.write().await.remove(id);
        Ok(())
    }
}

/// Two-tier [`SessionStore`] composed of a cache frontend and a store backend.
///
/// Reads hit the cache first; on miss, they fall through to the backend and
/// populate the cache. Writes go to both tiers. Deletes remove from both.
///
/// This lets users pair a fast in-process cache (e.g. [`MemorySessionStore`])
/// with a durable backend (e.g. a Redis-backed store), keeping in-memory
/// read performance while gaining cross-instance durability.
///
/// Mirrors `tower_sessions_core::session_store::CachingSessionStore`.
///
/// # Example
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use tower_mcp::session_store::{CachingSessionStore, MemorySessionStore, SessionStore};
///
/// // let backend: Arc<dyn SessionStore> = Arc::new(RedisSessionStore::new(...));
/// # let backend: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
/// let cache = MemorySessionStore::new();
/// let store: Arc<dyn SessionStore> =
///     Arc::new(CachingSessionStore::new(cache, backend));
/// ```
#[derive(Debug, Clone)]
pub struct CachingSessionStore<Cache, Store> {
    cache: Cache,
    store: Store,
}

impl<Cache, Store> CachingSessionStore<Cache, Store> {
    /// Create a new caching store with the given cache and backend.
    pub fn new(cache: Cache, store: Store) -> Self {
        Self { cache, store }
    }
}

#[async_trait]
impl<Cache, Store> SessionStore for CachingSessionStore<Cache, Store>
where
    Cache: SessionStore,
    Store: SessionStore,
{
    async fn create(&self, record: &mut SessionRecord) -> Result<()> {
        // Create in the backend first so the authoritative ID is established,
        // then mirror into the cache.
        self.store.create(record).await?;
        self.cache.save(record).await?;
        Ok(())
    }

    async fn save(&self, record: &SessionRecord) -> Result<()> {
        self.store.save(record).await?;
        self.cache.save(record).await?;
        Ok(())
    }

    async fn load(&self, id: &str) -> Result<Option<SessionRecord>> {
        if let Some(record) = self.cache.load(id).await? {
            return Ok(Some(record));
        }
        match self.store.load(id).await? {
            Some(record) => {
                // Populate the cache for subsequent reads. Failures here are
                // non-fatal — we still return the record.
                let _ = self.cache.save(&record).await;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let cache_result = self.cache.delete(id).await;
        let store_result = self.store.delete(id).await;
        cache_result.and(store_result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sample_record(id: &str) -> SessionRecord {
        SessionRecord::new(id, "2025-11-25", Duration::from_secs(60))
    }

    #[tokio::test]
    async fn memory_store_create_load_delete() {
        let store = MemorySessionStore::new();
        let mut record = sample_record("abc");
        store.create(&mut record).await.unwrap();

        let loaded = store.load("abc").await.unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().id, "abc");

        store.delete("abc").await.unwrap();
        assert!(store.load("abc").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn memory_store_create_regenerates_id_on_collision() {
        let store = MemorySessionStore::new();
        let mut first = sample_record("dup");
        store.create(&mut first).await.unwrap();

        let mut second = sample_record("dup");
        store.create(&mut second).await.unwrap();

        assert_ne!(first.id, second.id);
        assert!(store.load(&first.id).await.unwrap().is_some());
        assert!(store.load(&second.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn memory_store_save_upserts() {
        let store = MemorySessionStore::new();
        let mut record = sample_record("upsert");
        store.create(&mut record).await.unwrap();

        record.protocol_version = "2025-06-18".into();
        store.save(&record).await.unwrap();

        let loaded = store.load("upsert").await.unwrap().unwrap();
        assert_eq!(loaded.protocol_version, "2025-06-18");
    }

    #[tokio::test]
    async fn memory_store_hides_expired_records() {
        let store = MemorySessionStore::new();
        let mut record = SessionRecord::new("expired", "2025-11-25", Duration::from_millis(1));
        store.create(&mut record).await.unwrap();

        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(store.load("expired").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn memory_store_cleanup_removes_expired() {
        let store = MemorySessionStore::new();

        let mut live = SessionRecord::new("live", "2025-11-25", Duration::from_secs(60));
        store.create(&mut live).await.unwrap();

        let mut dead = SessionRecord::new("dead", "2025-11-25", Duration::from_millis(1));
        store.create(&mut dead).await.unwrap();

        tokio::time::sleep(Duration::from_millis(10)).await;

        let removed = store.cleanup_expired().await;
        assert_eq!(removed, 1);
        assert_eq!(store.len().await, 1);
    }

    #[tokio::test]
    async fn memory_store_delete_is_idempotent() {
        let store = MemorySessionStore::new();
        store.delete("nonexistent").await.unwrap();
    }

    #[tokio::test]
    async fn record_touch_updates_timestamps() {
        let mut record = SessionRecord::new("t", "2025-11-25", Duration::from_secs(60));
        let original_expiry = record.expires_at;

        tokio::time::sleep(Duration::from_millis(10)).await;
        record.touch(Duration::from_secs(60));

        assert!(record.expires_at > original_expiry);
    }

    #[tokio::test]
    async fn dyn_session_store_object_safe() {
        // Compile-time check that SessionStore is object-safe.
        let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
        let mut record = sample_record("dyn");
        store.create(&mut record).await.unwrap();
        assert!(store.load(&record.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn caching_store_writes_to_both_tiers() {
        let cache = MemorySessionStore::new();
        let backend = MemorySessionStore::new();
        let store = CachingSessionStore::new(cache.clone(), backend.clone());

        let mut record = sample_record("cached");
        store.create(&mut record).await.unwrap();

        assert!(cache.load(&record.id).await.unwrap().is_some());
        assert!(backend.load(&record.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn caching_store_populates_cache_on_miss() {
        let cache = MemorySessionStore::new();
        let backend = MemorySessionStore::new();

        // Prime the backend directly; cache is empty.
        let mut record = sample_record("warm");
        backend.create(&mut record).await.unwrap();
        let id = record.id.clone();
        assert!(cache.load(&id).await.unwrap().is_none());

        // A load through the caching store should populate the cache.
        let store = CachingSessionStore::new(cache.clone(), backend);
        let loaded = store.load(&id).await.unwrap();
        assert!(loaded.is_some());
        assert!(cache.load(&id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn caching_store_delete_clears_both() {
        let cache = MemorySessionStore::new();
        let backend = MemorySessionStore::new();
        let store = CachingSessionStore::new(cache.clone(), backend.clone());

        let mut record = sample_record("gone");
        store.create(&mut record).await.unwrap();
        store.delete(&record.id).await.unwrap();

        assert!(cache.load(&record.id).await.unwrap().is_none());
        assert!(backend.load(&record.id).await.unwrap().is_none());
    }
}
