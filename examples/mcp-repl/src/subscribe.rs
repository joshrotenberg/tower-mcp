//! The `subscribe` / `unsubscribe` / `subscriptions` built-ins: resource
//! update subscriptions and the set currently held.
//!
//! The protocol side is two RPCs and a notification: `resources/subscribe`
//! asks the server to send `notifications/resources/updated` for a URI, and
//! the REPL prints those inline as they arrive, like progress and log lines.
//! The set of active subscriptions lives here rather than being threaded
//! through `handle_line`, since the notification callback and the command
//! both need it and neither owns the other.

use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};

static ACTIVE: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();

fn active() -> &'static Mutex<BTreeSet<String>> {
    ACTIVE.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Record a subscription. False when it was already held, which is how the
/// REPL reports a re-subscribe rather than claiming a new one.
pub fn add(uri: &str) -> bool {
    active().lock().unwrap().insert(uri.to_string())
}

/// Drop a subscription. False when it was not held.
pub fn remove(uri: &str) -> bool {
    active().lock().unwrap().remove(uri)
}

/// The active subscriptions, in URI order so repeated listings are stable.
pub fn list() -> Vec<String> {
    active().lock().unwrap().iter().cloned().collect()
}

/// Whether a URI is subscribed. Used to label an update that arrives for
/// something the REPL did not ask for (a shared session, or a server that
/// pushes updates unprompted).
pub fn contains(uri: &str) -> bool {
    active().lock().unwrap().contains(uri)
}

/// A server's `resources.subscribe` capability, read from the initialize
/// result. `None` when the server was not initialized.
///
/// A server that does not advertise this will reject `resources/subscribe`,
/// so the REPL says so up front rather than letting the error be the answer.
pub fn server_supports(capabilities: &serde_json::Value) -> bool {
    capabilities
        .pointer("/resources/subscribe")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The registry is process-global, so one test owns it end to end rather
    // than several racing over the same set.
    #[test]
    fn subscriptions_are_tracked_deduplicated_and_ordered() {
        assert!(add("note://b"));
        assert!(add("note://a"));
        // A second subscribe to the same URI is not a new subscription.
        assert!(!add("note://a"));
        assert!(contains("note://a"));
        assert_eq!(list(), ["note://a", "note://b"]);

        assert!(remove("note://a"));
        // Unsubscribing something not held reports that, rather than pretending.
        assert!(!remove("note://a"));
        assert!(!contains("note://a"));
        assert_eq!(list(), ["note://b"]);
        remove("note://b");
        assert!(list().is_empty());
    }

    #[test]
    fn capability_is_read_from_the_initialize_result() {
        let yes = serde_json::json!({ "resources": { "subscribe": true } });
        let no = serde_json::json!({ "resources": { "listChanged": true } });
        assert!(server_supports(&yes));
        assert!(!server_supports(&no));
        // A server with no resources capability at all.
        assert!(!server_supports(&serde_json::json!({ "tools": {} })));
    }
}
