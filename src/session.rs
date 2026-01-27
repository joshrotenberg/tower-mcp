//! MCP session state management
//!
//! Tracks the lifecycle state of an MCP connection as per the specification.
//! The session progresses through phases: Uninitialized -> Initializing -> Initialized.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

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
/// Uses atomic operations for thread-safe state transitions.
#[derive(Clone)]
pub struct SessionState {
    phase: Arc<AtomicU8>,
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
        }
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

    /// Transition from Initializing to Initialized.
    /// Called when receiving an `initialized` notification.
    /// Returns true if the transition was successful.
    pub fn mark_initialized(&self) -> bool {
        self.phase
            .compare_exchange(
                SessionPhase::Initializing as u8,
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
}
