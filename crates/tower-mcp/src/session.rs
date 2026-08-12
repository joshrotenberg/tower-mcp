//! MCP session state management
//!
//! Tracks the lifecycle state of an MCP connection as per the specification.
//! The session progresses through phases: Uninitialized -> Initializing -> Initialized.
//!
//! Sessions also support type-safe extensions for storing arbitrary data like
//! authentication claims, user roles, or other session-scoped state.

use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::router::Extensions;

/// Session lifecycle phase
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum SessionPhase {
    /// Initial state - only `initialize` and `ping` requests are valid
    Uninitialized = 0,
    /// Server has responded to `initialize`, waiting for `initialized` notification
    Initializing = 1,
    /// `initialized` notification received, normal operation
    Initialized = 2,
}

impl From<u8> for SessionPhase {
    fn from(value: u8) -> Self {
        match value {
            0 => SessionPhase::Uninitialized,
            1 => SessionPhase::Initializing,
            2 => SessionPhase::Initialized,
            _ => SessionPhase::Uninitialized,
        }
    }
}

/// Shared session state that can be cloned across requests.
///
/// Uses atomic operations for thread-safe state transitions. Includes a type-safe
/// extensions map for storing session-scoped data like authentication claims.
///
/// # Example
///
/// ```rust
/// use tower_mcp::SessionState;
///
/// #[derive(Debug, Clone)]
/// struct UserClaims {
///     user_id: String,
///     role: String,
/// }
///
/// let session = SessionState::new();
///
/// // Store auth claims in the session
/// session.insert(UserClaims {
///     user_id: "user123".to_string(),
///     role: "admin".to_string(),
/// });
///
/// // Retrieve claims later
/// if let Some(claims) = session.get::<UserClaims>() {
///     assert_eq!(claims.role, "admin");
/// }
/// ```
#[derive(Clone)]
pub struct SessionState {
    phase: Arc<AtomicU8>,
    handshake_started: Arc<AtomicBool>,
    extensions: Arc<RwLock<Extensions>>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionState {
    /// Create a new session in the Uninitialized phase
    pub fn new() -> Self {
        Self {
            phase: Arc::new(AtomicU8::new(SessionPhase::Uninitialized as u8)),
            handshake_started: Arc::new(AtomicBool::new(false)),
            extensions: Arc::new(RwLock::new(Extensions::new())),
        }
    }

    /// Insert a value into the session extensions.
    ///
    /// This is typically used by auth middleware to store claims that can
    /// be checked by capability filters.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::SessionState;
    ///
    /// let session = SessionState::new();
    /// session.insert(42u32);
    /// assert_eq!(session.get::<u32>(), Some(42));
    /// ```
    pub fn insert<T: Send + Sync + Clone + 'static>(&self, val: T) {
        if let Ok(mut ext) = self.extensions.write() {
            ext.insert(val);
        }
    }

    /// Get a cloned value from the session extensions.
    ///
    /// Returns `None` if no value of the given type has been inserted or if
    /// the lock cannot be acquired.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::SessionState;
    ///
    /// let session = SessionState::new();
    /// session.insert("hello".to_string());
    /// assert_eq!(session.get::<String>(), Some("hello".to_string()));
    /// assert_eq!(session.get::<u32>(), None);
    /// ```
    pub fn get<T: Send + Sync + Clone + 'static>(&self) -> Option<T> {
        self.extensions
            .read()
            .ok()
            .and_then(|ext| ext.get::<T>().cloned())
    }

    /// Get the current session phase
    pub fn phase(&self) -> SessionPhase {
        SessionPhase::from(self.phase.load(Ordering::Acquire))
    }

    /// Check if the session is initialized (operation phase)
    pub fn is_initialized(&self) -> bool {
        self.phase() == SessionPhase::Initialized
    }

    /// Record that an `initialize` request has been received for this session.
    ///
    /// Transports that create a session *in response to* an `initialize` frame
    /// call this at the front door, before the request is dispatched. That is
    /// what lets [`mark_initialized`](Self::mark_initialized) tell the #458
    /// race (the `initialized` notification overtook a dispatch that is still
    /// running) apart from a client that never sent `initialize` at all.
    ///
    /// [`mark_initializing`](Self::mark_initializing) also sets this, so a
    /// transport that dispatches `initialize` in frame order does not need to
    /// call it. Idempotent, and never cleared.
    pub fn mark_handshake_started(&self) {
        self.handshake_started.store(true, Ordering::Release);
    }

    /// Whether an `initialize` request has been received for this session.
    ///
    /// False for a fresh session, and for one a client is trying to open with
    /// an unsolicited `initialized` notification.
    pub fn handshake_started(&self) -> bool {
        self.handshake_started.load(Ordering::Acquire)
    }

    /// Transition from Uninitialized to Initializing.
    /// Called after responding to an `initialize` request.
    /// Returns true if the transition was successful.
    ///
    /// Also records that the handshake has started, so the notification that
    /// follows can complete it.
    pub fn mark_initializing(&self) -> bool {
        self.mark_handshake_started();
        self.phase
            .compare_exchange(
                SessionPhase::Uninitialized as u8,
                SessionPhase::Initializing as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Transition to Initialized phase.
    /// Called when receiving an `initialized` notification.
    ///
    /// Accepts `Initializing → Initialized`, and `Uninitialized → Initialized`
    /// only once [`mark_handshake_started`](Self::mark_handshake_started) has
    /// run. The latter path handles a race in HTTP transports where the client
    /// sends the `initialized` notification before the server has finished
    /// processing the `initialize` request (#458); the handshake flag is what
    /// keeps it from also accepting a client that skipped `initialize`, which
    /// would leave the server serving a peer whose protocol version and
    /// capabilities it never learned.
    ///
    /// Transports that serve without a handshake by design (restored sessions,
    /// `optional_sessions`, the stateless 2026-07-28 path) want
    /// [`mark_preinitialized`](Self::mark_preinitialized) instead.
    ///
    /// Returns true if the transition was successful.
    pub fn mark_initialized(&self) -> bool {
        // Try the expected path first: Initializing → Initialized
        if self
            .phase
            .compare_exchange(
                SessionPhase::Initializing as u8,
                SessionPhase::Initialized as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            return true;
        }

        // Handle the race: Uninitialized → Initialized, but only for a session
        // that has actually seen an `initialize` request. Without that check a
        // single unsolicited notification opens the whole surface.
        if !self.handshake_started() {
            return false;
        }

        self.phase
            .compare_exchange(
                SessionPhase::Uninitialized as u8,
                SessionPhase::Initialized as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Move the session straight to `Initialized`, no handshake required.
    ///
    /// For transports that serve requests without an `initialize` exchange
    /// because the protocol or the deployment says they should: a session
    /// restored from a store (it was initialized on another instance), the
    /// `optional_sessions` opt-in for clients that do not carry a session ID
    /// forward, and the stateless 2026-07-28 path, which has no handshake at
    /// all.
    ///
    /// This is a server-side decision, unlike
    /// [`mark_initialized`](Self::mark_initialized), which acts on a frame the
    /// client sent. Returns true if the phase changed.
    pub fn mark_preinitialized(&self) -> bool {
        self.mark_handshake_started();
        self.phase
            .swap(SessionPhase::Initialized as u8, Ordering::AcqRel)
            != SessionPhase::Initialized as u8
    }

    /// Check if a request method is allowed in the current phase.
    /// Per spec:
    /// - Before initialization: only `initialize` and `ping` are valid
    /// - During all phases: `ping` is always valid
    pub fn is_request_allowed(&self, method: &str) -> bool {
        match self.phase() {
            SessionPhase::Uninitialized => {
                // server/discover (SEP-1442) is allowed before initialization
                matches!(method, "initialize" | "ping" | "server/discover")
            }
            SessionPhase::Initializing | SessionPhase::Initialized => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[derive(Clone, Debug)]
    enum LifecycleOperation {
        MarkInitializing,
        MarkInitialized,
        MarkHandshakeStarted,
        MarkPreinitialized,
        CheckRequest(String),
    }

    fn lifecycle_operation() -> impl Strategy<Value = LifecycleOperation> {
        prop_oneof![
            Just(LifecycleOperation::MarkInitializing),
            Just(LifecycleOperation::MarkInitialized),
            Just(LifecycleOperation::MarkHandshakeStarted),
            Just(LifecycleOperation::MarkPreinitialized),
            prop_oneof![
                Just("initialize".to_string()),
                Just("ping".to_string()),
                Just("server/discover".to_string()),
                Just("tools/list".to_string()),
                "[a-z/_.-]{0,64}",
            ]
            .prop_map(LifecycleOperation::CheckRequest),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Model random lifecycle sequences and assert that the atomic state
        /// machine never permits an illegal transition or pre-init request.
        #[test]
        fn lifecycle_matches_model(
            operations in prop::collection::vec(lifecycle_operation(), 0..256)
        ) {
            let session = SessionState::new();
            let mut expected_phase = SessionPhase::Uninitialized;
            let mut expected_handshake = false;

            for operation in operations {
                match operation {
                    LifecycleOperation::MarkInitializing => {
                        let expected_success = expected_phase == SessionPhase::Uninitialized;
                        prop_assert_eq!(session.mark_initializing(), expected_success);
                        expected_handshake = true;
                        if expected_success {
                            expected_phase = SessionPhase::Initializing;
                        }
                    }
                    LifecycleOperation::MarkInitialized => {
                        // Uninitialized only advances once the handshake has
                        // been recorded; Initializing always does.
                        let expected_success = match expected_phase {
                            SessionPhase::Initializing => true,
                            SessionPhase::Uninitialized => expected_handshake,
                            SessionPhase::Initialized => false,
                        };
                        prop_assert_eq!(session.mark_initialized(), expected_success);
                        if expected_success {
                            expected_phase = SessionPhase::Initialized;
                        }
                    }
                    LifecycleOperation::MarkHandshakeStarted => {
                        session.mark_handshake_started();
                        expected_handshake = true;
                    }
                    LifecycleOperation::MarkPreinitialized => {
                        let expected_success = expected_phase != SessionPhase::Initialized;
                        prop_assert_eq!(session.mark_preinitialized(), expected_success);
                        expected_handshake = true;
                        expected_phase = SessionPhase::Initialized;
                    }
                    LifecycleOperation::CheckRequest(method) => {
                        let expected_allowed = expected_phase != SessionPhase::Uninitialized
                            || matches!(method.as_str(), "initialize" | "ping" | "server/discover");
                        prop_assert_eq!(
                            session.is_request_allowed(&method),
                            expected_allowed,
                            "phase={:?}, method={:?}",
                            expected_phase,
                            method
                        );
                    }
                }
                prop_assert_eq!(session.phase(), expected_phase);
                prop_assert_eq!(session.handshake_started(), expected_handshake);
                prop_assert_eq!(
                    session.is_initialized(),
                    expected_phase == SessionPhase::Initialized
                );
                // The invariant the phase is a proxy for: a session can only
                // be serving if an initialize was seen or the server waived it.
                prop_assert!(expected_phase != SessionPhase::Initialized || expected_handshake);
            }
        }
    }

    #[test]
    fn test_session_lifecycle() {
        let session = SessionState::new();

        // Initial state
        assert_eq!(session.phase(), SessionPhase::Uninitialized);
        assert!(!session.is_initialized());

        // Only initialize and ping allowed
        assert!(session.is_request_allowed("initialize"));
        assert!(session.is_request_allowed("ping"));
        assert!(!session.is_request_allowed("tools/list"));

        // Transition to initializing
        assert!(session.mark_initializing());
        assert_eq!(session.phase(), SessionPhase::Initializing);
        assert!(!session.is_initialized());

        // Can't mark initializing again
        assert!(!session.mark_initializing());

        // All requests allowed during initializing
        assert!(session.is_request_allowed("tools/list"));

        // Transition to initialized
        assert!(session.mark_initialized());
        assert_eq!(session.phase(), SessionPhase::Initialized);
        assert!(session.is_initialized());

        // Can't mark initialized again
        assert!(!session.mark_initialized());
    }

    #[test]
    fn test_session_clone_shares_state() {
        let session1 = SessionState::new();
        let session2 = session1.clone();

        session1.mark_initializing();
        assert_eq!(session2.phase(), SessionPhase::Initializing);

        session2.mark_initialized();
        assert_eq!(session1.phase(), SessionPhase::Initialized);
    }

    #[test]
    fn test_session_extensions_insert_and_get() {
        let session = SessionState::new();

        // Insert and retrieve a value
        session.insert(42u32);
        assert_eq!(session.get::<u32>(), Some(42));

        // Different type returns None
        assert_eq!(session.get::<String>(), None);
    }

    #[test]
    fn test_session_extensions_overwrite() {
        let session = SessionState::new();

        session.insert(42u32);
        assert_eq!(session.get::<u32>(), Some(42));

        // Overwrite with new value
        session.insert(100u32);
        assert_eq!(session.get::<u32>(), Some(100));
    }

    #[test]
    fn test_session_extensions_multiple_types() {
        let session = SessionState::new();

        session.insert(42u32);
        session.insert("hello".to_string());
        session.insert(true);

        assert_eq!(session.get::<u32>(), Some(42));
        assert_eq!(session.get::<String>(), Some("hello".to_string()));
        assert_eq!(session.get::<bool>(), Some(true));
    }

    #[test]
    fn test_session_extensions_shared_across_clones() {
        let session1 = SessionState::new();
        let session2 = session1.clone();

        // Insert in one clone
        session1.insert(42u32);

        // Should be visible in the other
        assert_eq!(session2.get::<u32>(), Some(42));

        // Insert in the second clone
        session2.insert("world".to_string());

        // Should be visible in the first
        assert_eq!(session1.get::<String>(), Some("world".to_string()));
    }

    /// The #458 race: the `initialized` notification arrives before the
    /// `initialize` request has finished dispatching, so the phase is still
    /// Uninitialized. The transport recorded the handshake when it created the
    /// session, so the notification still completes it.
    #[test]
    fn test_mark_initialized_from_uninitialized_after_handshake_started() {
        let session = SessionState::new();
        session.mark_handshake_started();

        assert_eq!(session.phase(), SessionPhase::Uninitialized);
        assert!(session.mark_initialized());
        assert_eq!(session.phase(), SessionPhase::Initialized);
        assert!(session.is_initialized());

        // All requests allowed
        assert!(session.is_request_allowed("tools/list"));
        assert!(session.is_request_allowed("ping"));
    }

    /// #1269: without a handshake there is nothing to complete. A client that
    /// sends only the notification has negotiated no protocol version and
    /// declared no capabilities, so the guard must hold.
    #[test]
    fn test_mark_initialized_from_uninitialized_without_handshake_is_refused() {
        let session = SessionState::new();

        assert!(!session.handshake_started());
        assert!(!session.mark_initialized());
        assert_eq!(session.phase(), SessionPhase::Uninitialized);
        assert!(!session.is_initialized());

        // The pre-initialize guard still applies.
        assert!(!session.is_request_allowed("tools/list"));
        assert!(session.is_request_allowed("initialize"));
        assert!(session.is_request_allowed("ping"));

        // And a repeat does not wear it down.
        assert!(!session.mark_initialized());
        assert!(!session.mark_initialized());
        assert_eq!(session.phase(), SessionPhase::Uninitialized);
    }

    /// A refused notification must not poison the real handshake that follows.
    #[test]
    fn test_handshake_still_works_after_a_refused_notification() {
        let session = SessionState::new();

        assert!(!session.mark_initialized());
        assert!(session.mark_initializing());
        assert_eq!(session.phase(), SessionPhase::Initializing);
        assert!(session.mark_initialized());
        assert!(session.is_initialized());
    }

    /// Server-side promotion needs no handshake, and works from either phase.
    #[test]
    fn test_mark_preinitialized_skips_the_handshake() {
        let session = SessionState::new();
        assert!(session.mark_preinitialized());
        assert_eq!(session.phase(), SessionPhase::Initialized);
        assert!(session.handshake_started());
        assert!(session.is_request_allowed("tools/list"));

        // Already there: no change to report.
        assert!(!session.mark_preinitialized());

        // From Initializing as well.
        let mid = SessionState::new();
        mid.mark_initializing();
        assert!(mid.mark_preinitialized());
        assert_eq!(mid.phase(), SessionPhase::Initialized);
    }

    #[test]
    fn test_handshake_flag_is_shared_across_clones() {
        let session1 = SessionState::new();
        let session2 = session1.clone();

        assert!(!session2.handshake_started());
        session1.mark_handshake_started();
        assert!(session2.handshake_started());

        // Which means the clone can absorb the race too.
        assert!(session2.mark_initialized());
        assert!(session1.is_initialized());
    }

    #[test]
    fn test_mark_initialized_idempotent_when_already_initialized() {
        let session = SessionState::new();

        // Normal lifecycle
        session.mark_initializing();
        session.mark_initialized();
        assert_eq!(session.phase(), SessionPhase::Initialized);

        // Calling mark_initialized again should fail (already in target state)
        assert!(!session.mark_initialized());
        assert_eq!(session.phase(), SessionPhase::Initialized);
    }

    #[test]
    fn test_session_extensions_custom_type() {
        #[derive(Debug, Clone, PartialEq)]
        struct UserClaims {
            user_id: String,
            role: String,
        }

        let session = SessionState::new();

        session.insert(UserClaims {
            user_id: "user123".to_string(),
            role: "admin".to_string(),
        });

        let claims = session.get::<UserClaims>();
        assert!(claims.is_some());
        let claims = claims.unwrap();
        assert_eq!(claims.user_id, "user123");
        assert_eq!(claims.role, "admin");
    }
}
