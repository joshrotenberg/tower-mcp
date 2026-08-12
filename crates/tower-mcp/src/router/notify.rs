//! Logging, subscriptions, and change notifications for
//! [`McpRouter`](super::McpRouter).
//!
//! Everything the server says to a client without being asked: `log` and its
//! four level helpers, the resource-subscription bookkeeping, and the
//! `notifications/*/list_changed` and `notifications/resources/updated`
//! senders.
//!
//! Subscriptions live here rather than beside the resource registry because
//! they exist to drive [`McpRouter::notify_resource_updated`]: a subscription
//! nobody notifies is bookkeeping for its own sake.
//!
//! Split out of `router.rs` in #1256. An `impl` block in a child module, so
//! neither the type nor its API changed.

use super::*;

impl McpRouter {
    /// Send a log message notification to the client
    ///
    /// This sends a `notifications/message` notification with the given parameters.
    /// Returns `true` if the notification was sent, `false` if no notification channel
    /// is configured.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use tower_mcp::protocol::{LogLevel, LoggingMessageParams};
    ///
    /// // Simple info message
    /// router.log(LoggingMessageParams::new(LogLevel::Info,
    ///     serde_json::json!({"message": "Operation completed"})
    /// ));
    ///
    /// // Error with logger name
    /// router.log(LoggingMessageParams::new(LogLevel::Error,
    ///     serde_json::json!({"error": "Connection failed"}))
    ///     .with_logger("database"));
    /// ```
    pub fn log(&self, params: LoggingMessageParams) -> bool {
        let Some(tx) = &self.inner.notification_tx else {
            return false;
        };
        tx.try_send(ServerNotification::LogMessage(params)).is_ok()
    }

    /// Send an info-level log message
    ///
    /// Convenience method for sending an info log with a message string.
    pub fn log_info(&self, message: &str) -> bool {
        self.log(LoggingMessageParams::new(
            LogLevel::Info,
            serde_json::json!({ "message": message }),
        ))
    }

    /// Send a warning-level log message
    pub fn log_warning(&self, message: &str) -> bool {
        self.log(LoggingMessageParams::new(
            LogLevel::Warning,
            serde_json::json!({ "message": message }),
        ))
    }

    /// Send an error-level log message
    pub fn log_error(&self, message: &str) -> bool {
        self.log(LoggingMessageParams::new(
            LogLevel::Error,
            serde_json::json!({ "message": message }),
        ))
    }

    /// Send a debug-level log message
    pub fn log_debug(&self, message: &str) -> bool {
        self.log(LoggingMessageParams::new(
            LogLevel::Debug,
            serde_json::json!({ "message": message }),
        ))
    }

    /// Check if a resource URI is currently subscribed
    pub fn is_subscribed(&self, uri: &str) -> bool {
        if let Ok(subs) = self.inner.subscriptions.read() {
            return subs.contains(uri);
        }
        false
    }

    /// Get a list of all subscribed resource URIs
    pub fn subscribed_uris(&self) -> Vec<String> {
        if let Ok(subs) = self.inner.subscriptions.read() {
            return subs.iter().cloned().collect();
        }
        Vec::new()
    }

    /// Subscribe to a resource URI
    pub(super) fn subscribe(&self, uri: &str) -> bool {
        if let Ok(mut subs) = self.inner.subscriptions.write() {
            return subs.insert(uri.to_string());
        }
        false
    }

    /// Unsubscribe from a resource URI
    pub(super) fn unsubscribe(&self, uri: &str) -> bool {
        if let Ok(mut subs) = self.inner.subscriptions.write() {
            return subs.remove(uri);
        }
        false
    }

    /// Notify clients that a subscribed resource has been updated
    ///
    /// Legacy sessions receive the notification only after
    /// `resources/subscribe`. Final HTTP listeners are filtered by their
    /// `subscriptions/listen` registration.
    /// Returns `true` if the notification was sent.
    pub fn notify_resource_updated(&self, uri: &str) -> bool {
        let notification = ServerNotification::ResourceUpdated {
            uri: uri.to_string(),
        };
        let mut sent = false;

        if self.is_subscribed(uri)
            && let Some(tx) = &self.inner.notification_tx
        {
            sent |= tx.try_send(notification.clone()).is_ok();
        }

        #[cfg(all(feature = "http", feature = "stateless"))]
        if let Ok(active) = self.inner.modern_notification_sink.read()
            && let Some(sink) = active.as_ref()
        {
            sent |= sink(&notification);
        }

        sent
    }

    /// Push a task's current state to subscribed `subscriptions/listen`
    /// streams as a `notifications/tasks` notification.
    ///
    /// The router already announces the transitions it drives: completion,
    /// failure, cancellation, and the resumption that follows a
    /// `tasks/update`. Call this after driving a transition yourself, most
    /// commonly [`TaskStore::require_input`], which a tool handler invokes on
    /// the store directly.
    ///
    /// Announcing task creation is deliberately left out. A client learns the
    /// task ID from the `tools/call` result, so it cannot have subscribed to a
    /// task before that result reaches it.
    ///
    /// [`TaskStore::require_input`]: crate::async_task::TaskStore::require_input
    pub async fn notify_task_status_changed(&self, task_id: &str) {
        self.notify_task_state(task_id).await;
    }

    /// Notify clients that the list of available resources has changed
    ///
    /// Returns `true` if the notification was sent.
    pub fn notify_resources_list_changed(&self) -> bool {
        let Some(tx) = &self.inner.notification_tx else {
            return false;
        };
        tx.try_send(ServerNotification::ResourcesListChanged)
            .is_ok()
    }

    /// Notify clients that the list of available tools has changed
    ///
    /// Returns `true` if the notification was sent.
    pub fn notify_tools_list_changed(&self) -> bool {
        let Some(tx) = &self.inner.notification_tx else {
            return false;
        };
        tx.try_send(ServerNotification::ToolsListChanged).is_ok()
    }

    /// Notify clients that the list of available prompts has changed
    ///
    /// Returns `true` if the notification was sent.
    pub fn notify_prompts_list_changed(&self) -> bool {
        let Some(tx) = &self.inner.notification_tx else {
            return false;
        };
        tx.try_send(ServerNotification::PromptsListChanged).is_ok()
    }
}
