//! Async task management for long-running MCP operations
//!
//! This module provides task lifecycle management for operations that may take
//! longer than a typical request/response cycle. Tasks can be enqueued, tracked,
//! polled for status, and cancelled.
//!
//! # Example
//!
//! ```rust,ignore
//! use tower_mcp::async_task::{TaskStore, Task};
//! use tower_mcp::protocol::TaskStatus;
//!
//! // Create a task store
//! let store = TaskStore::new();
//!
//! // Enqueue a task
//! let task = store.create_task("my-tool", serde_json::json!({"key": "value"}), None);
//!
//! // Get task status
//! let info = store.get_task(&task.id);
//!
//! // Mark task as complete
//! store.complete_task(&task.id, Ok(result));
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::protocol::{CallToolResult, TaskInfo, TaskStatus};

/// Default time-to-live for completed tasks (5 minutes)
const DEFAULT_TTL_SECS: u64 = 300;

/// Default poll interval suggestion (2 seconds)
const DEFAULT_POLL_INTERVAL_SECS: u64 = 2;

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
    /// Time-to-live in seconds (for cleanup after completion)
    pub ttl: u64,
    /// Suggested polling interval in seconds
    pub poll_interval: u64,
    /// Current progress (0.0 - 100.0)
    pub progress: Option<f64>,
    /// Human-readable status message
    pub message: Option<String>,
    /// The result of the tool call (when completed)
    pub result: Option<CallToolResult>,
    /// Error message (when failed)
    pub error: Option<String>,
    /// Cancellation token for aborting the task
    pub cancellation_token: CancellationToken,
    /// When the task reached terminal status (for TTL tracking)
    pub completed_at: Option<Instant>,
}

impl Task {
    /// Create a new task
    fn new(id: String, tool_name: String, arguments: serde_json::Value, ttl: Option<u64>) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        Self {
            id,
            tool_name,
            arguments,
            status: TaskStatus::Working,
            created_at: Instant::now(),
            created_at_str: chrono_now_iso8601(),
            ttl: ttl.unwrap_or(DEFAULT_TTL_SECS),
            poll_interval: DEFAULT_POLL_INTERVAL_SECS,
            progress: None,
            message: Some("Task started".to_string()),
            result: None,
            error: None,
            cancellation_token: CancellationToken { cancelled },
            completed_at: None,
        }
    }

    /// Convert to TaskInfo for API responses
    pub fn to_info(&self) -> TaskInfo {
        TaskInfo {
            task_id: self.id.clone(),
            status: self.status,
            created_at: self.created_at_str.clone(),
            ttl: Some(self.ttl),
            poll_interval: Some(self.poll_interval),
            progress: self.progress,
            message: self.message.clone(),
        }
    }

    /// Check if this task should be cleaned up (TTL expired)
    pub fn is_expired(&self) -> bool {
        if let Some(completed_at) = self.completed_at {
            completed_at.elapsed() > Duration::from_secs(self.ttl)
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

/// Thread-safe task storage
#[derive(Debug, Clone)]
pub struct TaskStore {
    tasks: Arc<RwLock<HashMap<String, Task>>>,
    next_id: Arc<AtomicU64>,
}

impl Default for TaskStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskStore {
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

    /// Create and store a new task
    ///
    /// Returns the task ID and a cancellation token for the spawned work.
    pub fn create_task(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        ttl: Option<u64>,
    ) -> (String, CancellationToken) {
        let id = self.generate_id();
        let task = Task::new(id.clone(), tool_name.to_string(), arguments, ttl);
        let token = task.cancellation_token.clone();

        if let Ok(mut tasks) = self.tasks.write() {
            tasks.insert(id.clone(), task);
        }

        (id, token)
    }

    /// Get task info by ID
    pub fn get_task(&self, task_id: &str) -> Option<TaskInfo> {
        if let Ok(tasks) = self.tasks.read() {
            tasks.get(task_id).map(|t| t.to_info())
        } else {
            None
        }
    }

    /// Get full task data by ID (for internal use)
    pub fn get_task_full(
        &self,
        task_id: &str,
    ) -> Option<(TaskStatus, Option<CallToolResult>, Option<String>)> {
        if let Ok(tasks) = self.tasks.read() {
            tasks
                .get(task_id)
                .map(|t| (t.status, t.result.clone(), t.error.clone()))
        } else {
            None
        }
    }

    /// List all tasks, optionally filtered by status
    pub fn list_tasks(&self, status_filter: Option<TaskStatus>) -> Vec<TaskInfo> {
        if let Ok(tasks) = self.tasks.read() {
            tasks
                .values()
                .filter(|t| status_filter.is_none() || status_filter == Some(t.status))
                .map(|t| t.to_info())
                .collect()
        } else {
            vec![]
        }
    }

    /// Update task progress
    pub fn update_progress(&self, task_id: &str, progress: f64, message: Option<String>) -> bool {
        let Ok(mut tasks) = self.tasks.write() else {
            return false;
        };
        let Some(task) = tasks.get_mut(task_id) else {
            return false;
        };
        if task.status.is_terminal() {
            return false;
        }
        task.progress = Some(progress);
        if let Some(msg) = message {
            task.message = Some(msg);
        }
        true
    }

    /// Mark a task as requiring input
    pub fn require_input(&self, task_id: &str, message: &str) -> bool {
        let Ok(mut tasks) = self.tasks.write() else {
            return false;
        };
        let Some(task) = tasks.get_mut(task_id) else {
            return false;
        };
        if task.status.is_terminal() {
            return false;
        }
        task.status = TaskStatus::InputRequired;
        task.message = Some(message.to_string());
        true
    }

    /// Mark a task as completed with a result
    pub fn complete_task(&self, task_id: &str, result: CallToolResult) -> bool {
        let Ok(mut tasks) = self.tasks.write() else {
            return false;
        };
        let Some(task) = tasks.get_mut(task_id) else {
            return false;
        };
        if task.status.is_terminal() {
            return false;
        }
        task.status = TaskStatus::Completed;
        task.progress = Some(100.0);
        task.message = Some("Task completed".to_string());
        task.result = Some(result);
        task.completed_at = Some(Instant::now());
        true
    }

    /// Mark a task as failed with an error
    pub fn fail_task(&self, task_id: &str, error: &str) -> bool {
        let Ok(mut tasks) = self.tasks.write() else {
            return false;
        };
        let Some(task) = tasks.get_mut(task_id) else {
            return false;
        };
        if task.status.is_terminal() {
            return false;
        }
        task.status = TaskStatus::Failed;
        task.message = Some(format!("Task failed: {}", error));
        task.error = Some(error.to_string());
        task.completed_at = Some(Instant::now());
        true
    }

    /// Cancel a task
    pub fn cancel_task(&self, task_id: &str, reason: Option<&str>) -> Option<TaskStatus> {
        let mut tasks = self.tasks.write().ok()?;
        let task = tasks.get_mut(task_id)?;

        // Signal cancellation
        task.cancellation_token.cancel();

        // If not already terminal, mark as cancelled
        if !task.status.is_terminal() {
            task.status = TaskStatus::Cancelled;
            task.message = Some(
                reason
                    .map(|r| format!("Cancelled: {}", r))
                    .unwrap_or_else(|| "Task cancelled".to_string()),
            );
            task.completed_at = Some(Instant::now());
        }
        Some(task.status)
    }

    /// Remove expired tasks (call periodically for cleanup)
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

    #[test]
    fn test_create_task() {
        let store = TaskStore::new();
        let (id, token) = store.create_task("test-tool", serde_json::json!({"a": 1}), None);

        assert!(id.starts_with("task-"));
        assert!(!token.is_cancelled());

        let info = store.get_task(&id).expect("task should exist");
        assert_eq!(info.task_id, id);
        assert_eq!(info.status, TaskStatus::Working);
    }

    #[test]
    fn test_task_lifecycle() {
        let store = TaskStore::new();
        let (id, _) = store.create_task("test-tool", serde_json::json!({}), None);

        // Update progress
        assert!(store.update_progress(&id, 50.0, Some("Halfway".to_string())));

        let info = store.get_task(&id).unwrap();
        assert_eq!(info.progress, Some(50.0));
        assert_eq!(info.message.as_deref(), Some("Halfway"));

        // Complete task
        assert!(store.complete_task(&id, CallToolResult::text("Done")));

        let info = store.get_task(&id).unwrap();
        assert_eq!(info.status, TaskStatus::Completed);
        assert_eq!(info.progress, Some(100.0));
    }

    #[test]
    fn test_task_cancellation() {
        let store = TaskStore::new();
        let (id, token) = store.create_task("test-tool", serde_json::json!({}), None);

        assert!(!token.is_cancelled());

        let status = store.cancel_task(&id, Some("User requested"));
        assert_eq!(status, Some(TaskStatus::Cancelled));
        assert!(token.is_cancelled());

        let info = store.get_task(&id).unwrap();
        assert_eq!(info.status, TaskStatus::Cancelled);
    }

    #[test]
    fn test_task_failure() {
        let store = TaskStore::new();
        let (id, _) = store.create_task("test-tool", serde_json::json!({}), None);

        assert!(store.fail_task(&id, "Something went wrong"));

        let info = store.get_task(&id).unwrap();
        assert_eq!(info.status, TaskStatus::Failed);
        assert!(info.message.as_ref().unwrap().contains("failed"));
    }

    #[test]
    fn test_list_tasks() {
        let store = TaskStore::new();
        store.create_task("tool1", serde_json::json!({}), None);
        store.create_task("tool2", serde_json::json!({}), None);
        let (id3, _) = store.create_task("tool3", serde_json::json!({}), None);

        // Complete one task
        store.complete_task(&id3, CallToolResult::text("Done"));

        // List all tasks
        let all = store.list_tasks(None);
        assert_eq!(all.len(), 3);

        // List only working tasks
        let working = store.list_tasks(Some(TaskStatus::Working));
        assert_eq!(working.len(), 2);

        // List only completed tasks
        let completed = store.list_tasks(Some(TaskStatus::Completed));
        assert_eq!(completed.len(), 1);
    }

    #[test]
    fn test_terminal_state_immutable() {
        let store = TaskStore::new();
        let (id, _) = store.create_task("test-tool", serde_json::json!({}), None);

        // Complete the task
        store.complete_task(&id, CallToolResult::text("Done"));

        // Try to update - should fail
        assert!(!store.update_progress(&id, 50.0, None));
        assert!(!store.fail_task(&id, "Error"));

        // Status should still be completed
        let info = store.get_task(&id).unwrap();
        assert_eq!(info.status, TaskStatus::Completed);
    }

    #[test]
    fn test_task_ids_unique() {
        let store = TaskStore::new();
        let (id1, _) = store.create_task("tool", serde_json::json!({}), None);
        let (id2, _) = store.create_task("tool", serde_json::json!({}), None);
        let (id3, _) = store.create_task("tool", serde_json::json!({}), None);

        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_get_task_full() {
        let store = TaskStore::new();
        let (id, _) = store.create_task("test-tool", serde_json::json!({}), None);

        // Complete with result
        let result = CallToolResult::text("The result");
        store.complete_task(&id, result);

        let (status, result, error) = store.get_task_full(&id).unwrap();
        assert_eq!(status, TaskStatus::Completed);
        assert!(result.is_some());
        assert!(error.is_none());
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
