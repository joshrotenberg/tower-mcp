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

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::error::JsonRpcError;
use crate::protocol::{CallToolResult, InputRequests, InputResponses, TaskObject, TaskStatus};

/// Default time-to-live for a task (5 minutes, in milliseconds).
///
/// Per SEP-2663 the TTL runs from task creation, not from the moment the task
/// reaches a terminal state.
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
    /// Structured execution error (when failed).
    ///
    /// SEP-2663 requires `tasks/get` to surface a JSON-RPC error object, not a
    /// message string. A tool that returns `CallToolResult { isError: true }`
    /// is a *completed* task carrying an error result, so it never sets this.
    pub error: Option<JsonRpcError>,
    /// Principal that created the task, or `None` when it was created
    /// without an authenticated context.
    ///
    /// Never serialized: ownership is an authorization fact, not wire state.
    pub owner: TaskOwner,
    /// Input requests currently awaiting a client response, keyed as sent.
    pub input_requests: InputRequests,
    /// Keys answered by a previous `tasks/update`.
    pub answered_input_keys: BTreeSet<String>,
    /// Keys displaced by a later [`TaskStore::require_input`] before being
    /// answered.
    pub superseded_input_keys: BTreeSet<String>,
    /// Cancellation token for aborting the task
    pub cancellation_token: CancellationToken,
    /// When the task reached terminal status (for TTL tracking)
    pub completed_at: Option<Instant>,
    /// Notified when task reaches a terminal state
    pub completion_notify: Arc<tokio::sync::Notify>,
}

impl Task {
    /// Create a new task
    fn new(
        id: String,
        tool_name: String,
        arguments: serde_json::Value,
        ttl: Option<u64>,
        owner: TaskOwner,
    ) -> Self {
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
            owner,
            input_requests: InputRequests::new(),
            answered_input_keys: BTreeSet::new(),
            superseded_input_keys: BTreeSet::new(),
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
            result: None,
            error: None,
            meta: None,
        }
    }

    /// Check if this task should be cleaned up (TTL expired).
    ///
    /// The clock runs from creation, per SEP-2663. A long-running task can
    /// therefore expire while still working, which is the intended behavior:
    /// `ttlMs` bounds how long the server retains the task, not how long it
    /// lingers after finishing.
    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() > Duration::from_millis(self.ttl)
    }

    /// Outstanding input requests, if the task is waiting on the client.
    pub fn outstanding_input_requests(&self) -> &InputRequests {
        &self.input_requests
    }

    /// Check if the task has been cancelled
    pub fn is_cancelled(&self) -> bool {
        self.cancellation_token.is_cancelled()
    }
}

/// Generate an unguessable task identifier.
///
/// SEP-2663 notes that a task ID can function as a bearer token: anything that
/// knows the ID can poll, update, or cancel the task. Identifiers are therefore
/// 128 random bits from the system CSPRNG, rendered as hex, rather than a
/// sequential counter.
///
/// # Panics
///
/// Panics if the operating system entropy source is unavailable. A server that
/// cannot generate unguessable identifiers must not fall back to guessable
/// ones.
pub fn generate_task_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("system entropy source unavailable for task ID generation");
    let mut id = String::with_capacity(2 * bytes.len());
    for byte in bytes {
        let _ = write!(id, "{byte:02x}");
    }
    id
}

/// The principal a task belongs to.
///
/// `None` means the task was created without an authenticated context, which
/// is the normal case for a server with no authentication configured.
///
/// SEP-2663 notes that a task ID can behave as a bearer token. Recording the
/// owner is what stops the ID from being sufficient authority on its own once
/// a second principal learns it.
pub type TaskOwner = Option<String>;

/// Whether `principal` may act on a task owned by `owner`.
///
/// Matching is equality, not "protect owned tasks and leave unowned ones
/// open". An unowned task can only exist if it was created with no
/// authenticated context, so a request that now carries a principal is a
/// different security context and is refused.
pub fn owner_matches(owner: &TaskOwner, principal: Option<&str>) -> bool {
    owner.as_deref() == principal
}

/// Outcome of applying `tasks/update.inputResponses` to a task.
///
/// SEP-2663 requires partial responses to be honored: keys that match an
/// outstanding request are consumed, everything else is ignored rather than
/// rejected, and any request left unanswered stays outstanding.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppliedInputResponses {
    /// Keys matched to an outstanding request and consumed.
    pub accepted: BTreeSet<String>,
    /// Keys ignored because they were never issued, were already answered, or
    /// were superseded by a later request.
    pub ignored: BTreeSet<String>,
    /// Requests still awaiting a response after this update.
    pub still_outstanding: BTreeSet<String>,
}

impl AppliedInputResponses {
    /// Whether every outstanding request has now been answered.
    pub fn is_complete(&self) -> bool {
        self.still_outstanding.is_empty()
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
///
/// The error is a structured [`JsonRpcError`] because SEP-2663 requires
/// `tasks/get` on a failed task to return a JSON-RPC error object.
pub type TaskSnapshot = (TaskObject, Option<CallToolResult>, Option<JsonRpcError>);

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
/// - An expired task is indistinguishable from an unknown one. Reads return
///   `None` once `ttlMs` has elapsed since creation, whether or not the entry
///   has actually been reclaimed, so callers cannot probe for the existence of
///   a task whose retention window has closed.
/// - [`cancel_task`](Self::cancel_task) must signal the task's
///   [`CancellationToken`] even if the task is already terminal.
/// - [`wait_for_completion`](Self::wait_for_completion) blocks until the task
///   reaches a terminal state; how an implementation waits (notification,
///   polling, pub/sub) is an implementation detail and must not leak into the
///   trait.
#[async_trait]
pub trait TaskStore: Send + Sync + 'static {
    /// Create and store a new task owned by `owner`.
    ///
    /// Returns the task ID and a cancellation token for the spawned work.
    /// `owner` is the authenticated principal responsible for the task, or
    /// `None` when the request carried no authenticated context.
    async fn create_task(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        ttl: Option<u64>,
        owner: TaskOwner,
    ) -> Result<(String, CancellationToken)>;

    /// Read a task's owner.
    ///
    /// The outer `Option` distinguishes a known task from an unknown or
    /// expired one; the inner [`TaskOwner`] distinguishes an owned task from
    /// one created without an authenticated principal.
    async fn task_owner(&self, task_id: &str) -> Result<Option<TaskOwner>>;

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

    /// Mark a task as requiring input, recording the requests to be answered.
    ///
    /// `requests` replaces the outstanding set. Any key that was outstanding
    /// and is not re-issued becomes superseded; a re-issued key is a fresh
    /// question and becomes outstanding again even if previously answered.
    ///
    /// Returns `Ok(false)` if the task is unknown, expired, or already
    /// terminal.
    async fn require_input(
        &self,
        task_id: &str,
        requests: InputRequests,
        message: Option<&str>,
    ) -> Result<bool>;

    /// Read the requests a task is currently waiting on.
    ///
    /// Returns an empty map when the task is not `input_required`, and `None`
    /// when the task is unknown or expired.
    async fn outstanding_input_requests(&self, task_id: &str) -> Result<Option<InputRequests>>;

    /// Apply `tasks/update.inputResponses` to a task.
    ///
    /// Consumes the keys that match an outstanding request and ignores the
    /// rest. When the last outstanding request is answered the task returns to
    /// [`TaskStatus::Working`].
    ///
    /// Returns `None` if the task is unknown, expired, or already terminal.
    async fn apply_input_responses(
        &self,
        task_id: &str,
        responses: InputResponses,
    ) -> Result<Option<AppliedInputResponses>>;

    /// Update a task's time-to-live, measured from creation.
    ///
    /// SEP-2663 allows `ttlMs` to change over a task's lifetime. Returns
    /// `Ok(false)` if the task is unknown or already expired.
    async fn set_ttl(&self, task_id: &str, ttl_ms: u64) -> Result<bool>;

    /// Mark a task as completed with a result.
    ///
    /// A result carrying `isError: true` still completes the task: the tool
    /// ran and produced a domain error, which SEP-2663 distinguishes from an
    /// execution failure.
    ///
    /// Returns `Ok(false)` if the task is unknown, expired, or already
    /// terminal.
    async fn complete_task(&self, task_id: &str, result: CallToolResult) -> Result<bool>;

    /// Mark a task as failed with a structured execution error.
    ///
    /// Returns `Ok(false)` if the task is unknown, expired, or already
    /// terminal.
    async fn fail_task(&self, task_id: &str, error: JsonRpcError) -> Result<bool>;

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
        }
    }

    /// Remove expired tasks (call periodically for cleanup).
    ///
    /// Returns the number removed. Not part of the [`TaskStore`] trait;
    /// external backends typically expire entries natively (e.g. Redis TTL).
    ///
    /// Calling this is an optimization, not a correctness requirement: reads
    /// already treat an expired task as absent.
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
        owner: TaskOwner,
    ) -> Result<(String, CancellationToken)> {
        let id = generate_task_id();
        let task = Task::new(id.clone(), tool_name.to_string(), arguments, ttl, owner);
        let token = task.cancellation_token.clone();

        if let Ok(mut tasks) = self.tasks.write() {
            tasks.insert(id.clone(), task);
        }

        Ok((id, token))
    }

    async fn get_task(&self, task_id: &str) -> Result<Option<TaskObject>> {
        Ok(if let Ok(tasks) = self.tasks.read() {
            tasks
                .get(task_id)
                .filter(|t| !t.is_expired())
                .map(|t| t.to_task_object())
        } else {
            None
        })
    }

    async fn task_owner(&self, task_id: &str) -> Result<Option<TaskOwner>> {
        Ok(if let Ok(tasks) = self.tasks.read() {
            tasks
                .get(task_id)
                .filter(|t| !t.is_expired())
                .map(|t| t.owner.clone())
        } else {
            None
        })
    }

    async fn get_task_result(&self, task_id: &str) -> Result<Option<TaskSnapshot>> {
        Ok(if let Ok(tasks) = self.tasks.read() {
            tasks
                .get(task_id)
                .filter(|t| !t.is_expired())
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
            let Some(task) = tasks.get(task_id).filter(|t| !t.is_expired()) else {
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
                .filter(|t| !t.is_expired())
                .filter(|t| status_filter.is_none() || status_filter == Some(t.status))
                .map(|t| t.to_task_object())
                .collect()
        } else {
            vec![]
        })
    }

    async fn require_input(
        &self,
        task_id: &str,
        requests: InputRequests,
        message: Option<&str>,
    ) -> Result<bool> {
        let Ok(mut tasks) = self.tasks.write() else {
            return Ok(false);
        };
        let Some(task) = tasks.get_mut(task_id).filter(|t| !t.is_expired()) else {
            return Ok(false);
        };
        if task.status.is_terminal() {
            return Ok(false);
        }

        // Outstanding requests the server did not re-issue are superseded.
        for key in std::mem::take(&mut task.input_requests).into_keys() {
            if !requests.contains_key(&key) {
                task.superseded_input_keys.insert(key);
            }
        }
        // A re-issued key is a fresh question, whatever its prior fate.
        for key in requests.keys() {
            task.answered_input_keys.remove(key);
            task.superseded_input_keys.remove(key);
        }

        task.input_requests = requests;
        task.status = TaskStatus::InputRequired;
        task.status_message = Some(
            message
                .map(str::to_string)
                .unwrap_or_else(|| "Awaiting client input".to_string()),
        );
        task.last_updated_at_str = chrono_now_iso8601();
        Ok(true)
    }

    async fn outstanding_input_requests(&self, task_id: &str) -> Result<Option<InputRequests>> {
        Ok(if let Ok(tasks) = self.tasks.read() {
            tasks
                .get(task_id)
                .filter(|t| !t.is_expired())
                .map(|t| t.input_requests.clone())
        } else {
            None
        })
    }

    async fn apply_input_responses(
        &self,
        task_id: &str,
        responses: InputResponses,
    ) -> Result<Option<AppliedInputResponses>> {
        let Ok(mut tasks) = self.tasks.write() else {
            return Ok(None);
        };
        let Some(task) = tasks.get_mut(task_id).filter(|t| !t.is_expired()) else {
            return Ok(None);
        };
        if task.status.is_terminal() {
            return Ok(None);
        }

        let mut applied = AppliedInputResponses::default();
        for key in responses.into_keys() {
            if task.input_requests.remove(&key).is_some() {
                task.answered_input_keys.insert(key.clone());
                applied.accepted.insert(key);
            } else {
                // Never issued, already answered, or superseded. All three are
                // ignored rather than rejected, so a client replaying a stale
                // update does not fail the task.
                applied.ignored.insert(key);
            }
        }
        applied.still_outstanding = task.input_requests.keys().cloned().collect();

        if !applied.accepted.is_empty() {
            task.last_updated_at_str = chrono_now_iso8601();
        }
        if applied.is_complete() && task.status == TaskStatus::InputRequired {
            task.status = TaskStatus::Working;
            task.status_message = Some("Task resumed".to_string());
        }
        Ok(Some(applied))
    }

    async fn set_ttl(&self, task_id: &str, ttl_ms: u64) -> Result<bool> {
        let Ok(mut tasks) = self.tasks.write() else {
            return Ok(false);
        };
        let Some(task) = tasks.get_mut(task_id).filter(|t| !t.is_expired()) else {
            return Ok(false);
        };
        task.ttl = ttl_ms;
        task.last_updated_at_str = chrono_now_iso8601();
        Ok(true)
    }

    async fn complete_task(&self, task_id: &str, result: CallToolResult) -> Result<bool> {
        let Ok(mut tasks) = self.tasks.write() else {
            return Ok(false);
        };
        let Some(task) = tasks.get_mut(task_id).filter(|t| !t.is_expired()) else {
            return Ok(false);
        };
        if task.status.is_terminal() {
            return Ok(false);
        }
        task.status = TaskStatus::Completed;
        task.status_message = Some("Task completed".to_string());
        task.result = Some(result);
        task.input_requests.clear();
        task.completed_at = Some(Instant::now());
        task.last_updated_at_str = chrono_now_iso8601();
        task.completion_notify.notify_waiters();
        Ok(true)
    }

    async fn fail_task(&self, task_id: &str, error: JsonRpcError) -> Result<bool> {
        let Ok(mut tasks) = self.tasks.write() else {
            return Ok(false);
        };
        let Some(task) = tasks.get_mut(task_id).filter(|t| !t.is_expired()) else {
            return Ok(false);
        };
        if task.status.is_terminal() {
            return Ok(false);
        }
        task.status = TaskStatus::Failed;
        task.status_message = Some(format!("Task failed: {}", error.message));
        task.error = Some(error);
        task.input_requests.clear();
        task.completed_at = Some(Instant::now());
        task.last_updated_at_str = chrono_now_iso8601();
        task.completion_notify.notify_waiters();
        Ok(true)
    }

    async fn cancel_task(&self, task_id: &str, reason: Option<&str>) -> Result<Option<TaskObject>> {
        let Ok(mut tasks) = self.tasks.write() else {
            return Ok(None);
        };
        let Some(task) = tasks.get_mut(task_id).filter(|t| !t.is_expired()) else {
            return Ok(None);
        };

        // Signal cancellation
        task.cancellation_token.cancel();

        // If not already terminal, mark as cancelled
        if !task.status.is_terminal() {
            task.input_requests.clear();
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

/// Build the validated extension declaration for the final Tasks extension.
///
/// The SEP-2663 capability shape is an empty object: support is declared by
/// the identifier's presence, with no settings to negotiate.
pub fn tasks_extension() -> crate::ExtensionDeclaration {
    crate::ExtensionDeclaration::empty(crate::protocol::TASKS_EXTENSION_ID)
        .expect("the built-in Tasks extension declaration is valid")
}

impl crate::McpRouter {
    /// Advertise final Tasks support (SEP-2663) from this server.
    ///
    /// Compiling the task APIs does not advertise them. A server opts in here,
    /// and only then does the final protocol path advertise
    /// `io.modelcontextprotocol/tasks`, accept task-augmented `tools/call`
    /// requests, or serve the final task methods. Legacy 2025-11-25 task
    /// behavior is unaffected either way.
    pub fn with_tasks(self) -> Self {
        self.with_protocol_extension(tasks_extension())
    }
}

impl crate::McpClientBuilder {
    /// Declare final Tasks support (SEP-2663) from this client.
    pub fn with_tasks(self) -> Self {
        self.with_protocol_extension(tasks_extension())
    }
}

impl crate::RequestContext {
    /// Whether both peers negotiated the final Tasks extension.
    ///
    /// Task dispatch keys off this rather than off the protocol version: a
    /// 2026-07-28 request from a client that did not declare the extension
    /// must not receive a task.
    pub fn supports_tasks(&self) -> bool {
        self.negotiated_extensions()
            .is_some_and(|extensions| extensions.contains(crate::protocol::TASKS_EXTENSION_ID))
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
    use crate::protocol::{
        ElicitAction, ElicitResult, InputRequest, InputResponse, ListRootsParams,
    };

    #[tokio::test]
    async fn test_create_task() {
        let store = MemoryTaskStore::new();
        let (id, token) = store
            .create_task("test-tool", serde_json::json!({"a": 1}), None, None)
            .await
            .unwrap();

        assert!(!id.is_empty());
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
            .create_task("test-tool", serde_json::json!({}), None, None)
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
            .create_task("test-tool", serde_json::json!({}), None, None)
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
            .create_task("test-tool", serde_json::json!({}), None, None)
            .await
            .unwrap();

        assert!(
            store
                .fail_task(&id, JsonRpcError::internal_error("Something went wrong"))
                .await
                .unwrap()
        );

        let info = store.get_task(&id).await.unwrap().unwrap();
        assert_eq!(info.status, TaskStatus::Failed);
        assert!(info.status_message.as_ref().unwrap().contains("failed"));
    }

    #[tokio::test]
    async fn test_list_tasks() {
        let store = MemoryTaskStore::new();
        store
            .create_task("tool1", serde_json::json!({}), None, None)
            .await
            .unwrap();
        store
            .create_task("tool2", serde_json::json!({}), None, None)
            .await
            .unwrap();
        let (id3, _) = store
            .create_task("tool3", serde_json::json!({}), None, None)
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
            .create_task("test-tool", serde_json::json!({}), None, None)
            .await
            .unwrap();

        // Complete the task
        store
            .complete_task(&id, CallToolResult::text("Done"))
            .await
            .unwrap();

        // Try to fail - should fail
        assert!(
            !store
                .fail_task(&id, JsonRpcError::internal_error("Error"))
                .await
                .unwrap()
        );

        // Status should still be completed
        let info = store.get_task(&id).await.unwrap().unwrap();
        assert_eq!(info.status, TaskStatus::Completed);
    }

    #[tokio::test]
    async fn test_task_ids_unique() {
        let store = MemoryTaskStore::new();
        let (id1, _) = store
            .create_task("tool", serde_json::json!({}), None, None)
            .await
            .unwrap();
        let (id2, _) = store
            .create_task("tool", serde_json::json!({}), None, None)
            .await
            .unwrap();
        let (id3, _) = store
            .create_task("tool", serde_json::json!({}), None, None)
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
            .create_task("test-tool", serde_json::json!({}), None, None)
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
            .create_task("test-tool", serde_json::json!({}), None, None)
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
            .create_task("tool", serde_json::json!({}), None, None)
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

    fn requests(keys: &[&str]) -> InputRequests {
        keys.iter()
            .map(|k| {
                (
                    k.to_string(),
                    InputRequest::ListRoots(ListRootsParams { meta: None }),
                )
            })
            .collect()
    }

    fn accept(key: &str) -> (String, InputResponse) {
        (
            key.to_string(),
            InputResponse::Elicit(ElicitResult {
                action: ElicitAction::Accept,
                content: None,
                meta: None,
            }),
        )
    }

    async fn working_task(store: &MemoryTaskStore, ttl: Option<u64>) -> String {
        store
            .create_task("tool", serde_json::json!({}), ttl, None)
            .await
            .unwrap()
            .0
    }

    #[tokio::test]
    async fn task_ids_are_unguessable_not_sequential() {
        let store = MemoryTaskStore::new();
        let mut ids = BTreeSet::new();
        for _ in 0..64 {
            ids.insert(working_task(&store, None).await);
        }
        assert_eq!(ids.len(), 64, "task IDs collided");

        for id in &ids {
            assert_eq!(id.len(), 32, "expected 128 bits of hex: {id}");
            assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "{id}");
            assert!(!id.starts_with("task-"), "sequential-looking ID: {id}");
        }

        // A counter would make every ID a near-neighbor of the last. Require
        // the set to span a wide range of leading bytes instead.
        let leading: BTreeSet<&str> = ids.iter().map(|id| &id[..2]).collect();
        assert!(
            leading.len() > 32,
            "only {} distinct leading bytes across 64 IDs",
            leading.len()
        );
    }

    #[tokio::test]
    async fn ttl_runs_from_creation_and_expired_tasks_read_as_absent() {
        let store = MemoryTaskStore::new();
        let id = working_task(&store, Some(0)).await;

        // TTL of 0 expires immediately, while the task is still working, so
        // the clock plainly is not waiting for a terminal state.
        tokio::time::sleep(Duration::from_millis(5)).await;

        assert!(store.get_task(&id).await.unwrap().is_none());
        assert!(store.get_task_result(&id).await.unwrap().is_none());
        assert!(store.list_tasks(None).await.unwrap().is_empty());
        assert!(
            store
                .outstanding_input_requests(&id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(store.cancel_task(&id, None).await.unwrap().is_none());
        assert!(!store.set_ttl(&id, 60_000).await.unwrap());
        assert!(
            !store
                .complete_task(&id, CallToolResult::text("late"))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn ttl_is_mutable_over_the_task_lifetime() {
        let store = MemoryTaskStore::new();
        let id = working_task(&store, Some(60_000)).await;

        assert!(store.set_ttl(&id, 120_000).await.unwrap());
        let task = store.get_task(&id).await.unwrap().unwrap();
        assert_eq!(task.ttl, Some(120_000));

        // Shortening the window to zero retires the task immediately.
        assert!(store.set_ttl(&id, 0).await.unwrap());
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(store.get_task(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn require_input_records_requests_and_exposes_them() {
        let store = MemoryTaskStore::new();
        let id = working_task(&store, None).await;

        assert!(
            store
                .require_input(&id, requests(&["approval", "region"]), Some("need input"))
                .await
                .unwrap()
        );

        let task = store.get_task(&id).await.unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::InputRequired);
        assert_eq!(task.status_message.as_deref(), Some("need input"));

        let outstanding = store
            .outstanding_input_requests(&id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            outstanding.keys().collect::<Vec<_>>(),
            vec!["approval", "region"],
            "every outstanding request must be exposed, not just the newest"
        );
    }

    #[tokio::test]
    async fn partial_input_responses_leave_the_rest_outstanding() {
        let store = MemoryTaskStore::new();
        let id = working_task(&store, None).await;
        store
            .require_input(&id, requests(&["approval", "region"]), None)
            .await
            .unwrap();

        let applied = store
            .apply_input_responses(&id, [accept("approval")].into_iter().collect())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(applied.accepted, ["approval".to_string()].into());
        assert!(applied.ignored.is_empty());
        assert_eq!(applied.still_outstanding, ["region".to_string()].into());
        assert!(!applied.is_complete());

        // The task stays blocked while anything is unanswered.
        let task = store.get_task(&id).await.unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::InputRequired);
        assert_eq!(
            store
                .outstanding_input_requests(&id)
                .await
                .unwrap()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec!["region"]
        );

        // Answering the last one resumes the task.
        let applied = store
            .apply_input_responses(&id, [accept("region")].into_iter().collect())
            .await
            .unwrap()
            .unwrap();
        assert!(applied.is_complete());
        assert_eq!(
            store.get_task(&id).await.unwrap().unwrap().status,
            TaskStatus::Working
        );
    }

    #[tokio::test]
    async fn unknown_answered_and_superseded_response_keys_are_ignored() {
        let store = MemoryTaskStore::new();
        let id = working_task(&store, None).await;
        store
            .require_input(&id, requests(&["approval", "stale"]), None)
            .await
            .unwrap();

        // Answer one, then re-issue a set that drops `stale`, superseding it.
        store
            .apply_input_responses(&id, [accept("approval")].into_iter().collect())
            .await
            .unwrap()
            .unwrap();
        store
            .require_input(&id, requests(&["region"]), None)
            .await
            .unwrap();

        let applied = store
            .apply_input_responses(
                &id,
                [accept("never-issued"), accept("approval"), accept("stale")]
                    .into_iter()
                    .collect(),
            )
            .await
            .unwrap()
            .unwrap();

        assert!(
            applied.accepted.is_empty(),
            "none of these keys are outstanding"
        );
        assert_eq!(
            applied.ignored,
            [
                "never-issued".to_string(),
                "approval".to_string(),
                "stale".to_string()
            ]
            .into(),
            "unknown, already-answered, and superseded keys are all ignored"
        );
        assert_eq!(applied.still_outstanding, ["region".to_string()].into());
        assert_eq!(
            store.get_task(&id).await.unwrap().unwrap().status,
            TaskStatus::InputRequired,
            "ignoring a stale update must not resume or fail the task"
        );
    }

    #[tokio::test]
    async fn reissued_key_becomes_a_fresh_question() {
        let store = MemoryTaskStore::new();
        let id = working_task(&store, None).await;
        store
            .require_input(&id, requests(&["approval"]), None)
            .await
            .unwrap();
        store
            .apply_input_responses(&id, [accept("approval")].into_iter().collect())
            .await
            .unwrap()
            .unwrap();

        // The server asks the same key again: the earlier answer must not
        // satisfy it.
        store
            .require_input(&id, requests(&["approval"]), None)
            .await
            .unwrap();
        let applied = store
            .apply_input_responses(&id, [accept("approval")].into_iter().collect())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(applied.accepted, ["approval".to_string()].into());
        assert!(applied.is_complete());
    }

    #[tokio::test]
    async fn failed_tasks_preserve_the_structured_error() {
        let store = MemoryTaskStore::new();
        let id = working_task(&store, None).await;

        let mut error = JsonRpcError::invalid_params("bad region");
        error.data = Some(serde_json::json!({"field": "region"}));
        assert!(store.fail_task(&id, error).await.unwrap());

        let (_, result, error) = store.get_task_result(&id).await.unwrap().unwrap();
        assert!(result.is_none());
        let error = error.expect("structured error must survive the store");
        assert_eq!(
            error.code, -32602,
            "the original code must not be flattened"
        );
        assert_eq!(error.message, "bad region");
        assert_eq!(error.data.unwrap()["field"], "region");
    }

    #[tokio::test]
    async fn tool_error_results_complete_the_task() {
        let store = MemoryTaskStore::new();
        let id = working_task(&store, None).await;

        let mut result = CallToolResult::text("domain failure");
        result.is_error = true;
        assert!(store.complete_task(&id, result).await.unwrap());

        let (task, result, error) = store.get_task_result(&id).await.unwrap().unwrap();
        assert_eq!(
            task.status,
            TaskStatus::Completed,
            "isError is a domain error, not an execution failure"
        );
        assert!(result.unwrap().is_error);
        assert!(error.is_none(), "no JSON-RPC error accompanies isError");
    }

    #[tokio::test]
    async fn tasks_record_their_creating_principal() {
        let store = MemoryTaskStore::new();
        let (owned, _) = store
            .create_task("tool", serde_json::json!({}), None, Some("alice".into()))
            .await
            .unwrap();
        let (unowned, _) = store
            .create_task("tool", serde_json::json!({}), None, None)
            .await
            .unwrap();

        assert_eq!(
            store.task_owner(&owned).await.unwrap(),
            Some(Some("alice".to_string()))
        );
        assert_eq!(store.task_owner(&unowned).await.unwrap(), Some(None));
        assert_eq!(
            store.task_owner("does-not-exist").await.unwrap(),
            None,
            "an unknown task has no owner record at all"
        );

        // Ownership is an authorization fact and must not reach the wire.
        let wire = serde_json::to_value(store.get_task(&owned).await.unwrap().unwrap()).unwrap();
        assert!(
            wire.get("owner").is_none(),
            "owner leaked to the wire: {wire}"
        );
        assert!(!wire.to_string().contains("alice"));
    }

    #[test]
    fn owner_matching_is_equality_not_leniency() {
        assert!(owner_matches(&None, None), "no auth configured");
        assert!(owner_matches(&Some("alice".into()), Some("alice")));

        assert!(
            !owner_matches(&Some("alice".into()), Some("bob")),
            "a different principal must not inherit the task"
        );
        assert!(
            !owner_matches(&Some("alice".into()), None),
            "dropping the token must not grant access"
        );
        assert!(
            !owner_matches(&None, Some("alice")),
            "an unowned task belongs to a different security context"
        );
    }

    #[tokio::test]
    async fn terminal_states_clear_outstanding_requests() {
        for (label, terminate) in [("completed", true), ("cancelled", false)] {
            let store = MemoryTaskStore::new();
            let id = working_task(&store, None).await;
            store
                .require_input(&id, requests(&["approval"]), None)
                .await
                .unwrap();

            if terminate {
                store
                    .complete_task(&id, CallToolResult::text("done"))
                    .await
                    .unwrap();
            } else {
                store.cancel_task(&id, None).await.unwrap();
            }

            assert!(
                store
                    .outstanding_input_requests(&id)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_empty(),
                "{label} task still advertises outstanding input requests"
            );
            assert!(
                store
                    .apply_input_responses(&id, [accept("approval")].into_iter().collect())
                    .await
                    .unwrap()
                    .is_none(),
                "{label} task accepted a late input response"
            );
        }
    }
}
