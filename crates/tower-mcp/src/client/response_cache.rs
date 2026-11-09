//! Client-side response caching for SEP-2549 cacheable results.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::protocol::CacheScope;

/// Default upper bound for a server-provided cache TTL (24 hours).
pub const DEFAULT_MAX_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Configuration for the MCP client's SEP-2549 response cache.
///
/// Caching is consulted only after the 2026-07-28 lifecycle has been selected.
/// A result with no `ttlMs` uses [`default_ttl`](Self::default_ttl), which is
/// zero by default, and is therefore immediately stale.
///
/// A result served from the cache reports the lifetime it has left in `ttlMs`,
/// after applying [`max_ttl`](Self::max_ttl), so caching it again cannot extend
/// that lifetime. A stale result served after a failed refresh reports zero. A
/// result whose server omitted `ttlMs` still omits it, even when
/// [`default_ttl`](Self::default_ttl) allowed it to be cached. A response that
/// came from the server carries the server's own hint unchanged.
#[derive(Debug, Clone)]
pub struct ClientCacheConfig {
    /// Enable the response cache. Default: `true`.
    pub enabled: bool,
    /// TTL used when a cacheable result omits `ttlMs`. Default: zero.
    pub default_ttl: Duration,
    /// Maximum server-provided or default TTL the client will honor.
    /// Default: 24 hours.
    pub max_ttl: Duration,
    /// Maximum number of distinct `resources/read` entries retained.
    ///
    /// List and discovery entries are exempt because their key space is
    /// bounded. Zero removes the resource-entry bound. Default: 512.
    pub max_resource_entries: usize,
    /// Return an expired entry when refreshing it fails. Default: `false`.
    pub serve_stale_on_error: bool,
    /// Opaque authorization-context partition for private entries.
    ///
    /// Use a stable principal identifier rather than an access token. Public
    /// entries remain reusable if this partition changes.
    pub partition: String,
}

impl ClientCacheConfig {
    /// Disable response caching while retaining the other defaults.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// Set the fallback TTL for results that omit `ttlMs`.
    pub fn with_default_ttl(mut self, ttl: Duration) -> Self {
        self.default_ttl = ttl;
        self
    }

    /// Set the upper bound applied to every cache TTL.
    pub fn with_max_ttl(mut self, ttl: Duration) -> Self {
        self.max_ttl = ttl;
        self
    }

    /// Set the maximum retained `resources/read` entries.
    pub fn with_max_resource_entries(mut self, max_entries: usize) -> Self {
        self.max_resource_entries = max_entries;
        self
    }

    /// Allow an expired entry to be returned when a refresh fails.
    pub fn with_serve_stale_on_error(mut self, enabled: bool) -> Self {
        self.serve_stale_on_error = enabled;
        self
    }

    /// Set the authorization-context partition for private entries.
    pub fn with_partition(mut self, partition: impl Into<String>) -> Self {
        self.partition = partition.into();
        self
    }
}

impl Default for ClientCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_ttl: Duration::ZERO,
            max_ttl: DEFAULT_MAX_CACHE_TTL,
            max_resource_entries: 512,
            serve_stale_on_error: false,
            partition: String::new(),
        }
    }
}

/// Every page of a paginated list, with cache hints valid for the whole result.
///
/// Returned by the `list_all_*_with_cache_hints` methods on
/// [`McpClient`](super::McpClient). Each page's expiry is fixed when that page
/// is received, and [`ttl_ms`](Self::ttl_ms) is the time left until the
/// earliest of them when pagination finishes, so slow pagination shortens it.
///
/// A missing hint means the aggregate cannot claim anything for that page:
///
/// | Pages | `ttl_ms` | `cache_scope` |
/// | --- | --- | --- |
/// | every page has the hint | earliest expiry, as time remaining | `Public` only if every page is public, otherwise `Private` |
/// | some pages omit the hint | `Some(0)` | `Private` |
/// | every page omits the hint | `None` | `None` |
///
/// Each page's `ttlMs` is capped at
/// [`ClientCacheConfig::max_ttl`], so a page from the server and the same page
/// from the cache contribute the same bound.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AggregateList<T> {
    /// Items from every page, in page order.
    pub items: Vec<T>,
    /// Milliseconds until the first page in the result expires.
    pub ttl_ms: Option<u64>,
    /// Scope that is safe for the whole result.
    pub cache_scope: Option<CacheScope>,
}

/// Combines per-page cache hints as pages arrive.
#[derive(Debug)]
pub(crate) struct AggregateHints {
    max_ttl: Duration,
    earliest_expiry: Option<Instant>,
    missing_ttl: bool,
    public: bool,
    private: bool,
    missing_scope: bool,
}

impl AggregateHints {
    pub(crate) fn new(max_ttl: Duration) -> Self {
        Self {
            max_ttl,
            earliest_expiry: None,
            missing_ttl: false,
            public: false,
            private: false,
            missing_scope: false,
        }
    }

    /// Record one page. Call as soon as the page is received.
    pub(crate) fn observe(&mut self, ttl_ms: Option<u64>, scope: Option<CacheScope>) {
        let received = Instant::now();
        // A hint too large for the clock is treated like a missing one.
        match ttl_ms.and_then(|ttl_ms| {
            received.checked_add(Duration::from_millis(ttl_ms).min(self.max_ttl))
        }) {
            Some(expiry) => {
                self.earliest_expiry = Some(
                    self.earliest_expiry
                        .map_or(expiry, |earliest| earliest.min(expiry)),
                );
            }
            None => self.missing_ttl = true,
        }
        match scope {
            Some(CacheScope::Public) => self.public = true,
            Some(_) => self.private = true,
            None => self.missing_scope = true,
        }
    }

    /// Build the result once pagination has finished.
    pub(crate) fn finish<T>(self, items: Vec<T>) -> AggregateList<T> {
        let ttl_ms = self.earliest_expiry.map(|expiry| {
            if self.missing_ttl {
                return 0;
            }
            let remaining = expiry.saturating_duration_since(Instant::now());
            u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX)
        });
        let cache_scope = if self.private || (self.public && self.missing_scope) {
            Some(CacheScope::Private)
        } else if self.public {
            Some(CacheScope::Public)
        } else {
            None
        };
        AggregateList {
            items,
            ttl_ms,
            cache_scope,
        }
    }
}

#[derive(Debug, Clone, Eq)]
struct CacheKey {
    method: String,
    params: String,
    partition: CachePartition,
}

impl PartialEq for CacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.method == other.method
            && self.params == other.params
            && self.partition == other.partition
    }
}

impl Hash for CacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.method.hash(state);
        self.params.hash(state);
        self.partition.hash(state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CachePartition {
    Public,
    Private(String),
}

#[derive(Debug, Clone)]
struct CacheEntry {
    value: serde_json::Value,
    expires_at: Instant,
    scope: CacheScope,
    /// The server sent `ttlMs`, so a hit reports the remaining lifetime.
    has_server_ttl: bool,
}

#[derive(Debug, Clone)]
pub(crate) enum CacheLookup {
    Fresh(serde_json::Value),
    Stale(serde_json::Value),
    Miss,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LogicalKey {
    method: String,
    params: Option<String>,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<CacheKey, CacheEntry>,
    resource_order: VecDeque<CacheKey>,
    generations: HashMap<LogicalKey, GenerationSlot>,
    partition: String,
}

#[derive(Debug, Default)]
struct GenerationSlot {
    generation: u64,
    in_flight: usize,
}

#[derive(Debug)]
pub(crate) struct ClientResponseCache {
    config: ClientCacheConfig,
    state: Mutex<CacheState>,
}

impl ClientResponseCache {
    pub(crate) fn new(config: ClientCacheConfig) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(CacheState {
                partition: config.partition.clone(),
                ..CacheState::default()
            }),
            config,
        })
    }

    pub(crate) fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub(crate) fn serve_stale_on_error(&self) -> bool {
        self.config.serve_stale_on_error
    }

    pub(crate) fn aggregate_hints(&self) -> AggregateHints {
        AggregateHints::new(self.config.max_ttl)
    }

    pub(crate) async fn set_partition(&self, partition: String) {
        let mut state = self.state.lock().await;
        if state.partition == partition {
            return;
        }
        for slot in state.generations.values_mut() {
            slot.generation = slot.generation.wrapping_add(1);
        }
        state
            .entries
            .retain(|key, _| key.partition == CachePartition::Public);
        state
            .resource_order
            .retain(|key| key.partition == CachePartition::Public);
        state.partition = partition;
    }

    pub(crate) async fn clear(&self) {
        let mut state = self.state.lock().await;
        for slot in state.generations.values_mut() {
            slot.generation = slot.generation.wrapping_add(1);
        }
        state.entries.clear();
        state.resource_order.clear();
    }

    pub(crate) async fn len(&self) -> usize {
        self.state.lock().await.entries.len()
    }

    pub(crate) async fn capture_generation(&self, method: &str, params: &str) -> u64 {
        let mut state = self.state.lock().await;
        let key = generation_key(method, params);
        let slot = state.generations.entry(key).or_default();
        slot.in_flight = slot.in_flight.saturating_add(1);
        slot.generation
    }

    pub(crate) async fn release_generation(&self, method: &str, params: &str) {
        let mut state = self.state.lock().await;
        finish_generation(&mut state, &generation_key(method, params), None);
    }

    pub(crate) async fn lookup(&self, method: &str, params: &str) -> CacheLookup {
        if !self.config.enabled {
            return CacheLookup::Miss;
        }

        let state = self.state.lock().await;
        let private_key = CacheKey {
            method: method.to_string(),
            params: params.to_string(),
            partition: CachePartition::Private(state.partition.clone()),
        };
        if let Some(entry) = state.entries.get(&private_key) {
            return classify_entry(entry);
        }

        let public_key = CacheKey {
            method: method.to_string(),
            params: params.to_string(),
            partition: CachePartition::Public,
        };
        match state.entries.get(&public_key) {
            Some(entry) if entry.scope == CacheScope::Public => classify_entry(entry),
            _ => CacheLookup::Miss,
        }
    }

    pub(crate) async fn write(
        &self,
        method: &str,
        params: &str,
        captured_generation: u64,
        value: serde_json::Value,
        ttl_ms: Option<u64>,
        scope: Option<CacheScope>,
    ) {
        if !self.config.enabled {
            return;
        }

        let mut state = self.state.lock().await;
        let logical = generation_key(method, params);
        if !finish_generation(&mut state, &logical, Some(captured_generation)) {
            tracing::debug!(
                method,
                "Skipping cache write invalidated while the request was in flight"
            );
            return;
        }

        remove_logical_entries(&mut state, method, params);

        let ttl = Duration::from_millis(ttl_ms.unwrap_or_else(|| {
            u64::try_from(self.config.default_ttl.as_millis()).unwrap_or(u64::MAX)
        }))
        .min(self.config.max_ttl);
        if ttl.is_zero() {
            return;
        }

        let scope = scope.unwrap_or(CacheScope::Private);
        let partition = match scope {
            CacheScope::Public => CachePartition::Public,
            CacheScope::Private => CachePartition::Private(state.partition.clone()),
            _ => CachePartition::Private(state.partition.clone()),
        };
        let key = CacheKey {
            method: method.to_string(),
            params: params.to_string(),
            partition,
        };
        let now = Instant::now();
        let Some(expires_at) = now.checked_add(ttl) else {
            tracing::warn!(
                method,
                "Skipping response-cache TTL that exceeds clock range"
            );
            return;
        };
        state.entries.insert(
            key.clone(),
            CacheEntry {
                value,
                expires_at,
                scope,
                has_server_ttl: ttl_ms.is_some(),
            },
        );

        if method == "resources/read" {
            state.resource_order.retain(|existing| existing != &key);
            state.resource_order.push_back(key);
            enforce_resource_bound(&mut state, self.config.max_resource_entries);
        }
    }

    pub(crate) async fn evict_method(&self, method: &str) {
        let mut state = self.state.lock().await;
        let logical = generation_key(method, "");
        if let Some(slot) = state.generations.get_mut(&logical) {
            slot.generation = slot.generation.wrapping_add(1);
        }
        state.entries.retain(|key, _| key.method != method);
        state.resource_order.retain(|key| key.method != method);
    }

    pub(crate) async fn evict_resource(&self, uri: &str) {
        let mut state = self.state.lock().await;
        let logical = generation_key("resources/read", uri);
        if let Some(slot) = state.generations.get_mut(&logical) {
            slot.generation = slot.generation.wrapping_add(1);
        }
        remove_logical_entries(&mut state, "resources/read", uri);
    }
}

fn finish_generation(state: &mut CacheState, key: &LogicalKey, captured: Option<u64>) -> bool {
    let Some(slot) = state.generations.get_mut(key) else {
        return false;
    };
    let valid = captured.is_none_or(|captured| slot.generation == captured);
    slot.in_flight = slot.in_flight.saturating_sub(1);
    if slot.in_flight == 0 {
        state.generations.remove(key);
    }
    valid
}

fn generation_key(method: &str, params: &str) -> LogicalKey {
    let params = (method == "resources/read").then(|| params.to_string());
    LogicalKey {
        method: method.to_string(),
        params,
    }
}

fn classify_entry(entry: &CacheEntry) -> CacheLookup {
    let remaining = entry.expires_at.saturating_duration_since(Instant::now());
    let mut value = entry.value.clone();
    // Rewrites the clone, so the stored source result is never compounded.
    if entry.has_server_ttl
        && let Some(fields) = value.as_object_mut()
    {
        let remaining_ms = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX);
        fields.insert("ttlMs".to_string(), remaining_ms.into());
    }
    if remaining.is_zero() {
        CacheLookup::Stale(value)
    } else {
        CacheLookup::Fresh(value)
    }
}

fn remove_logical_entries(state: &mut CacheState, method: &str, params: &str) {
    state
        .entries
        .retain(|key, _| key.method != method || key.params != params);
    state
        .resource_order
        .retain(|key| key.method != method || key.params != params);
}

fn enforce_resource_bound(state: &mut CacheState, max_entries: usize) {
    if max_entries == 0 {
        return;
    }
    while state.resource_order.len() > max_entries {
        if let Some(oldest) = state.resource_order.pop_front() {
            state.entries.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(id: u64) -> serde_json::Value {
        serde_json::json!({ "id": id })
    }

    /// Fail a paused-clock test that stops making progress.
    async fn bounded<F: std::future::Future>(future: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(7 * 24 * 60 * 60), future)
            .await
            .expect("test exceeded its time bound")
    }

    fn fresh_ttl(lookup: CacheLookup) -> Option<u64> {
        match lookup {
            CacheLookup::Fresh(value) => value.get("ttlMs").and_then(serde_json::Value::as_u64),
            other => panic!("expected a fresh entry, got {other:?}"),
        }
    }

    async fn write_tools(
        cache: &ClientResponseCache,
        ttl_ms: Option<u64>,
        scope: Option<CacheScope>,
    ) {
        let generation = cache.capture_generation("tools/list", "").await;
        let mut result = value(1);
        if let Some(ttl_ms) = ttl_ms {
            result["ttlMs"] = ttl_ms.into();
        }
        cache
            .write("tools/list", "", generation, result, ttl_ms, scope)
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn hit_rewrites_only_the_returned_copy() {
        bounded(async {
            let cache = ClientResponseCache::new(
                ClientCacheConfig::default().with_max_ttl(Duration::from_secs(10)),
            );
            write_tools(&cache, Some(60_000), Some(CacheScope::Public)).await;

            for expected in [10_000, 7_000, 4_000] {
                assert_eq!(
                    fresh_ttl(cache.lookup("tools/list", "").await),
                    Some(expected)
                );
                tokio::time::advance(Duration::from_secs(3)).await;
            }

            let state = cache.state.lock().await;
            let stored = state.entries.values().next().unwrap();
            assert_eq!(stored.value["ttlMs"], 60_000);
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn stale_entry_saturates_at_zero() {
        bounded(async {
            let cache = ClientResponseCache::new(ClientCacheConfig::default());
            write_tools(&cache, Some(1_000), Some(CacheScope::Public)).await;

            tokio::time::advance(Duration::from_secs(3_600)).await;
            match cache.lookup("tools/list", "").await {
                CacheLookup::Stale(value) => assert_eq!(value["ttlMs"], 0),
                other => panic!("expected a stale entry, got {other:?}"),
            }
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn missing_server_ttl_is_not_invented() {
        bounded(async {
            let cache = ClientResponseCache::new(
                ClientCacheConfig::default().with_default_ttl(Duration::from_secs(30)),
            );
            write_tools(&cache, None, Some(CacheScope::Public)).await;

            assert_eq!(fresh_ttl(cache.lookup("tools/list", "").await), None);
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn remaining_ttl_holds_across_partition_rotation_and_eviction() {
        bounded(async {
            let cache = ClientResponseCache::new(
                ClientCacheConfig::default().with_partition("principal-a"),
            );
            write_tools(&cache, Some(60_000), Some(CacheScope::Public)).await;

            tokio::time::advance(Duration::from_secs(20)).await;
            cache.set_partition("principal-b".to_string()).await;
            assert_eq!(
                fresh_ttl(cache.lookup("tools/list", "").await),
                Some(40_000)
            );

            cache.evict_method("tools/list").await;
            assert!(matches!(
                cache.lookup("tools/list", "").await,
                CacheLookup::Miss
            ));
        })
        .await;
    }

    #[tokio::test]
    async fn zero_ttl_is_not_cached() {
        let cache = ClientResponseCache::new(ClientCacheConfig::default());
        let generation = cache.capture_generation("tools/list", "").await;
        cache
            .write(
                "tools/list",
                "",
                generation,
                value(1),
                Some(0),
                Some(CacheScope::Public),
            )
            .await;
        assert!(matches!(
            cache.lookup("tools/list", "").await,
            CacheLookup::Miss
        ));
    }

    #[tokio::test]
    async fn max_ttl_caps_server_ttl_and_retains_stale_value() {
        let cache = ClientResponseCache::new(
            ClientCacheConfig::default().with_max_ttl(Duration::from_millis(1)),
        );
        let generation = cache.capture_generation("tools/list", "").await;
        cache
            .write(
                "tools/list",
                "",
                generation,
                value(1),
                Some(60_000),
                Some(CacheScope::Public),
            )
            .await;
        assert!(matches!(
            cache.lookup("tools/list", "").await,
            CacheLookup::Fresh(_)
        ));

        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(matches!(
            cache.lookup("tools/list", "").await,
            CacheLookup::Stale(_)
        ));
    }

    #[tokio::test]
    async fn completed_requests_release_generation_slots() {
        let cache = ClientResponseCache::new(ClientCacheConfig::default());

        let generation = cache.capture_generation("tools/list", "").await;
        cache
            .write(
                "tools/list",
                "",
                generation,
                value(1),
                Some(60_000),
                Some(CacheScope::Public),
            )
            .await;
        assert!(cache.state.lock().await.generations.is_empty());

        cache
            .capture_generation("resources/read", "resource://a")
            .await;
        cache
            .release_generation("resources/read", "resource://a")
            .await;
        assert!(cache.state.lock().await.generations.is_empty());
    }

    #[tokio::test]
    async fn private_entries_follow_partition_but_public_entries_do_not() {
        let cache =
            ClientResponseCache::new(ClientCacheConfig::default().with_partition("principal-a"));
        let generation = cache
            .capture_generation("resources/read", "config://a")
            .await;
        cache
            .write(
                "resources/read",
                "config://a",
                generation,
                value(1),
                Some(60_000),
                Some(CacheScope::Private),
            )
            .await;
        cache.set_partition("principal-b".to_string()).await;
        assert!(matches!(
            cache.lookup("resources/read", "config://a").await,
            CacheLookup::Miss
        ));

        let generation = cache.capture_generation("tools/list", "").await;
        cache
            .write(
                "tools/list",
                "",
                generation,
                value(2),
                Some(60_000),
                Some(CacheScope::Public),
            )
            .await;
        cache.set_partition("principal-c".to_string()).await;
        assert!(matches!(
            cache.lookup("tools/list", "").await,
            CacheLookup::Fresh(_)
        ));
    }

    #[tokio::test]
    async fn invalidation_wins_over_an_in_flight_write() {
        let cache = ClientResponseCache::new(ClientCacheConfig::default());
        let generation = cache.capture_generation("tools/list", "").await;
        cache.evict_method("tools/list").await;
        cache
            .write(
                "tools/list",
                "",
                generation,
                value(1),
                Some(60_000),
                Some(CacheScope::Public),
            )
            .await;
        assert!(matches!(
            cache.lookup("tools/list", "").await,
            CacheLookup::Miss
        ));
    }

    #[tokio::test]
    async fn clear_and_partition_rotation_suppress_in_flight_writes() {
        let cache =
            ClientResponseCache::new(ClientCacheConfig::default().with_partition("principal-a"));

        let clear_generation = cache.capture_generation("tools/list", "").await;
        cache.clear().await;
        cache
            .write(
                "tools/list",
                "",
                clear_generation,
                value(1),
                Some(60_000),
                Some(CacheScope::Public),
            )
            .await;
        assert!(matches!(
            cache.lookup("tools/list", "").await,
            CacheLookup::Miss
        ));

        let rotate_generation = cache
            .capture_generation("resources/read", "config://a")
            .await;
        cache.set_partition("principal-b".to_string()).await;
        cache
            .write(
                "resources/read",
                "config://a",
                rotate_generation,
                value(2),
                Some(60_000),
                Some(CacheScope::Private),
            )
            .await;
        assert!(matches!(
            cache.lookup("resources/read", "config://a").await,
            CacheLookup::Miss
        ));
    }

    #[tokio::test]
    async fn resource_cache_is_bounded_without_evicting_lists() {
        let cache =
            ClientResponseCache::new(ClientCacheConfig::default().with_max_resource_entries(2));
        let generation = cache.capture_generation("tools/list", "").await;
        cache
            .write(
                "tools/list",
                "",
                generation,
                value(0),
                Some(60_000),
                Some(CacheScope::Public),
            )
            .await;
        for index in 0..3 {
            let uri = format!("resource://{index}");
            let generation = cache.capture_generation("resources/read", &uri).await;
            cache
                .write(
                    "resources/read",
                    &uri,
                    generation,
                    value(index),
                    Some(60_000),
                    Some(CacheScope::Private),
                )
                .await;
        }
        assert_eq!(cache.len().await, 3);
        assert!(matches!(
            cache.lookup("resources/read", "resource://0").await,
            CacheLookup::Miss
        ));
        assert!(matches!(
            cache.lookup("tools/list", "").await,
            CacheLookup::Fresh(_)
        ));
    }
}
