//! MCP session state management
//!
//! Tracks the lifecycle state of an MCP connection as per the specification.
//! The session progresses through phases: Uninitialized -> Initializing -> Initialized.
//!
//! Sessions also support type-safe extensions for storing arbitrary data like
//! authentication claims, user roles, or other session-scoped state.

use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::router::Extensions;

/// Session lifecycle phase
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
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

    /// Transition from Uninitialized to Initializing.
    /// Called after responding to an `initialize` request.
    /// Returns true if the transition was successful.
    pub fn mark_initializing(&self) -> bool {
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
    /// Accepts transitions from both `Initializing` and `Uninitialized` states.
    /// The `Uninitialized → Initialized` path handles a race condition in HTTP
    /// transports where the client sends the `initialized` notification before
    /// the server has finished processing the `initialize` request (the session
    /// is stored in `Uninitialized` state before the request is dispatched).
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

        // Handle the race: Uninitialized → Initialized
        // This occurs when the initialized notification arrives before
        // the initialize request has been fully processed.
        self.phase
            .compare_exchange(
                SessionPhase::Uninitialized as u8,
                SessionPhase::Initialized as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Check if a request method is allowed in the current phase.
    /// Per spec:
    /// - Before initialization: only `initialize` and `ping` are valid
    /// - During all phases: `ping` is always valid
    pub fn is_request_allowed(&self, method: &str) -> bool {
        match self.phase() {
            SessionPhase::Uninitialized => {
                matches!(method, "initialize" | "ping")
            }
            SessionPhase::Initializing | SessionPhase::Initialized => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_mark_initialized_from_uninitialized() {
        let session = SessionState::new();

        // Start in Uninitialized, skip straight to Initialized
        // This handles the race where `initialized` notification arrives
        // before the `initialize` request is fully processed.
        assert_eq!(session.phase(), SessionPhase::Uninitialized);
        assert!(session.mark_initialized());
        assert_eq!(session.phase(), SessionPhase::Initialized);
        assert!(session.is_initialized());

        // All requests allowed
        assert!(session.is_request_allowed("tools/list"));
        assert!(session.is_request_allowed("ping"));
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
