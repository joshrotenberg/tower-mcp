//! Async task management for long-running MCP operations
//!
//! This module provides task lifecycle management for operations that may take
//! longer than a typical request/response cycle. Tasks can be created via
//! task-augmented `tools/call` requests, tracked, polled for status, and cancelled.
//!
//! Task state lives behind the pluggable [`TaskStore`] trait, mirroring the
//! shape of [`crate::session_store`] and [`crate::event_store`]: a trait, an
//! error enum, and an in-memory default. By default routers use
//! [`MemoryTaskStore`], which keeps tasks in an in-process map (behavior
//! identical to earlier versions). External stores (Redis, Postgres, etc.) can
//! be plugged in so `tasks/get` works on any instance behind a load balancer
//! in the sessionless 2026-07-28 flows (SEP-2663).
//!
//! # Example
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use tower_mcp::async_task::{MemoryTaskStore, TaskStore};
//! use tower_mcp::McpRouter;
//!
//! let store: Arc<dyn TaskStore> = Arc::new(MemoryTaskStore::new());
//! let router = McpRouter::new().task_store(store);
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::protocol::{CallToolResult, TaskObject, TaskStatus};

/// Default time-to-live for completed tasks (5 minutes, in milliseconds)
const DEFAULT_TTL_MS: u64 = 300_000;

/// Default poll interval suggestion (2 seconds, in milliseconds)
const DEFAULT_POLL_INTERVAL_MS: u64 = 2_000;

/// Internal task representation with full state
#[derive(Debug)]
pub struct Task {
    /// Unique task identifier
    pub id: String,
    /// Name of the tool being executed
    pub tool_name: String,
    /// Arguments passed to the tool
    pub arguments: serde_json::Value,
    /// Current task status
    pub status: TaskStatus,
    /// When the task was created
    pub created_at: Instant,
    /// ISO 8601 timestamp string
    pub created_at_str: String,
    /// ISO 8601 timestamp of last state change
    pub last_updated_at_str: String,
    /// Time-to-live in milliseconds (for cleanup after completion)
    pub ttl: u64,
    /// Suggested polling interval in milliseconds
    pub poll_interval: u64,
    /// Human-readable status message
    pub status_message: Option<String>,
    /// The result of the tool call (when completed)
    pub result: Option<CallToolResult>,
    /// Error message (when failed)
    pub error: Option<String>,
    /// Cancellation token for aborting the task
    pub cancellation_token: CancellationToken,
    /// When the task reached terminal status (for TTL tracking)
    pub completed_at: Option<Instant>,
    /// Notified when task reaches a terminal state
    pub completion_notify: Arc<tokio::sync::Notify>,
}

impl Task {
    /// Create a new task
    fn new(id: String, tool_name: String, arguments: serde_json::Value, ttl: Option<u64>) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let now_str = chrono_now_iso8601();
        Self {
            id,
            tool_name,
            arguments,
            status: TaskStatus::Working,
            created_at: Instant::now(),
            created_at_str: now_str.clone(),
            last_updated_at_str: now_str,
            ttl: ttl.unwrap_or(DEFAULT_TTL_MS),
            poll_interval: DEFAULT_POLL_INTERVAL_MS,
            status_message: Some("Task started".to_string()),
            result: None,
            error: None,
            cancellation_token: CancellationToken { cancelled },
            completed_at: None,
            completion_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Convert to TaskObject for API responses
    pub fn to_task_object(&self) -> TaskObject {
        TaskObject {
            task_id: self.id.clone(),
            status: self.status,
            status_message: self.status_message.clone(),
            created_at: self.created_at_str.clone(),
            last_updated_at: self.last_updated_at_str.clone(),
            ttl: Some(self.ttl),
            poll_interval: Some(self.poll_interval),
            meta: None,
        }
    }

    /// Check if this task should be cleaned up (TTL expired)
    pub fn is_expired(&self) -> bool {
        if let Some(completed_at) = self.completed_at {
            completed_at.elapsed() > Duration::from_millis(self.ttl)
        } else {
            false
        }
    }

    /// Check if the task has been cancelled
    pub fn is_cancelled(&self) -> bool {
        self.cancellation_token.is_cancelled()
    }
}

/// A shareable cancellation token for task management
#[derive(Debug, Clone)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    /// Check if cancellation has been requested
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Request cancellation
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

/// Errors returned by [`TaskStore`] implementations.
///
/// Mirrors the three-variant shape of
/// [`SessionStoreError`](crate::session_store::SessionStoreError): encode and
/// decode errors from (de)serializing task state, and catch-all backend errors
/// from the storage layer. [`MemoryTaskStore`] never returns errors; the
/// variants exist for external implementations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TaskStoreError {
    /// Failed to encode task state (e.g. serde serialization error).
    #[error("encode error: {0}")]
    Encode(String),
    /// Failed to decode task state (e.g. corrupt data in the backend).
    #[error("decode error: {0}")]
    Decode(String),
    /// Backend error (e.g. connection failure, transient storage error).
    #[error("backend error: {0}")]
    Backend(String),
}

/// Result alias for task store operations.
pub type Result<T> = std::result::Result<T, TaskStoreError>;

/// A task's current snapshot: the task object plus any result or error
/// captured so far.
pub type TaskSnapshot = (TaskObject, Option<CallToolResult>, Option<String>);

/// Storage backend for async task state.
///
/// Implementations persist task lifecycle state keyed by task ID. The default
/// implementation is [`MemoryTaskStore`]; external stores (Redis, Postgres,
/// etc.) typically live in separate crates.
///
/// # Semantics
///
/// - Terminal states ([`TaskStatus::is_terminal`]) are immutable: once a task
///   is completed, failed, or cancelled, further transitions must be rejected
///   (`Ok(false)` from the transition methods).
/// - [`cancel_task`](Self::cancel_task) must signal the task's
///   [`CancellationToken`] even if the task is already terminal.
/// - [`wait_for_completion`](Self::wait_for_completion) blocks until the task
///   reaches a terminal state; how an implementation waits (notification,
///   polling, pub/sub) is an implementation detail and must not leak into the
///   trait.
#[async_trait]
pub trait TaskStore: Send + Sync + 'static {
    /// Create and store a new task.
    ///
    /// Returns the task ID and a cancellation token for the spawned work.
    async fn create_task(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        ttl: Option<u64>,
    ) -> Result<(String, CancellationToken)>;

    /// Get task object by ID. Returns `None` if unknown.
    async fn get_task(&self, task_id: &str) -> Result<Option<TaskObject>>;

    /// Get a task's full snapshot (task object, result, error) by ID.
    async fn get_task_result(&self, task_id: &str) -> Result<Option<TaskSnapshot>>;

    /// Wait for a task to reach a terminal state, then return its snapshot.
    ///
    /// If the task is already terminal, returns immediately. Otherwise blocks
    /// until the task completes, fails, or is cancelled. Returns `None` if
    /// the task is unknown.
    async fn wait_for_completion(&self, task_id: &str) -> Result<Option<TaskSnapshot>>;

    /// List all tasks, optionally filtered by status.
    async fn list_tasks(&self, status_filter: Option<TaskStatus>) -> Result<Vec<TaskObject>>;

    /// Mark a task as requiring input.
    ///
    /// Returns `Ok(false)` if the task is unknown or already terminal.
    async fn require_input(&self, task_id: &str, message: &str) -> Result<bool>;

    /// Mark a task as completed with a result.
    ///
    /// Returns `Ok(false)` if the task is unknown or already terminal.
    async fn complete_task(&self, task_id: &str, result: CallToolResult) -> Result<bool>;

    /// Mark a task as failed with an error.
    ///
    /// Returns `Ok(false)` if the task is unknown or already terminal.
    async fn fail_task(&self, task_id: &str, error: &str) -> Result<bool>;

    /// Cancel a task.
    ///
    /// Signals the task's [`CancellationToken`] and, if the task is not
    /// already terminal, marks it cancelled. Returns the updated task object,
    /// or `None` if the task is unknown.
    async fn cancel_task(&self, task_id: &str, reason: Option<&str>) -> Result<Option<TaskObject>>;
}

/// In-memory [`TaskStore`] backed by a `HashMap`.
///
/// This is the default store. Suitable for single-instance deployments. For
/// horizontal scaling, use an external store that shares state across
/// instances. Completion wakeups for
/// [`wait_for_completion`](TaskStore::wait_for_completion) use a per-task
/// [`tokio::sync::Notify`], which is an implementation detail of this store.
#[derive(Debug, Clone)]
pub struct MemoryTaskStore {
    tasks: Arc<RwLock<HashMap<String, Task>>>,
    next_id: Arc<AtomicU64>,
}

impl Default for MemoryTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryTaskStore {
    /// Create a new task store
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Generate a unique task ID
    fn generate_id(&self) -> String {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("task-{}", id)
    }

    /// Remove expired tasks (call periodically for cleanup).
    ///
    /// Returns the number removed. Not part of the [`TaskStore`] trait;
    /// external backends typically expire entries natively (e.g. Redis TTL).
    pub fn cleanup_expired(&self) -> usize {
        if let Ok(mut tasks) = self.tasks.write() {
            let before = tasks.len();
            tasks.retain(|_, t| !t.is_expired());
            before - tasks.len()
        } else {
            0
        }
    }

    /// Get the number of tasks in the store
    #[cfg(test)]
    pub fn len(&self) -> usize {
        if let Ok(tasks) = self.tasks.read() {
            tasks.len()
        } else {
            0
        }
    }

    /// Check if the store is empty
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl TaskStore for MemoryTaskStore {
    async fn create_task(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        ttl: Option<u64>,
    ) -> Result<(String, CancellationToken)> {
        let id = self.generate_id();
        let task = Task::new(id.clone(), tool_name.to_string(), arguments, ttl);
        let token = task.cancellation_token.clone();

        if let Ok(mut tasks) = self.tasks.write() {
            tasks.insert(id.clone(), task);
        }

        Ok((id, token))
    }

    async fn get_task(&self, task_id: &str) -> Result<Option<TaskObject>> {
        Ok(if let Ok(tasks) = self.tasks.read() {
            tasks.get(task_id).map(|t| t.to_task_object())
        } else {
            None
        })
    }

    async fn get_task_result(&self, task_id: &str) -> Result<Option<TaskSnapshot>> {
        Ok(if let Ok(tasks) = self.tasks.read() {
            tasks
                .get(task_id)
                .map(|t| (t.to_task_object(), t.result.clone(), t.error.clone()))
        } else {
            None
        })
    }

    async fn wait_for_completion(&self, task_id: &str) -> Result<Option<TaskSnapshot>> {
        // First check if already terminal and get the notify handle
        let notify = {
            let Ok(tasks) = self.tasks.read() else {
                return Ok(None);
            };
            let Some(task) = tasks.get(task_id) else {
                return Ok(None);
            };
            if task.status.is_terminal() {
                return Ok(Some((
                    task.to_task_object(),
                    task.result.clone(),
                    task.error.clone(),
                )));
            }
            task.completion_notify.clone()
        };

        // Wait for completion notification
        notify.notified().await;

        // Read the result
        self.get_task_result(task_id).await
    }

    async fn list_tasks(&self, status_filter: Option<TaskStatus>) -> Result<Vec<TaskObject>> {
        Ok(if let Ok(tasks) = self.tasks.read() {
            tasks
                .values()
                .filter(|t| status_filter.is_none() || status_filter == Some(t.status))
                .map(|t| t.to_task_object())
                .collect()
        } else {
            vec![]
        })
    }

    async fn require_input(&self, task_id: &str, message: &str) -> Result<bool> {
        let Ok(mut tasks) = self.tasks.write() else {
            return Ok(false);
        };
        let Some(task) = tasks.get_mut(task_id) else {
            return Ok(false);
        };
        if task.status.is_terminal() {
            return Ok(false);
        }
        task.status = TaskStatus::InputRequired;
        task.status_message = Some(message.to_string());
        task.last_updated_at_str = chrono_now_iso8601();
        Ok(true)
    }

    async fn complete_task(&self, task_id: &str, result: CallToolResult) -> Result<bool> {
        let Ok(mut tasks) = self.tasks.write() else {
            return Ok(false);
        };
        let Some(task) = tasks.get_mut(task_id) else {
            return Ok(false);
        };
        if task.status.is_terminal() {
            return Ok(false);
        }
        task.status = TaskStatus::Completed;
        task.status_message = Some("Task completed".to_string());
        task.result = Some(result);
        task.completed_at = Some(Instant::now());
        task.last_updated_at_str = chrono_now_iso8601();
        task.completion_notify.notify_waiters();
        Ok(true)
    }

    async fn fail_task(&self, task_id: &str, error: &str) -> Result<bool> {
        let Ok(mut tasks) = self.tasks.write() else {
            return Ok(false);
        };
        let Some(task) = tasks.get_mut(task_id) else {
            return Ok(false);
        };
        if task.status.is_terminal() {
            return Ok(false);
        }
        task.status = TaskStatus::Failed;
        task.status_message = Some(format!("Task failed: {}", error));
        task.error = Some(error.to_string());
        task.completed_at = Some(Instant::now());
        task.last_updated_at_str = chrono_now_iso8601();
        task.completion_notify.notify_waiters();
        Ok(true)
    }

    async fn cancel_task(&self, task_id: &str, reason: Option<&str>) -> Result<Option<TaskObject>> {
        let Ok(mut tasks) = self.tasks.write() else {
            return Ok(None);
        };
        let Some(task) = tasks.get_mut(task_id) else {
            return Ok(None);
        };

        // Signal cancellation
        task.cancellation_token.cancel();

        // If not already terminal, mark as cancelled
        if !task.status.is_terminal() {
            task.status = TaskStatus::Cancelled;
            task.status_message = Some(
                reason
                    .map(|r| format!("Cancelled: {}", r))
                    .unwrap_or_else(|| "Task cancelled".to_string()),
            );
            task.completed_at = Some(Instant::now());
            task.last_updated_at_str = chrono_now_iso8601();
            task.completion_notify.notify_waiters();
        }
        Ok(Some(task.to_task_object()))
    }
}

/// Generate ISO 8601 timestamp for current time
fn chrono_now_iso8601() -> String {
    use std::time::SystemTime;

    let now = SystemTime::now();
    let duration = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();

    let secs = duration.as_secs();
    let millis = duration.subsec_millis();

    // Simple ISO 8601 format (UTC)
    // Calculate date/time components
    let days = secs / 86400;
    let remaining = secs % 86400;
    let hours = remaining / 3600;
    let remaining = remaining % 3600;
    let minutes = remaining / 60;
    let seconds = remaining % 60;

    // Calculate year/month/day from days since epoch (1970-01-01)
    // This is a simplified calculation that handles leap years
    let mut year = 1970i32;
    let mut remaining_days = days as i32;

    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        year += 1;
    }

    let days_in_months: [i32; 12] = if is_leap_year(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 1;
    for days_in_month in days_in_months.iter() {
        if remaining_days < *days_in_month {
            break;
        }
        remaining_days -= days_in_month;
        month += 1;
    }

    let day = remaining_days + 1;

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hours, minutes, seconds, millis
    )
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_task() {
        let store = MemoryTaskStore::new();
        let (id, token) = store
            .create_task("test-tool", serde_json::json!({"a": 1}), None)
            .await
            .unwrap();

        assert!(id.starts_with("task-"));
        assert!(!token.is_cancelled());

        let info = store
            .get_task(&id)
            .await
            .unwrap()
            .expect("task should exist");
        assert_eq!(info.task_id, id);
        assert_eq!(info.status, TaskStatus::Working);
    }

    #[tokio::test]
    async fn test_task_lifecycle() {
        let store = MemoryTaskStore::new();
        let (id, _) = store
            .create_task("test-tool", serde_json::json!({}), None)
            .await
            .unwrap();

        // Complete task
        assert!(
            store
                .complete_task(&id, CallToolResult::text("Done"))
                .await
                .unwrap()
        );

        let info = store.get_task(&id).await.unwrap().unwrap();
        assert_eq!(info.status, TaskStatus::Completed);
    }

    #[tokio::test]
    async fn test_task_cancellation() {
        let store = MemoryTaskStore::new();
        let (id, token) = store
            .create_task("test-tool", serde_json::json!({}), None)
            .await
            .unwrap();

        assert!(!token.is_cancelled());

        let task_obj = store
            .cancel_task(&id, Some("User requested"))
            .await
            .unwrap();
        assert!(task_obj.is_some());
        assert_eq!(task_obj.unwrap().status, TaskStatus::Cancelled);
        assert!(token.is_cancelled());

        let info = store.get_task(&id).await.unwrap().unwrap();
        assert_eq!(info.status, TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn test_task_failure() {
        let store = MemoryTaskStore::new();
        let (id, _) = store
            .create_task("test-tool", serde_json::json!({}), None)
            .await
            .unwrap();

        assert!(store.fail_task(&id, "Something went wrong").await.unwrap());

        let info = store.get_task(&id).await.unwrap().unwrap();
        assert_eq!(info.status, TaskStatus::Failed);
        assert!(info.status_message.as_ref().unwrap().contains("failed"));
    }

    #[tokio::test]
    async fn test_list_tasks() {
        let store = MemoryTaskStore::new();
        store
            .create_task("tool1", serde_json::json!({}), None)
            .await
            .unwrap();
        store
            .create_task("tool2", serde_json::json!({}), None)
            .await
            .unwrap();
        let (id3, _) = store
            .create_task("tool3", serde_json::json!({}), None)
            .await
            .unwrap();

        // Complete one task
        store
            .complete_task(&id3, CallToolResult::text("Done"))
            .await
            .unwrap();

        // List all tasks
        let all = store.list_tasks(None).await.unwrap();
        assert_eq!(all.len(), 3);

        // List only working tasks
        let working = store.list_tasks(Some(TaskStatus::Working)).await.unwrap();
        assert_eq!(working.len(), 2);

        // List only completed tasks
        let completed = store.list_tasks(Some(TaskStatus::Completed)).await.unwrap();
        assert_eq!(completed.len(), 1);
    }

    #[tokio::test]
    async fn test_terminal_state_immutable() {
        let store = MemoryTaskStore::new();
        let (id, _) = store
            .create_task("test-tool", serde_json::json!({}), None)
            .await
            .unwrap();

        // Complete the task
        store
            .complete_task(&id, CallToolResult::text("Done"))
            .await
            .unwrap();

        // Try to fail - should fail
        assert!(!store.fail_task(&id, "Error").await.unwrap());

        // Status should still be completed
        let info = store.get_task(&id).await.unwrap().unwrap();
        assert_eq!(info.status, TaskStatus::Completed);
    }

    #[tokio::test]
    async fn test_task_ids_unique() {
        let store = MemoryTaskStore::new();
        let (id1, _) = store
            .create_task("tool", serde_json::json!({}), None)
            .await
            .unwrap();
        let (id2, _) = store
            .create_task("tool", serde_json::json!({}), None)
            .await
            .unwrap();
        let (id3, _) = store
            .create_task("tool", serde_json::json!({}), None)
            .await
            .unwrap();

        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }

    #[tokio::test]
    async fn test_get_task_result() {
        let store = MemoryTaskStore::new();
        let (id, _) = store
            .create_task("test-tool", serde_json::json!({}), None)
            .await
            .unwrap();

        // Complete with result
        let result = CallToolResult::text("The result");
        store.complete_task(&id, result).await.unwrap();

        let (task_obj, result, error) = store.get_task_result(&id).await.unwrap().unwrap();
        assert_eq!(task_obj.status, TaskStatus::Completed);
        assert!(result.is_some());
        assert!(error.is_none());
    }

    #[tokio::test]
    async fn test_wait_for_completion_returns_terminal_snapshot() {
        let store = MemoryTaskStore::new();
        let (id, _) = store
            .create_task("test-tool", serde_json::json!({}), None)
            .await
            .unwrap();

        // Complete the task from another task while a waiter is blocked.
        let waiter_store = store.clone();
        let waiter_id = id.clone();
        let waiter =
            tokio::spawn(async move { waiter_store.wait_for_completion(&waiter_id).await });

        tokio::time::sleep(Duration::from_millis(10)).await;
        store
            .complete_task(&id, CallToolResult::text("Done"))
            .await
            .unwrap();

        let (task_obj, result, error) = waiter.await.unwrap().unwrap().unwrap();
        assert_eq!(task_obj.status, TaskStatus::Completed);
        assert!(result.is_some());
        assert!(error.is_none());
    }

    #[tokio::test]
    async fn dyn_task_store_object_safe() {
        // Compile-time check that TaskStore is object-safe.
        let store: Arc<dyn TaskStore> = Arc::new(MemoryTaskStore::new());
        let (id, _) = store
            .create_task("tool", serde_json::json!({}), None)
            .await
            .unwrap();
        assert!(store.get_task(&id).await.unwrap().is_some());
    }

    #[test]
    fn test_iso8601_timestamp() {
        let ts = chrono_now_iso8601();
        // Basic format check
        assert!(ts.ends_with('Z'));
        assert!(ts.contains('T'));
        assert_eq!(ts.len(), 24); // YYYY-MM-DDTHH:MM:SS.mmmZ
    }

    #[test]
    fn test_task_status_display() {
        assert_eq!(TaskStatus::Working.to_string(), "working");
        assert_eq!(TaskStatus::InputRequired.to_string(), "input_required");
        assert_eq!(TaskStatus::Completed.to_string(), "completed");
        assert_eq!(TaskStatus::Failed.to_string(), "failed");
        assert_eq!(TaskStatus::Cancelled.to_string(), "cancelled");
    }

    #[test]
    fn test_task_status_is_terminal() {
        assert!(!TaskStatus::Working.is_terminal());
        assert!(!TaskStatus::InputRequired.is_terminal());
        assert!(TaskStatus::Completed.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
    }
}
