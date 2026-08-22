//! Session management for [`HttpTransport`](super::HttpTransport).
//!
//! The live [`Session`] runtime state, its [`SessionRegistry`], the public
//! [`SessionConfig`], and the [`SessionHandle`] / [`SessionInfo`] pair used to
//! inspect and manage sessions from outside the transport.
//!
//! Split out of `http.rs` in #1256 (phase 3). An `impl` block in a child
//! module, so none of these types' paths changed -- `http.rs` re-exports the
//! public ones (`SessionConfig`, `SessionInfo`, `SessionHandle`,
//! `DEFAULT_SESSION_TTL`) at their existing locations.

use super::*;

/// Pending request waiting for a response from the client
struct PendingRequest {
    response_tx: oneshot::Sender<Result<serde_json::Value>>,
}

/// Session state for HTTP transport
/// How a session produces its MCP service for request processing.
pub(super) enum SessionServiceSource {
    /// Session was created from an McpRouter with a factory for middleware wrapping.
    Router {
        router: McpRouter,
        factory: ServiceFactory,
    },
    /// Session was created from a pre-built boxed service (e.g., McpProxy).
    /// Wrapped in Mutex because BoxCloneService is Send but not Sync,
    /// and Session must be Sync for Arc<Session> to be Send.
    Boxed(std::sync::Mutex<McpBoxService>),
}

pub(super) struct Session {
    /// Session ID
    pub(super) id: String,
    /// Source for creating the MCP service
    pub(super) service_source: SessionServiceSource,
    /// Broadcast channel for SSE notifications and outgoing requests
    pub(super) notifications_tx: broadcast::Sender<String>,
    /// When this session was created
    created_at: Instant,
    /// Last time this session was accessed
    last_accessed: RwLock<Instant>,
    /// Pending outgoing requests waiting for responses
    pending_requests: Mutex<HashMap<RequestId, PendingRequest>>,
    /// Session-wide allocator for request-scoped server-to-client request IDs.
    ///
    /// Each originating POST owns a separate channel, but IDs must remain
    /// unique across concurrent POSTs in the same session.
    pub(super) request_id_allocator: Option<Arc<AtomicI64>>,
    /// Negotiated protocol version (set after initialize)
    pub(super) protocol_version: RwLock<String>,
    /// Client implementation info advertised in the `initialize` request.
    ///
    /// Populated by `handle_post` after a successful initialize response,
    /// and restored from a [`SessionRecord`](crate::session_store::SessionRecord)
    /// when a session is rebuilt from the persistent store. `None` until the
    /// first initialize completes.
    pub(super) client_info: RwLock<Option<Implementation>>,
    /// Client capabilities advertised in the `initialize` request.
    ///
    /// Populated by `handle_post` after a successful initialize response,
    /// and restored from a [`SessionRecord`](crate::session_store::SessionRecord)
    /// when a session is rebuilt from the persistent store. `None` until the
    /// first initialize completes.
    pub(super) client_capabilities: RwLock<Option<ClientCapabilities>>,
    /// Counter for SSE event IDs (for stream resumption per SEP-1699)
    event_counter: AtomicU64,
    /// Pluggable store for SSE events (enables cross-instance replay)
    event_store: Arc<dyn crate::event_store::EventStore>,
    /// Whether `notifications/initialized` has been received from the client.
    ///
    /// Per the MCP 2025-11-25 spec, clients MUST send this notification after
    /// receiving the `initialize` response and before sending any other requests.
    /// Checked by `handle_post` when `strict_initialization` is enabled on
    /// [`SessionConfig`]. Pre-initialized sessions (optional_sessions path) and
    /// restored sessions start with this set to `true`.
    pub(super) initialized_notification_received: std::sync::atomic::AtomicBool,
}

impl Session {
    pub(super) fn new(
        router: McpRouter,
        sampling_enabled: bool,
        service_factory: ServiceFactory,
        event_store: Arc<dyn crate::event_store::EventStore>,
    ) -> Self {
        let (notifications_tx, _) = broadcast::channel(100);

        // Set up notification forwarding: mpsc -> broadcast
        // The router sends notifications (progress, log, resource updates) to
        // an mpsc channel. We bridge these to the session's broadcast channel
        // so they reach connected SSE clients.
        let (notif_sender, mut notif_receiver) = notification_channel(256);
        let router = router.with_notification_sender(notif_sender);

        let broadcast_tx = notifications_tx.clone();
        tokio::spawn(async move {
            while let Some(notification) = notif_receiver.recv().await {
                if let Some(json) = crate::transport::stdio::serialize_notification(&notification) {
                    // Best effort: if no subscribers, the message is dropped
                    let _ = broadcast_tx.send(json);
                }
            }
        });

        let request_id_allocator = if sampling_enabled {
            Some(Arc::new(AtomicI64::new(1)))
        } else {
            None
        };

        let now = Instant::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            service_source: SessionServiceSource::Router {
                router,
                factory: service_factory,
            },
            notifications_tx,
            created_at: now,
            last_accessed: RwLock::new(now),
            pending_requests: Mutex::new(HashMap::new()),
            request_id_allocator,
            protocol_version: RwLock::new(LATEST_PROTOCOL_VERSION.to_string()),
            client_info: RwLock::new(None),
            client_capabilities: RwLock::new(None),
            event_counter: AtomicU64::new(0),
            event_store,
            initialized_notification_received: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Create a session from a pre-built boxed service.
    ///
    /// This is used when the transport is created via [`HttpTransport::from_service()`].
    /// Notification bridging and sampling setup are skipped — the caller is
    /// responsible for configuring these on the service before passing it in.
    fn from_service(
        service: McpBoxService,
        event_store: Arc<dyn crate::event_store::EventStore>,
    ) -> Self {
        let (notifications_tx, _) = broadcast::channel(100);

        let now = Instant::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            service_source: SessionServiceSource::Boxed(std::sync::Mutex::new(service)),
            notifications_tx,
            created_at: now,
            last_accessed: RwLock::new(now),
            pending_requests: Mutex::new(HashMap::new()),
            request_id_allocator: None,
            protocol_version: RwLock::new(LATEST_PROTOCOL_VERSION.to_string()),
            client_info: RwLock::new(None),
            client_capabilities: RwLock::new(None),
            event_counter: AtomicU64::new(0),
            event_store,
            initialized_notification_received: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Rebuild a session from a [`SessionRecord`] so a request for an
    /// unknown session ID can be served transparently.
    ///
    /// The router is pre-marked initialized and the protocol version is
    /// restored from the record. Runtime state (broadcast channels,
    /// pending-request table, and legacy resource subscription memberships) is
    /// freshly allocated — in-flight state from before the rebuild is not
    /// recovered. Clients must resubscribe to resources after restoration. The
    /// `event_counter` is left at zero; the [`SessionRegistry`] seeds it from
    /// the event store so future event IDs don't collide with buffered ones.
    fn restored(
        record: &crate::session_store::SessionRecord,
        router: McpRouter,
        sampling_enabled: bool,
        service_factory: ServiceFactory,
        event_store: Arc<dyn crate::event_store::EventStore>,
    ) -> Self {
        // Skip the Initializing intermediate state — this session was
        // already initialized on the original instance.
        router.session().mark_preinitialized();

        let (notifications_tx, _) = broadcast::channel(100);
        let (notif_sender, mut notif_receiver) = notification_channel(256);
        let router = router.with_notification_sender(notif_sender);

        let broadcast_tx = notifications_tx.clone();
        tokio::spawn(async move {
            while let Some(notification) = notif_receiver.recv().await {
                if let Some(json) = crate::transport::stdio::serialize_notification(&notification) {
                    let _ = broadcast_tx.send(json);
                }
            }
        });

        let request_id_allocator = if sampling_enabled {
            Some(Arc::new(AtomicI64::new(1)))
        } else {
            None
        };

        let now = Instant::now();
        Self {
            id: record.id.clone(),
            service_source: SessionServiceSource::Router {
                router,
                factory: service_factory,
            },
            notifications_tx,
            created_at: now,
            last_accessed: RwLock::new(now),
            pending_requests: Mutex::new(HashMap::new()),
            request_id_allocator,
            protocol_version: RwLock::new(record.protocol_version.clone()),
            client_info: RwLock::new(record.client_info.clone()),
            client_capabilities: RwLock::new(record.client_capabilities.clone()),
            event_counter: AtomicU64::new(0),
            event_store,
            // Restored sessions already completed the handshake on a previous
            // instance; treat `notifications/initialized` as already received.
            initialized_notification_received: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Rebuild a session from a [`SessionRecord`] for transports built
    /// with [`HttpTransport::from_service`]. The service's internal state
    /// (if any) is not restored — the caller is responsible for anything
    /// beyond the metadata in the record.
    fn from_service_restored(
        service: McpBoxService,
        record: &crate::session_store::SessionRecord,
        event_store: Arc<dyn crate::event_store::EventStore>,
    ) -> Self {
        let (notifications_tx, _) = broadcast::channel(100);
        let now = Instant::now();
        Self {
            id: record.id.clone(),
            service_source: SessionServiceSource::Boxed(std::sync::Mutex::new(service)),
            notifications_tx,
            created_at: now,
            last_accessed: RwLock::new(now),
            pending_requests: Mutex::new(HashMap::new()),
            request_id_allocator: None,
            protocol_version: RwLock::new(record.protocol_version.clone()),
            client_info: RwLock::new(record.client_info.clone()),
            client_capabilities: RwLock::new(record.client_capabilities.clone()),
            event_counter: AtomicU64::new(0),
            event_store,
            // Restored sessions already completed the handshake on a previous
            // instance; treat `notifications/initialized` as already received.
            initialized_notification_received: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Create a middleware-wrapped service from this session's service source.
    pub(super) fn make_service(&self) -> McpBoxService {
        match &self.service_source {
            SessionServiceSource::Router { router, factory } => (factory)(router.clone()),
            SessionServiceSource::Boxed(mutex) => mutex.lock().unwrap().clone(),
        }
    }

    /// Handle a client notification (fire-and-forget, no response).
    ///
    /// For router-based sessions, delegates to the router's notification handler.
    /// For service-based sessions, notifications are logged but not processed
    /// (the service should handle its own notification needs).
    pub(super) fn handle_notification(&self, notification: McpNotification) {
        match &self.service_source {
            SessionServiceSource::Router { router, .. } => {
                router.handle_notification(notification);
            }
            SessionServiceSource::Boxed(_) => {
                tracing::debug!(
                    notification = ?notification,
                    "Notification received on service-based session (not forwarded)"
                );
            }
        }
    }

    /// Whether an externally supplied notification belongs on this session's
    /// legacy SSE stream.
    ///
    /// Router-backed resource updates honor the exact URI memberships created
    /// by `resources/subscribe`. A boxed service exposes no router state for
    /// the transport to inspect, so it retains the caller-owned broadcast
    /// behavior that [`HttpTransport::from_service`] has always provided.
    fn accepts_external_notification(&self, notification: &ServerNotification) -> bool {
        match (notification, &self.service_source) {
            (
                ServerNotification::ResourceUpdated { uri },
                SessionServiceSource::Router { router, .. },
            ) => router.is_subscribed(uri),
            _ => true,
        }
    }

    /// Get the next SSE event ID for this session.
    ///
    /// Event IDs are monotonically increasing per session, enabling
    /// stream resumption via the Last-Event-ID header (SEP-1699).
    pub(super) fn next_event_id(&self) -> u64 {
        self.event_counter.fetch_add(1, Ordering::SeqCst)
    }

    /// Buffer an event for potential replay (SEP-1699).
    ///
    /// Delegates to the configured [`EventStore`](crate::event_store::EventStore).
    /// Store errors are logged but non-fatal — the transport continues
    /// serving the client even if the external event buffer is unavailable,
    /// since the event has already been sent on the live SSE stream.
    pub(super) async fn buffer_event(&self, id: u64, data: String) {
        let record = crate::event_store::EventRecord::new(id, data);
        if let Err(e) = self.event_store.append(&self.id, record).await {
            tracing::warn!(session_id = %self.id, event_id = id, error = %e, "Failed to append event to event store");
        }
    }

    /// Get buffered events after the given event ID.
    ///
    /// Returns events with IDs greater than `after_id`, in order. Used for
    /// stream resumption when a client reconnects with the `Last-Event-ID`
    /// header. Store errors produce an empty replay list and are logged.
    pub(super) async fn get_events_after(
        &self,
        after_id: u64,
    ) -> Vec<crate::event_store::EventRecord> {
        match self.event_store.replay_after(&self.id, after_id).await {
            Ok(events) => events,
            Err(e) => {
                tracing::warn!(session_id = %self.id, error = %e, "Failed to replay events from event store");
                Vec::new()
            }
        }
    }

    /// Update the last accessed time
    async fn touch(&self) {
        *self.last_accessed.write().await = Instant::now();
    }

    /// Check if the session has expired
    async fn is_expired(&self, ttl: Duration) -> bool {
        self.last_accessed.read().await.elapsed() > ttl
    }

    /// Store a pending request
    pub(super) async fn add_pending_request(
        &self,
        id: RequestId,
        response_tx: oneshot::Sender<Result<serde_json::Value>>,
    ) {
        let mut pending = self.pending_requests.lock().await;
        pending.insert(id, PendingRequest { response_tx });
    }

    /// Complete a pending request with a response
    pub(super) async fn complete_pending_request(
        &self,
        id: &RequestId,
        result: Result<serde_json::Value>,
    ) -> bool {
        let pending = {
            let mut pending_requests = self.pending_requests.lock().await;
            pending_requests.remove(id)
        };

        match pending {
            Some(pending) => {
                // Send result to waiter (ignore if they've dropped the receiver)
                let _ = pending.response_tx.send(result);
                true
            }
            None => false,
        }
    }

    /// Fail request-scoped client requests whose originating POST is gone.
    pub(super) async fn fail_pending_requests(&self, ids: &[RequestId], message: &str) {
        let removed = {
            let mut pending = self.pending_requests.lock().await;
            ids.iter()
                .filter_map(|id| pending.remove(id))
                .collect::<Vec<_>>()
        };

        for pending in removed {
            let _ = pending
                .response_tx
                .send(Err(Error::Transport(message.to_string())));
        }
    }
}

/// Default session TTL (30 minutes)
pub const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(30 * 60);

/// Default cleanup interval (1 minute)
const DEFAULT_CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

/// Configuration for session management
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Time-to-live for inactive sessions
    pub ttl: Duration,
    /// Maximum number of sessions (None = unlimited)
    pub max_sessions: Option<usize>,
    /// How often to run the cleanup task
    pub cleanup_interval: Duration,
    /// Whether to enforce that clients send `notifications/initialized` before
    /// making any non-initialize requests, per the MCP 2025-11-25 spec.
    ///
    /// When `true` (the default), the transport returns a JSON-RPC
    /// `InvalidRequest` error (-32600) to any request received before
    /// `notifications/initialized` on a 2025-11-25 session-based connection.
    ///
    /// Set to `false` to restore the previous lenient behavior, e.g. in
    /// dev/test scenarios where the full MCP handshake is inconvenient.
    pub strict_initialization: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            ttl: DEFAULT_SESSION_TTL,
            max_sessions: None,
            cleanup_interval: DEFAULT_CLEANUP_INTERVAL,
            strict_initialization: true,
        }
    }
}

impl SessionConfig {
    /// Create a new session config with the given TTL
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            ttl,
            ..Default::default()
        }
    }

    /// Set the maximum number of sessions
    pub fn max_sessions(mut self, max: usize) -> Self {
        self.max_sessions = Some(max);
        self
    }

    /// Set the cleanup interval
    pub fn cleanup_interval(mut self, interval: Duration) -> Self {
        self.cleanup_interval = interval;
        self
    }

    /// Enable or disable strict initialization enforcement.
    ///
    /// When enabled (default), the transport enforces that clients send
    /// `notifications/initialized` before any other requests on a
    /// 2025-11-25 session-based connection, per the MCP spec. Requests
    /// that arrive before this notification receive a JSON-RPC
    /// `InvalidRequest` error (-32600).
    ///
    /// Disable this for dev/test scenarios where the full MCP handshake
    /// is inconvenient.
    pub fn strict_initialization(mut self, enabled: bool) -> Self {
        self.strict_initialization = enabled;
        self
    }
}

/// Registry coordinating live session runtime state with a pluggable
/// persistent [`SessionStore`](crate::session_store::SessionStore).
///
/// - Runtime state (broadcast channels, pending requests, live services) is
///   kept in the in-process `sessions` map and cannot be serialized.
/// - Persistent metadata (IDs, timestamps, protocol version) is mirrored into
///   the caller-supplied [`SessionStore`]. The default
///   [`MemorySessionStore`](crate::session_store::MemorySessionStore) keeps
///   metadata in-process (same behavior as before this trait existed).
pub(super) struct SessionRegistry {
    pub(super) sessions: RwLock<HashMap<String, Arc<Session>>>,
    config: SessionConfig,
    sampling_enabled: bool,
    persistent: Arc<dyn crate::session_store::SessionStore>,
    events: Arc<dyn crate::event_store::EventStore>,
    /// Source for rebuilding services when restoring a session.
    service_source: ServiceSource,
    /// If `true`, a request for an unknown session ID whose record is not
    /// in the persistent store spins up a new session with synthetic
    /// client info instead of returning 404 (see anubis-mcp #125 for the
    /// precedent).
    auto_reinit: bool,
}

impl SessionRegistry {
    pub(super) fn new(
        config: SessionConfig,
        sampling_enabled: bool,
        persistent: Arc<dyn crate::session_store::SessionStore>,
        events: Arc<dyn crate::event_store::EventStore>,
        service_source: ServiceSource,
        auto_reinit: bool,
    ) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            config,
            sampling_enabled,
            persistent,
            events,
            service_source,
            auto_reinit,
        }
    }

    /// Build a SessionRecord reflecting the given live Session.
    async fn record_for(&self, session: &Session) -> crate::session_store::SessionRecord {
        let protocol_version = session.protocol_version.read().await.clone();
        let last_accessed = session.last_accessed.read().await;
        let mut record = crate::session_store::SessionRecord::new(
            session.id.clone(),
            protocol_version,
            self.config.ttl,
        );
        // Populate the client identity / capabilities advertised at
        // initialize time so persisted records faithfully describe the
        // session. These remain `None` until a successful initialize.
        record.client_info = session.client_info.read().await.clone();
        record.client_capabilities = session.client_capabilities.read().await.clone();
        // Convert from monotonic Instant to SystemTime approximation.
        let now = std::time::SystemTime::now();
        let created_ago = session.created_at.elapsed();
        let last_accessed_ago = last_accessed.elapsed();
        record.created_at = now.checked_sub(created_ago).unwrap_or(now);
        record.last_accessed = now.checked_sub(last_accessed_ago).unwrap_or(now);
        record.expires_at = record.last_accessed + self.config.ttl;
        record
    }

    /// Persist metadata for a newly created session, logging on failure.
    ///
    /// Persistence errors are intentionally non-fatal: the live runtime
    /// session is already registered locally, so the transport can continue
    /// serving requests even if the external store is briefly unavailable.
    async fn persist_new(&self, session: &Session) {
        let record = self.record_for(session).await;
        if let Err(e) = self.persistent.create(&mut record.clone()).await {
            tracing::warn!(session_id = %session.id, error = %e, "Failed to persist session record");
        }
    }

    /// Persist an update to an existing session's record (upsert).
    ///
    /// Called after live session activity to maintain the store's sliding
    /// expiry, and after other state changes that should be reflected in the
    /// persistent store -- notably a successful `initialize`, so the stored
    /// record carries the client's advertised `client_info` and capabilities.
    /// Failures are logged but non-fatal.
    pub(super) async fn save_record(&self, session: &Session) {
        let record = self.record_for(session).await;
        if let Err(e) = self.persistent.save(&record).await {
            tracing::warn!(session_id = %session.id, error = %e, "Failed to save session record");
        }
    }

    /// Create a session for an incoming `initialize` request.
    ///
    /// Only reached from the `is_init` branch of the POST handler, so the
    /// handshake is recorded here, before the request is dispatched. A client
    /// whose `initialized` notification overtakes that dispatch (#458) then
    /// still completes the handshake, while one that never sent `initialize`
    /// has no session for the notification to open.
    pub(super) async fn create(
        &self,
        router: McpRouter,
        service_factory: ServiceFactory,
    ) -> Option<Arc<Session>> {
        router.session().mark_handshake_started();

        let session = {
            let mut sessions = self.sessions.write().await;

            // Check max sessions limit
            if let Some(max) = self.config.max_sessions
                && sessions.len() >= max
            {
                tracing::warn!(
                    max_sessions = max,
                    current = sessions.len(),
                    "Session limit reached, rejecting new session"
                );
                return None;
            }

            let session = Arc::new(Session::new(
                router,
                self.sampling_enabled,
                service_factory,
                self.events.clone(),
            ));
            sessions.insert(session.id.clone(), session.clone());
            tracing::debug!(session_id = %session.id, sampling = self.sampling_enabled, "Created new session");
            session
        };
        self.persist_new(&session).await;
        Some(session)
    }

    pub(super) async fn create_from_service(&self, service: McpBoxService) -> Option<Arc<Session>> {
        let session = {
            let mut sessions = self.sessions.write().await;

            if let Some(max) = self.config.max_sessions
                && sessions.len() >= max
            {
                tracing::warn!(
                    max_sessions = max,
                    current = sessions.len(),
                    "Session limit reached, rejecting new session"
                );
                return None;
            }

            let session = Arc::new(Session::from_service(service, self.events.clone()));
            sessions.insert(session.id.clone(), session.clone());
            tracing::debug!(session_id = %session.id, "Created new session from service");
            session
        };
        self.persist_new(&session).await;
        Some(session)
    }

    /// Create a new session with its router already marked as initialized.
    ///
    /// Used by the optional-sessions feature to serve requests from clients
    /// that skip the initialize handshake.
    pub(super) async fn create_initialized(
        &self,
        router: McpRouter,
        service_factory: ServiceFactory,
    ) -> Option<Arc<Session>> {
        // Pre-initialize the router's session state so it won't reject requests
        router.session().mark_preinitialized();

        let session = {
            let mut sessions = self.sessions.write().await;

            if let Some(max) = self.config.max_sessions
                && sessions.len() >= max
            {
                return None;
            }

            let session = Arc::new(Session::new(
                router,
                self.sampling_enabled,
                service_factory,
                self.events.clone(),
            ));
            // Pre-initialized sessions bypass the full MCP handshake (they
            // exist for clients that don't track session IDs). Mark the
            // notification as already received so strict_initialization checks
            // don't reject their requests.
            session
                .initialized_notification_received
                .store(true, Ordering::Release);
            sessions.insert(session.id.clone(), session.clone());
            tracing::debug!(session_id = %session.id, "Created pre-initialized session (optional_sessions)");
            session
        };
        self.persist_new(&session).await;
        Some(session)
    }

    /// Create a pre-initialized session from a boxed service.
    pub(super) async fn create_initialized_from_service(
        &self,
        service: McpBoxService,
    ) -> Option<Arc<Session>> {
        let session = {
            let mut sessions = self.sessions.write().await;

            if let Some(max) = self.config.max_sessions
                && sessions.len() >= max
            {
                return None;
            }

            let session = Arc::new(Session::from_service(service, self.events.clone()));
            // Pre-initialized sessions bypass the full MCP handshake; mark the
            // notification as already received.
            session
                .initialized_notification_received
                .store(true, Ordering::Release);
            sessions.insert(session.id.clone(), session.clone());
            tracing::debug!(session_id = %session.id, "Created pre-initialized session from service (optional_sessions)");
            session
        };
        self.persist_new(&session).await;
        Some(session)
    }

    pub(super) async fn get(&self, id: &str) -> Option<Arc<Session>> {
        // Fast path: the session is live in this process.
        {
            let sessions = self.sessions.read().await;
            if let Some(session) = sessions.get(id).cloned() {
                session.touch().await;
                // Keep the registry read lock through the external save. A
                // concurrent removal must acquire the write lock, so its
                // persistent delete is ordered after this refresh and cannot
                // be undone by it.
                self.save_record(&session).await;
                return Some(session);
            }
        }

        // Slow path #1: the session is unknown locally but the persistent
        // store has a record — rebuild it.
        match self.persistent.load(id).await {
            Ok(Some(record)) => {
                tracing::info!(session_id = %id, "Restoring session from persistent store");
                if let Some(session) = self.restore_from_record(record).await {
                    return Some(session);
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(session_id = %id, error = %e, "Failed to load session record");
            }
        }

        // Slow path #2 (opt-in): auto-reinitialize with synthetic client
        // info so the client can continue without a re-handshake. Useful
        // for single-instance restarts where no external store is
        // configured; loses original client identity.
        if self.auto_reinit {
            tracing::info!(session_id = %id, "Auto-reinitializing unknown session");
            return self.auto_reinitialize(id).await;
        }

        None
    }

    /// Restore a live [`Session`] from a persisted [`SessionRecord`].
    ///
    /// The caller must ensure the record's ID is not already live locally;
    /// on success the session is inserted into the local registry, the
    /// event counter is seeded so new event IDs don't collide with buffered
    /// ones, and the record's `last_accessed` is refreshed and saved back to
    /// the store. Legacy resource subscription memberships are not persisted;
    /// the restored session starts empty and the client must resubscribe.
    async fn restore_from_record(
        &self,
        record: crate::session_store::SessionRecord,
    ) -> Option<Arc<Session>> {
        let session = {
            let mut sessions = self.sessions.write().await;

            if let Some(max) = self.config.max_sessions
                && sessions.len() >= max
            {
                tracing::warn!(
                    max_sessions = max,
                    "Session limit reached, cannot restore session"
                );
                return None;
            }

            // Guard against a concurrent create that beat us here.
            if let Some(existing) = sessions.get(&record.id).cloned() {
                existing.touch().await;
                return Some(existing);
            }

            let session: Arc<Session> = match &self.service_source {
                ServiceSource::Router { router, factory } => Arc::new(Session::restored(
                    &record,
                    router.with_fresh_session(),
                    self.sampling_enabled,
                    factory.clone(),
                    self.events.clone(),
                )),
                ServiceSource::Service(svc) => {
                    let service = svc.lock().unwrap().clone();
                    Arc::new(Session::from_service_restored(
                        service,
                        &record,
                        self.events.clone(),
                    ))
                }
            };

            sessions.insert(record.id.clone(), session.clone());
            tracing::debug!(session_id = %session.id, "Restored session into local registry");
            session
        };

        // Seed the event counter past the highest buffered event ID so new
        // SSE events don't collide with ones the client may still replay.
        if let Ok(events) = self.events.replay_after(&record.id, 0).await
            && let Some(max_id) = events.iter().map(|e| e.id).max()
        {
            session
                .event_counter
                .store(max_id + 1, std::sync::atomic::Ordering::SeqCst);
        }

        // Refresh last_accessed in the store so the record doesn't expire
        // immediately after restore.
        let mut refreshed = record;
        refreshed.touch(self.config.ttl);
        if let Err(e) = self.persistent.save(&refreshed).await {
            tracing::warn!(session_id = %refreshed.id, error = %e, "Failed to refresh restored session record");
        }

        Some(session)
    }

    /// Create a new session with the requested ID and synthetic client
    /// info, skipping the initialize handshake. Used when `auto_reinit`
    /// is enabled and no stored record exists.
    ///
    /// Loses the original client's identity and capabilities — the server
    /// sees a session from client `"auto-recovered"`.
    async fn auto_reinitialize(&self, id: &str) -> Option<Arc<Session>> {
        let mut record = crate::session_store::SessionRecord::new(
            id.to_string(),
            LATEST_PROTOCOL_VERSION.to_string(),
            self.config.ttl,
        );
        record.client_info = Some(crate::protocol::Implementation {
            name: "auto-recovered".into(),
            version: "unknown".into(),
            title: None,
            description: None,
            icons: None,
            website_url: None,
            meta: None,
        });
        record.client_capabilities = Some(crate::protocol::ClientCapabilities::default());

        // Persist first so a concurrent request sees the record. Ignore
        // persistence errors; the in-memory session will still work.
        if let Err(e) = self.persistent.create(&mut record).await {
            tracing::warn!(session_id = %id, error = %e, "Failed to persist auto-reinitialized session");
        }

        self.restore_from_record(record).await
    }

    pub(super) async fn remove(&self, id: &str) -> bool {
        let removed = {
            let mut sessions = self.sessions.write().await;
            sessions.remove(id).is_some()
        };
        if removed {
            tracing::debug!(session_id = %id, "Removed session");
            if let Err(e) = self.persistent.delete(id).await {
                tracing::warn!(session_id = %id, error = %e, "Failed to delete session record");
            }
            if let Err(e) = self.events.purge_session(id).await {
                tracing::warn!(session_id = %id, error = %e, "Failed to purge session events");
            }
        }
        removed
    }

    /// Route a pre-serialized external notification to live session SSE
    /// broadcast channels.
    ///
    /// Resource updates are limited to subscribed router-backed sessions;
    /// other notification kinds and service-backed sessions preserve the
    /// existing broadcast behavior. Failures to send (no SSE receiver
    /// attached to a session yet) are silent.
    pub(super) async fn broadcast_external_notification(
        &self,
        notification: &ServerNotification,
        json: &str,
    ) {
        let sessions = self.sessions.read().await;
        for session in sessions.values() {
            if session.accepts_external_notification(notification) {
                let _ = session.notifications_tx.send(json.to_string());
            }
        }
    }

    /// Remove expired sessions, returns count of removed sessions
    pub(super) async fn cleanup_expired(&self) -> usize {
        let expired = {
            let mut sessions = self.sessions.write().await;
            let ttl = self.config.ttl;

            let mut expired = Vec::new();
            for (id, session) in sessions.iter() {
                if session.is_expired(ttl).await {
                    expired.push(id.clone());
                }
            }

            for id in &expired {
                sessions.remove(id);
                tracing::debug!(session_id = %id, "Expired session removed");
            }

            if !expired.is_empty() {
                tracing::info!(
                    expired_count = expired.len(),
                    remaining = sessions.len(),
                    "Session cleanup completed"
                );
            }
            expired
        };

        for id in &expired {
            if let Err(e) = self.persistent.delete(id).await {
                tracing::warn!(session_id = %id, error = %e, "Failed to delete expired session record");
            }
            if let Err(e) = self.events.purge_session(id).await {
                tracing::warn!(session_id = %id, error = %e, "Failed to purge expired session events");
            }
        }

        expired.len()
    }
}

/// Metadata about an active session.
///
/// Returned by [`SessionHandle::list_sessions()`].
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// The session ID.
    pub id: String,
    /// How long ago this session was created.
    pub created_at: Duration,
    /// How long ago this session was last accessed.
    pub last_activity: Duration,
}

/// A handle for managing HTTP transport sessions and final subscription streams.
///
/// Obtained from [`HttpTransport::into_router_with_handle()`] or
/// [`HttpTransport::into_router_at_with_handle()`]. The handle is cheap to
/// clone and can be shared across threads.
///
/// # Example
///
/// ```rust,ignore
/// use tower_mcp::transport::http::HttpTransport;
///
/// let transport = HttpTransport::new(router);
/// let (router, handle) = transport.into_router_with_handle();
///
/// // Later, in an admin endpoint:
/// let count = handle.session_count().await;
/// for info in handle.list_sessions().await {
///     println!("{}: created {:?} ago", info.id, info.created_at);
/// }
/// handle.terminate_session("session-id").await;
///
/// // During graceful server shutdown (with the `stateless` feature):
/// handle.close_subscriptions();
/// ```
#[derive(Clone)]
pub struct SessionHandle {
    pub(super) store: Arc<SessionRegistry>,
    #[cfg(feature = "stateless")]
    pub(super) modern_subscriptions: Arc<ModernSubscriptionRegistry>,
}

impl SessionHandle {
    /// Returns the number of currently active sessions.
    pub async fn session_count(&self) -> usize {
        self.store.sessions.read().await.len()
    }

    /// Returns metadata for all active sessions.
    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        let sessions = self.store.sessions.read().await;
        let mut infos = Vec::with_capacity(sessions.len());
        for session in sessions.values() {
            let last_accessed = session.last_accessed.read().await;
            infos.push(SessionInfo {
                id: session.id.clone(),
                created_at: session.created_at.elapsed(),
                last_activity: last_accessed.elapsed(),
            });
        }
        infos
    }

    /// Terminates a session by ID, returning `true` if the session existed.
    pub async fn terminate_session(&self, id: &str) -> bool {
        self.store.remove(id).await
    }

    /// Returns the number of active final-protocol subscription streams.
    #[cfg(feature = "stateless")]
    pub fn subscription_count(&self) -> usize {
        self.modern_subscriptions.len()
    }

    /// Gracefully finish every active final-protocol subscription stream.
    ///
    /// Each stream receives its terminal `SubscriptionsListenResult` before
    /// closing. Returns the number of streams that were drained.
    #[cfg(feature = "stateless")]
    pub fn close_subscriptions(&self) -> usize {
        self.modern_subscriptions.close_all()
    }
}
