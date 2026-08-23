//! Async task management for long-running MCP operations
//!
//! This module provides task lifecycle management for operations that may take
//! longer than a typical request/response cycle. Legacy clients request task
//! augmentation explicitly; final-protocol servers elect tasks after extension
//! negotiation. Tasks can be tracked, polled, updated with input, and cancelled.
//!
//! Task state lives behind the pluggable [`TaskStore`] trait, mirroring the
//! shape of [`crate::session_store`] and [`crate::event_store`]: a trait, an
//! error enum, and an in-memory default. By default routers use
//! [`MemoryTaskStore`], which keeps tasks in an in-process map and
//! automatically signals and reclaims expired records. External stores
//! (Redis, Postgres, etc.) can be plugged in so `tasks/get` works on any
//! instance behind a load balancer in the sessionless 2026-07-28 flows
//! (SEP-2663).
//!
//! Nothing here is advertised until a server asks for it:
//! [`McpRouter::with_tasks`](crate::McpRouter::with_tasks) is the runtime
//! opt-in, and a client that did not declare the extension keeps receiving
//! ordinary synchronous results. See `examples/tasks.rs` for a runnable
//! server.
//!
//! # Lifecycle
//!
//! A task exists independently of the request that created it. The
//! `tools/call` response carries the task in place of the tool's result, and
//! every later operation names the task by its ID rather than by connection or
//! session. That is what lets a task outlive its transport, and what makes an
//! external store the requirement for more than one server instance.
//!
//! | Status | Reached by | Terminal |
//! |----------------|--------------------------------------------------------|-----|
//! | `working`      | creation, and again once every input request is answered | no  |
//! | `input_required` | [`TaskStore::require_input`]                          | no  |
//! | `completed`    | [`TaskStore::complete_task`]                            | yes |
//! | `failed`       | [`TaskStore::fail_task`]                                | yes |
//! | `cancelled`    | [`TaskStore::cancel_task`]                              | yes |
//!
//! Terminal states are immutable. A transition method answers `Ok(false)`
//! rather than an error when the task is already terminal, unknown, or
//! expired, so a handler finishing just after a cancellation is dropped
//! instead of overwriting the recorded outcome.
//!
//! Which terminal state a finished tool call reaches is the distinction to get
//! right:
//!
//! - A [`CallToolResult`] with `is_error: true` **completes** the task. The
//!   tool ran and reported a domain error, which is an answer the caller asked
//!   for, and `tasks/get` returns it in the result field exactly as the
//!   synchronous call would have. SEP-2663 keeps that separate from failure.
//! - `failed` carries a [`JsonRpcError`] and no result, meaning the call never
//!   produced one. The router uses it when the task machinery itself gives
//!   out: a park that cannot take, a store that cannot resume, a tool
//!   deregistered while its task waited.
//!
//! A tool handler returning `Err` lands in the first category, not the second:
//! the router converts it to an `is_error` result, so the task completes. A
//! server that wants a `failed` task drives [`TaskStore::fail_task`] itself.
//!
//! `ttlMs` runs from creation rather than from the terminal transition, so a
//! task can expire while still working. Past that point every read returns
//! `None` and every transition returns `Ok(false)`, whether or not the entry
//! has been reclaimed: an expired task is indistinguishable from one that
//! never existed. Reaching the deadline also raises the task's
//! [`CancellationToken`] and wakes completion/input waiters, so invisible work
//! is not left suspended. [`MemoryTaskStore`] schedules this automatically;
//! external stores must bridge their expiry mechanism to the returned token.
//!
//! ```rust
//! use tower_mcp::CallToolResult;
//! use tower_mcp::async_task::{MemoryTaskStore, TaskStore};
//! use tower_mcp::protocol::TaskStatus;
//!
//! # #[tokio::main]
//! # async fn main() {
//! let store = MemoryTaskStore::new();
//! let (id, _cancel) = store
//!     .create_task("build_report", serde_json::json!({"rows": 10}), None, None)
//!     .await
//!     .unwrap();
//!
//! assert_eq!(
//!     store.get_task(&id).await.unwrap().unwrap().status,
//!     TaskStatus::Working
//! );
//!
//! assert!(
//!     store
//!         .complete_task(&id, CallToolResult::text("report ready"))
//!         .await
//!         .unwrap()
//! );
//!
//! let (task, result, error) = store.get_task_result(&id).await.unwrap().unwrap();
//! assert_eq!(task.status, TaskStatus::Completed);
//! assert_eq!(result.unwrap().all_text(), "report ready");
//! assert!(error.is_none());
//!
//! // Terminal is final: a late transition is reported as not applied rather
//! // than rewriting the outcome a client may already have read.
//! assert!(
//!     !store
//!         .complete_task(&id, CallToolResult::text("stale"))
//!         .await
//!         .unwrap()
//! );
//! # }
//! ```
//!
//! # Waiting on the client
//!
//! A tool handler that needs something from the client returns an
//! input-required outcome instead of a result. The router parks the task by
//! calling [`TaskStore::require_input`] with the keyed requests, and the task
//! sits in `input_required` until the client answers with `tasks/update`.
//!
//! The client answers the *task*, not the original call, so there is no
//! `tools/call` for it to retry and the server performs the retry itself. Once
//! [`TaskStore::apply_input_responses`] reports nothing outstanding, the router
//! reads [`TaskStore::resume_context`] and invokes the handler again from the
//! top, with the accumulated answers reaching it through the request context
//! exactly as a client retry would have delivered them. A handler is free to
//! ask again; each round parks and resumes the same way.
//!
//! [`TaskStore::resume_context`] has a default returning `None` so that stores
//! written before resumption existed keep compiling. The router treats that as
//! "this store cannot resume" and fails the task with a message saying so,
//! rather than leaving it working forever (#1208).
//!
//! Two rules make the exchange safe to replay:
//!
//! - **A request key is unique over a task's lifetime.** Once answered or
//!   superseded it is spent, and reissuing it is a
//!   [`TaskStoreError::InvalidTransition`]. Reissuing a key that is still
//!   outstanding, still naming the same question, is not reuse: `requests`
//!   replaces the whole outstanding set, so carrying a key forward is how it
//!   stays outstanding (#1246).
//! - **Response keys that are not outstanding are ignored.** Unknown,
//!   already-answered, and superseded keys land in
//!   [`AppliedInputResponses::ignored`] instead of failing the update, so a
//!   client replaying a stale `tasks/update` neither breaks the task nor
//!   resumes it early.
//!
//! # Authorization
//!
//! SEP-2663 requires servers to authorize every task request, and warns that a
//! task ID can act as a bearer token: whoever holds it can poll, update, or
//! cancel the task. This module answers that in two layers.
//!
//! [`generate_task_id`] draws 128 bits from the system CSPRNG, so IDs cannot
//! be enumerated or guessed. That only protects IDs nobody has seen, so each
//! task also records the principal that created it (see [`TaskOwner`]), and
//! every later operation must match under [`owner_matches`].
//!
//! Matching is equality, not "protect owned tasks and leave unowned ones
//! open":
//!
//! | Task owner | Caller  | Result                                |
//! |------------|---------|---------------------------------------|
//! | none       | none    | allowed, no authentication configured |
//! | `alice`    | `alice` | allowed                               |
//! | `alice`    | `bob`   | denied                                |
//! | `alice`    | none    | denied                                |
//! | none       | `alice` | denied                                |
//!
//! The last row is deliberate. An unowned task can only exist if it was
//! created with no authenticated context, so a request that now carries a
//! principal is a different security context rather than an upgrade of the
//! same one. Servers mixing public and authenticated paths (see
//! [`OAuthLayer::public_path`](crate::oauth::OAuthLayer::public_path), or
//! routing them around the layer entirely) should expect a task created
//! anonymously to be unreachable once a token is presented.
//!
//! The principal comes from the OAuth `sub` claim that the HTTP and WebSocket
//! transports bridge into request extensions. Without the `oauth` feature
//! there is no principal, so every task is unowned and servers with no
//! authentication behave as they did before ownership existed.
//!
//! ## Why a denial looks like a missing task
//!
//! A refused operation returns exactly what an unknown task returns: `-32602`
//! with "Task not found".
//!
//! SEP-2663 mandates `-32602` for an invalid or nonexistent task ID, but
//! leaves the authorization failure to the server: tasks should be bound to
//! "some sort of authorization context, the implementation of which is left to
//! individual servers according to their existing bespoke permission models".
//! Reusing `-32602` is therefore tower-mcp policy, not a spec requirement.
//!
//! The reasoning is that answering "forbidden" would confirm the ID is real,
//! which is what unguessable IDs exist to prevent. The same SEP notes that
//! where binding is impossible "the task ID becomes the only line of defense
//! against contamination". A server that prefers a distinguishable error can
//! wrap the router and translate.
//!
//! Expiry follows the same rule: [`Task::is_expired`] runs from creation, and
//! an expired task reads as absent rather than as expired, so a retention
//! window cannot be probed either.
//!
//! # Status notifications
//!
//! A client may watch a task instead of polling it, by naming its ID in the
//! `taskIds` filter of a `subscriptions/listen` stream. Each
//! `notifications/tasks` carries the complete task, identical to the
//! `tasks/get` response at that moment, so a client that hears about a
//! completion already holds the result.
//!
//! The router announces the transitions it drives. A server that drives one
//! itself, most commonly [`TaskStore::require_input`], announces it with
//! [`McpRouter::notify_task_status_changed`](crate::McpRouter::notify_task_status_changed).
//!
//! Notifications are best effort and `tasks/get` stays authoritative: a task
//! outlives the request that created it, so there may be no subscriber at the
//! moment a transition happens, and a client that missed one loses nothing but
//! time.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::io;
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::error::JsonRpcError;
use crate::protocol::{CallToolResult, InputRequests, InputResponses, TaskObject, TaskStatus};

/// Cancellation signal shared by a task store and the work it owns.
///
/// Cloned tokens share the same underlying signal: cancelling any clone
/// cancels them all. The token is backed by
/// [`tokio_util::sync::CancellationToken`], so it can be checked synchronously
/// or awaited. A store must raise the same signal for explicit cancellation
/// and expiry; see [`TaskStore::create_task`].
///
/// This task-lifecycle token is intentionally a separate type from
/// [`crate::context::CancellationToken`], which belongs to the originating
/// request.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    inner: tokio_util::sync::CancellationToken,
}

impl CancellationToken {
    /// Create a new, un-cancelled token.
    ///
    /// [`TaskStore::create_task`] has to return one of these, so the public
    /// constructor lets external task stores create the process-local signal
    /// they bridge to durable cancellation and expiry state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Check whether cancellation or expiry has been signalled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Signal cancellation or expiry to every clone and waiter.
    pub fn cancel(&self) {
        self.inner.cancel();
    }

    /// Wait until cancellation or expiry is signalled.
    ///
    /// Completes immediately if the token was already signalled.
    pub async fn cancelled(&self) {
        self.inner.cancelled().await;
    }
}

/// Default time-to-live for a task (5 minutes).
///
/// Per SEP-2663 the TTL runs from task creation, not from the moment the task
/// reaches a terminal state.
const DEFAULT_TASK_TTL: Duration = Duration::from_secs(5 * 60);

/// Default interval between physical reclamation passes.
const DEFAULT_TASK_CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

/// Default maximum number of task records retained by [`MemoryTaskStore`].
const DEFAULT_MAX_RETAINED_TASKS: usize = 1_024;

/// Default maximum compact-JSON size of one accepted task payload (4 MiB).
const DEFAULT_MAX_TASK_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// Default maximum compact-JSON charge retained across all tasks (64 MiB).
const DEFAULT_MAX_RETAINED_TASK_BYTES: usize = 64 * 1024 * 1024;

/// Default poll interval suggestion (2 seconds, in milliseconds)
const DEFAULT_POLL_INTERVAL_MS: u64 = 2_000;

/// Byte and record limits for [`MemoryTaskStore`].
///
/// Defaults are deliberately finite: 1,024 retained task records, 4 MiB for
/// any one accepted identity, arguments, metadata, input, status-message,
/// result, or error payload, and 64 MiB of aggregate retained payload charge. Use
/// [`unbounded`](Self::unbounded) only when a host provides an equivalent
/// bound outside this store.
///
/// Byte sizes are the compact JSON encoding produced by `serde_json`. The
/// aggregate charge covers the retained tool name, owner, arguments,
/// metadata, status message, input requests and their spent keys,
/// accumulated input responses, result, and structured error. It deliberately
/// excludes `HashMap` allocation overhead and fixed lifecycle machinery such
/// as IDs, timestamps, cancellation tokens, and waiter handles; the task-count
/// limit bounds that fixed per-record overhead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct TaskRetentionLimits {
    /// Maximum task records physically retained, including expired tombstones.
    pub max_tasks: usize,
    /// Maximum compact-JSON bytes for one accepted payload.
    pub max_payload_bytes: usize,
    /// Maximum aggregate retained and reserved compact-JSON bytes.
    pub max_retained_bytes: usize,
}

impl TaskRetentionLimits {
    /// Create the default finite retention policy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Disable the in-memory store's count and byte limits.
    ///
    /// Prefer the finite [`Default`] unless another layer enforces equivalent
    /// process-memory limits.
    #[must_use]
    pub const fn unbounded() -> Self {
        Self {
            max_tasks: usize::MAX,
            max_payload_bytes: usize::MAX,
            max_retained_bytes: usize::MAX,
        }
    }

    /// Set the maximum number of physically retained task records.
    #[must_use]
    pub const fn max_tasks(mut self, max: usize) -> Self {
        self.max_tasks = max;
        self
    }

    /// Set the compact-JSON limit for one accepted payload.
    #[must_use]
    pub const fn max_payload_bytes(mut self, max: usize) -> Self {
        self.max_payload_bytes = max;
        self
    }

    /// Set the aggregate retained-and-reserved compact-JSON byte limit.
    #[must_use]
    pub const fn max_retained_bytes(mut self, max: usize) -> Self {
        self.max_retained_bytes = max;
        self
    }
}

impl Default for TaskRetentionLimits {
    fn default() -> Self {
        Self {
            max_tasks: DEFAULT_MAX_RETAINED_TASKS,
            max_payload_bytes: DEFAULT_MAX_TASK_PAYLOAD_BYTES,
            max_retained_bytes: DEFAULT_MAX_RETAINED_TASK_BYTES,
        }
    }
}

/// Content-free resource gauges for [`MemoryTaskStore`].
///
/// `reserved_bytes` is headroom held for replacing every live record with a
/// small, fixed retention-limit failure. The aggregate quota applies to
/// `retained_bytes + reserved_bytes`; the split lets operators distinguish
/// actual stored payload from safety headroom without exposing task contents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TaskStoreUsage {
    /// Number of task records physically retained, including tombstones.
    pub task_count: usize,
    /// Compact-JSON bytes currently charged to retained task payloads.
    pub retained_bytes: usize,
    /// Bytes reserved for bounded terminal retention failures.
    pub reserved_bytes: usize,
}

impl TaskStoreUsage {
    /// Aggregate bytes charged against [`TaskRetentionLimits::max_retained_bytes`].
    ///
    /// Snapshots returned by [`MemoryTaskStore::usage`] cannot overflow. This
    /// accessor saturates only if a caller manually mutates the public gauge
    /// fields into a value no store can produce.
    #[must_use]
    pub fn charged_bytes(self) -> usize {
        self.retained_bytes.saturating_add(self.reserved_bytes)
    }
}

/// Runtime policy for the in-memory task store.
///
/// The default task TTL remains five minutes for source and behavior
/// compatibility. Final-protocol clients cannot choose a TTL, so
/// [`default_ttl`](Self::default_ttl) is the server's retention and execution
/// bound for those tasks. Legacy clients that send an explicit TTL continue
/// to use that value.
///
/// Expiry signalling is scheduled independently of
/// [`cleanup_interval`](Self::cleanup_interval). The interval controls only
/// when expired records are physically removed from memory; at the TTL
/// deadline the task has already become invisible and its cancellation token
/// and completion waiters have already been woken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryTaskStoreConfig {
    /// TTL used when [`TaskStore::create_task`] receives `None`.
    pub default_ttl: Duration,
    /// Interval between automatic physical cleanup passes.
    pub cleanup_interval: Duration,
}

impl MemoryTaskStoreConfig {
    /// Create the default five-minute TTL, one-minute cleanup policy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the TTL used for tasks that do not provide one.
    #[must_use]
    pub fn default_ttl(mut self, ttl: Duration) -> Self {
        self.default_ttl = ttl;
        self
    }

    /// Set how frequently expired task records are physically reclaimed.
    ///
    /// A zero interval is accepted and treated as one millisecond to avoid a
    /// busy cleanup loop.
    #[must_use]
    pub fn cleanup_interval(mut self, interval: Duration) -> Self {
        self.cleanup_interval = interval;
        self
    }
}

impl Default for MemoryTaskStoreConfig {
    fn default() -> Self {
        Self {
            default_ttl: DEFAULT_TASK_TTL,
            cleanup_interval: DEFAULT_TASK_CLEANUP_INTERVAL,
        }
    }
}

fn duration_millis_saturated(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Internal task representation with full state
///
/// The record [`MemoryTaskStore`] keeps, exposed because its fields describe
/// what a store has to track. There is no public constructor: tasks come from
/// [`TaskStore::create_task`], and an external store is free to persist an
/// entirely different shape as long as it answers the trait's methods the same
/// way.
#[derive(Debug, Clone)]
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
    /// Time-to-live in milliseconds, measured from creation
    pub ttl: u64,
    /// Suggested polling interval in milliseconds
    pub poll_interval: u64,
    /// Human-readable status message
    pub status_message: Option<String>,
    /// Protocol metadata retained across every task view.
    pub meta: Option<serde_json::Value>,
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
    /// The answers themselves, accumulated across every `tasks/update`.
    ///
    /// A task's client answers through `tasks/update` rather than by retrying
    /// `tools/call`, so the server owns resumption and must keep the values
    /// to hand back to the resumed handler. Recording only the keys made the
    /// answers unrecoverable (#1208).
    pub input_responses: InputResponses,
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
        ttl: u64,
        owner: TaskOwner,
    ) -> Self {
        let now_str = chrono_now_iso8601();
        Self {
            id,
            tool_name,
            arguments,
            status: TaskStatus::Working,
            created_at: Instant::now(),
            created_at_str: now_str.clone(),
            last_updated_at_str: now_str,
            ttl,
            poll_interval: DEFAULT_POLL_INTERVAL_MS,
            status_message: Some("Task started".to_string()),
            meta: None,
            result: None,
            error: None,
            owner,
            input_requests: InputRequests::new(),
            answered_input_keys: BTreeSet::new(),
            input_responses: InputResponses::new(),
            superseded_input_keys: BTreeSet::new(),
            cancellation_token: CancellationToken::new(),
            completed_at: None,
            completion_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Convert to TaskObject for API responses
    ///
    /// The `result` and `error` fields are deliberately left empty. A task
    /// object travels in status responses, where the payload is not wanted;
    /// [`TaskStore::get_task_result`] is what pairs the object with whichever
    /// of the two the task actually holds.
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
            meta: self.meta.clone(),
        }
    }

    /// Check if this task should be cleaned up (TTL expired).
    ///
    /// The clock runs from creation, per SEP-2663. A long-running task can
    /// therefore expire while still working, which is the intended behavior:
    /// `ttlMs` bounds how long the server retains the task, not how long it
    /// lingers after finishing.
    pub fn is_expired(&self) -> bool {
        self.is_expired_at(Instant::now())
    }

    fn expires_at(&self) -> Option<Instant> {
        self.created_at.checked_add(Duration::from_millis(self.ttl))
    }

    fn is_expired_at(&self, now: Instant) -> bool {
        self.expires_at().is_some_and(|deadline| now >= deadline)
    }

    /// Outstanding input requests, if the task is waiting on the client.
    pub fn outstanding_input_requests(&self) -> &InputRequests {
        &self.input_requests
    }

    /// Check if the task has been cancelled
    pub fn is_cancelled(&self) -> bool {
        self.cancellation_token.is_cancelled()
    }

    /// Replace all application payload with one fixed, content-free failure.
    fn fail_retention_limit(&mut self) {
        self.arguments = serde_json::Value::Null;
        self.status = TaskStatus::Failed;
        self.status_message = Some(RETENTION_FAILURE_STATUS.to_string());
        self.meta = None;
        self.result = None;
        self.error = Some(JsonRpcError::internal_error(RETENTION_FAILURE_MESSAGE));
        self.input_requests = InputRequests::new();
        self.answered_input_keys = BTreeSet::new();
        self.input_responses = InputResponses::new();
        self.superseded_input_keys = BTreeSet::new();
        self.completed_at = Some(Instant::now());
        self.last_updated_at_str = chrono_now_iso8601();
    }
}

/// Memory-store-only lifecycle bookkeeping around the public task record.
///
/// Keeping expiry signalling state here avoids adding a field to [`Task`],
/// whose public fields historically allowed downstream struct literals.
#[derive(Debug, Clone)]
struct StoredTask {
    task: Task,
    expiry_signalled: bool,
    retained_bytes: usize,
    reserved_bytes: usize,
}

impl StoredTask {
    fn new(task: Task, retained_bytes: usize, reserved_bytes: usize) -> Self {
        Self {
            task,
            expiry_signalled: false,
            retained_bytes,
            reserved_bytes,
        }
    }

    /// Raise the persistent expiry signals exactly once.
    fn signal_expiry(&mut self) -> bool {
        if self.expiry_signalled {
            return false;
        }
        self.expiry_signalled = true;
        self.task.cancellation_token.cancel();
        self.task.completion_notify.notify_waiters();
        true
    }

    /// Drop payload allocations that can no longer be observed after expiry.
    fn scrub_expired_payload(&mut self) {
        self.task.tool_name = String::new();
        self.task.arguments = serde_json::Value::Null;
        self.task.status_message = None;
        self.task.meta = None;
        self.task.result = None;
        self.task.error = None;
        self.task.input_requests = InputRequests::new();
        self.task.answered_input_keys = BTreeSet::new();
        self.task.input_responses = InputResponses::new();
        self.task.superseded_input_keys = BTreeSet::new();
        self.reserved_bytes = 0;
    }
}

impl std::ops::Deref for StoredTask {
    type Target = Task;

    fn deref(&self) -> &Self::Target {
        &self.task
    }
}

impl std::ops::DerefMut for StoredTask {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.task
    }
}

fn charged_bytes(retained: usize, reserved: usize, limit: usize) -> Result<usize> {
    retained
        .checked_add(reserved)
        .filter(|charged| *charged <= limit)
        .ok_or(TaskStoreError::RetentionLimitExceeded {
            kind: TaskRetentionLimitKind::AggregateBytes,
            limit,
        })
}

fn prepare_stored_task(task: Task, limits: TaskRetentionLimits) -> Result<StoredTask> {
    let retained_bytes = retained_payload_size(&task, limits.max_retained_bytes)?;
    let reserved_bytes = if task.status.is_terminal() {
        0
    } else {
        // The candidate is already bounded before this clone is made. Keeping
        // exact headroom for its fixed failure replacement means an oversized
        // terminal payload can always produce a visible, wakeable outcome.
        let mut fallback = task.clone();
        fallback.fail_retention_limit();
        let fallback_bytes = retained_payload_size(&fallback, limits.max_retained_bytes)?;
        fallback_bytes.saturating_sub(retained_bytes)
    };
    charged_bytes(retained_bytes, reserved_bytes, limits.max_retained_bytes)?;
    Ok(StoredTask::new(task, retained_bytes, reserved_bytes))
}

fn replace_stored_task(
    data: &mut MemoryTaskStoreData,
    task_id: &str,
    replacement: StoredTask,
    limit: usize,
) -> Result<bool> {
    let Some(current) = data.tasks.get(task_id) else {
        return Ok(false);
    };
    let (retained_bytes, reserved_bytes) = replacement_totals(
        data,
        current.retained_bytes,
        current.reserved_bytes,
        replacement.retained_bytes,
        replacement.reserved_bytes,
        limit,
    )?;

    data.retained_bytes = retained_bytes;
    data.reserved_bytes = reserved_bytes;
    data.tasks.insert(task_id.to_string(), replacement);
    Ok(true)
}

fn replacement_totals(
    data: &MemoryTaskStoreData,
    old_retained: usize,
    old_reserved: usize,
    new_retained: usize,
    new_reserved: usize,
    limit: usize,
) -> Result<(usize, usize)> {
    let retained_bytes = data
        .retained_bytes
        .checked_sub(old_retained)
        .and_then(|bytes| bytes.checked_add(new_retained))
        .ok_or(TaskStoreError::RetentionLimitExceeded {
            kind: TaskRetentionLimitKind::AggregateBytes,
            limit,
        })?;
    let reserved_bytes = data
        .reserved_bytes
        .checked_sub(old_reserved)
        .and_then(|bytes| bytes.checked_add(new_reserved))
        .ok_or(TaskStoreError::RetentionLimitExceeded {
            kind: TaskRetentionLimitKind::AggregateBytes,
            limit,
        })?;
    charged_bytes(retained_bytes, reserved_bytes, limit)?;
    Ok((retained_bytes, reserved_bytes))
}

fn commit_removed_task(
    data: &mut MemoryTaskStoreData,
    task_id: &str,
    replacement: StoredTask,
    old_retained: usize,
    old_reserved: usize,
    limit: usize,
) -> Result<()> {
    let (retained_bytes, reserved_bytes) = replacement_totals(
        data,
        old_retained,
        old_reserved,
        replacement.retained_bytes,
        replacement.reserved_bytes,
        limit,
    )?;
    data.retained_bytes = retained_bytes;
    data.reserved_bytes = reserved_bytes;
    data.tasks.insert(task_id.to_string(), replacement);
    Ok(())
}

fn commit_retention_failure(
    data: &mut MemoryTaskStoreData,
    task_id: &str,
    mut task: StoredTask,
    old_retained: usize,
    old_reserved: usize,
    limits: TaskRetentionLimits,
) {
    task.task.fail_retention_limit();
    task.retained_bytes = retained_payload_size(&task.task, limits.max_retained_bytes)
        .expect("reserved retention failure must fit the aggregate byte limit");
    task.reserved_bytes = 0;
    let notify = task.completion_notify.clone();
    commit_removed_task(
        data,
        task_id,
        task,
        old_retained,
        old_reserved,
        limits.max_retained_bytes,
    )
    .expect("reserved retention failure must fit global task-store accounting");
    notify.notify_waiters();
}

fn cancel_with_bounded_status(task: &mut Task, status: &'static str, scrub: bool) {
    if scrub {
        task.arguments = serde_json::Value::Null;
        task.meta = None;
        task.result = None;
        task.error = None;
        task.answered_input_keys = BTreeSet::new();
        task.input_responses = InputResponses::new();
        task.superseded_input_keys = BTreeSet::new();
    }
    task.input_requests = InputRequests::new();
    task.status = TaskStatus::Cancelled;
    task.status_message = Some(status.to_string());
    task.completed_at = Some(Instant::now());
    task.last_updated_at_str = chrono_now_iso8601();
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
///
/// ```rust
/// use tower_mcp::async_task::owner_matches;
///
/// assert!(owner_matches(&None, None), "no authentication configured");
/// assert!(owner_matches(&Some("alice".into()), Some("alice")));
///
/// assert!(!owner_matches(&Some("alice".into()), Some("bob")));
/// assert!(
///     !owner_matches(&Some("alice".into()), None),
///     "dropping the token does not grant access"
/// );
/// assert!(
///     !owner_matches(&None, Some("alice")),
///     "a task created anonymously is unreachable once a token is presented"
/// );
/// ```
pub fn owner_matches(owner: &TaskOwner, principal: Option<&str>) -> bool {
    owner.as_deref() == principal
}

/// Outcome of applying `tasks/update.inputResponses` to a task.
///
/// SEP-2663 requires partial responses to be honored: keys that match an
/// outstanding request are consumed, everything else is ignored rather than
/// rejected, and any request left unanswered stays outstanding.
///
/// Returned by [`TaskStore::apply_input_responses`], which carries the worked
/// example.
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

/// What a resumed task needs to run its handler again.
///
/// The client answers a task through `tasks/update`, not by retrying
/// `tools/call`, so the server re-invokes the handler itself and must supply
/// what the client would otherwise have resent.
///
/// The handler runs from the top rather than continuing where it stopped, so
/// the arguments are the original ones, unmodified, and `input_responses`
/// accumulates across every round rather than holding only the latest answer.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TaskResumeContext {
    /// Tool to re-invoke.
    pub tool_name: String,
    /// The original call arguments, unchanged.
    pub arguments: serde_json::Value,
    /// Every answer accumulated so far, keyed as the requests were issued.
    pub input_responses: InputResponses,
    /// Cancellation and expiry signal for the resumed execution, when the
    /// store attached one.
    ///
    /// External stores should provide a clone of the token returned by
    /// [`TaskStore::create_task`], or another token wired to the same durable
    /// cancellation and expiry source, via
    /// [`with_cancellation_token`](Self::with_cancellation_token).
    /// The router fails the replay loudly when this is `None`, because a
    /// disconnected handler could otherwise run beyond the task deadline.
    pub cancellation_token: Option<CancellationToken>,
}

impl TaskResumeContext {
    /// Create the context needed to resume a task handler.
    ///
    /// External [`TaskStore`] implementations use this when reconstructing a
    /// task from durable state in [`TaskStore::resume_context`]. This keeps
    /// the original three-argument constructor source-compatible, but leaves
    /// [`cancellation_token`](Self::cancellation_token) as `None`; attach the
    /// task's lifecycle token with
    /// [`with_cancellation_token`](Self::with_cancellation_token) before
    /// returning it to the router.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::async_task::TaskResumeContext;
    ///
    /// let resume = TaskResumeContext::new(
    ///     "build_report",
    ///     serde_json::json!({"format": "pdf"}),
    ///     Default::default(),
    /// );
    ///
    /// assert_eq!(resume.tool_name, "build_report");
    /// ```
    #[must_use]
    pub fn new(
        tool_name: impl Into<String>,
        arguments: serde_json::Value,
        input_responses: InputResponses,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            arguments,
            input_responses,
            cancellation_token: None,
        }
    }

    /// Attach the cancellation and expiry signal for replayed execution.
    ///
    /// [`Self::new`] intentionally keeps its original three arguments for
    /// source compatibility. Stores that support resumption should call this
    /// builder with the token associated with the task so expiry can stop a
    /// replayed handler even when that handler does not poll cooperatively.
    #[must_use]
    pub fn with_cancellation_token(mut self, token: CancellationToken) -> Self {
        self.cancellation_token = Some(token);
        self
    }
}

impl AppliedInputResponses {
    /// Whether every outstanding request has now been answered.
    pub fn is_complete(&self) -> bool {
        self.still_outstanding.is_empty()
    }
}

/// Whether a task is present, expired, or was never known.
///
/// `Option` collapses the last two, so an application retaining expired
/// tombstones cannot tell a caller "that task expired" rather than "no such
/// task" (#1249).
///
/// # The owner is carried for a reason
///
/// `Expired` keeps the owner because authorization has to happen *before* the
/// distinction is revealed. A present or expired task belonging to another
/// principal must remain indistinguishable from `Missing` to that caller, or
/// `tasks/get` becomes an existence oracle: anyone could probe ids and learn
/// which ones belong to somebody. Only the matching owner sees `Expired`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TaskPresence {
    /// The task exists and has not expired.
    Present {
        /// Principal that created it.
        owner: TaskOwner,
    },
    /// The task existed and its TTL elapsed.
    ///
    /// Only reported by a store that retains expired records. One that drops
    /// them answers `Missing`, which is correct for it.
    Expired {
        /// Principal that created it, needed to authorize before disclosing.
        owner: TaskOwner,
    },
    /// No such task, or the store no longer retains it.
    Missing,
}

impl TaskPresence {
    /// The owner, for a task the store still knows about.
    pub fn owner(&self) -> Option<&TaskOwner> {
        match self {
            Self::Present { owner } | Self::Expired { owner } => Some(owner),
            Self::Missing => None,
        }
    }

    /// Whether the store knows this task at all, expired or not.
    pub fn is_known(&self) -> bool {
        !matches!(self, Self::Missing)
    }
}

/// Errors returned by [`TaskStore`] implementations.
///
/// Encode and decode errors come from (de)serializing task state, and
/// [`Backend`](Self::Backend) is the catch-all for the storage layer. Those
/// three mirror
/// [`SessionStoreError`](crate::session_store::SessionStoreError) and exist
/// for external implementations; [`MemoryTaskStore`] never returns them.
///
/// [`InvalidTransition`](Self::InvalidTransition) is different in kind. It
/// reports that the requested change is not legal for the task's current
/// state, which is deterministic and says nothing about storage health, so a
/// caller must not treat it as retryable. [`MemoryTaskStore`] does return it
/// (#1246).
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
    /// The requested change is not valid for the task's current state.
    ///
    /// Deterministic: the same call fails the same way, so retrying cannot
    /// help and callers should not confuse it with an infrastructure
    /// failure. Reusing an input request key is the current instance (#1246).
    #[error("invalid task transition: {0}")]
    InvalidTransition(String),
    /// A configured task-retention limit rejected a mutation.
    ///
    /// The error carries only the limit category and numeric bound. It never
    /// includes the rejected arguments, input, result, or error payload, so it
    /// is safe to map, log, or expose through a host's error policy.
    #[error("task retention {kind} limit exceeded (maximum {limit})")]
    RetentionLimitExceeded {
        /// Limit that rejected the operation.
        kind: TaskRetentionLimitKind,
        /// Configured maximum for that limit.
        limit: usize,
    },
}

/// Which [`TaskRetentionLimits`] bound rejected a store operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TaskRetentionLimitKind {
    /// Maximum physically retained task count.
    TaskCount,
    /// Maximum compact-JSON size of one accepted payload.
    PayloadBytes,
    /// Maximum aggregate retained and reserved compact-JSON bytes.
    AggregateBytes,
}

impl std::fmt::Display for TaskRetentionLimitKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TaskCount => "task-count",
            Self::PayloadBytes => "payload-bytes",
            Self::AggregateBytes => "aggregate-bytes",
        })
    }
}

/// Whether two input requests are the same question.
///
/// [`InputRequest`](crate::protocol::InputRequest) is `#[non_exhaustive]` and
/// carries params that do not implement `Eq`, so this compares their
/// serialized forms. A request that cannot be serialized is treated as
/// changed, which errs toward reporting reuse rather than silently accepting
/// a second question under a spent key.
fn same_input_request(
    a: &crate::protocol::InputRequest,
    b: &crate::protocol::InputRequest,
) -> bool {
    match (serde_json::to_value(a), serde_json::to_value(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Result alias for task store operations.
pub type Result<T> = std::result::Result<T, TaskStoreError>;

const RETENTION_FAILURE_MESSAGE: &str = "Task payload exceeded configured retention limits";
const RETENTION_FAILURE_STATUS: &str = "Task failed: retention limit exceeded";

/// Writer that counts serialized bytes without allocating a second buffer.
struct CountingWriter {
    written: usize,
    limit: usize,
    exceeded: bool,
}

impl CountingWriter {
    fn new(limit: usize) -> Self {
        Self {
            written: 0,
            limit,
            exceeded: false,
        }
    }
}

impl io::Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(next) = self.written.checked_add(bytes.len()) else {
            self.exceeded = true;
            return Err(io::Error::other("encoded task payload exceeds byte limit"));
        };
        if next > self.limit {
            self.exceeded = true;
            return Err(io::Error::other("encoded task payload exceeds byte limit"));
        }
        self.written = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encoded_size<T: serde::Serialize + ?Sized>(
    value: &T,
    limit: usize,
    kind: TaskRetentionLimitKind,
) -> Result<usize> {
    let mut writer = CountingWriter::new(limit);
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) => Ok(writer.written),
        Err(_) if writer.exceeded => Err(TaskStoreError::RetentionLimitExceeded { kind, limit }),
        Err(error) => Err(TaskStoreError::Encode(error.to_string())),
    }
}

#[derive(serde::Serialize)]
struct RetainedTaskPayload<'a> {
    tool_name: &'a str,
    arguments: &'a serde_json::Value,
    status_message: &'a Option<String>,
    meta: &'a Option<serde_json::Value>,
    result: &'a Option<CallToolResult>,
    error: &'a Option<JsonRpcError>,
    owner: &'a TaskOwner,
    input_requests: &'a InputRequests,
    answered_input_keys: &'a BTreeSet<String>,
    input_responses: &'a InputResponses,
    superseded_input_keys: &'a BTreeSet<String>,
}

fn retained_payload_size(task: &Task, limit: usize) -> Result<usize> {
    encoded_size(
        &RetainedTaskPayload {
            tool_name: &task.tool_name,
            arguments: &task.arguments,
            status_message: &task.status_message,
            meta: &task.meta,
            result: &task.result,
            error: &task.error,
            owner: &task.owner,
            input_requests: &task.input_requests,
            answered_input_keys: &task.answered_input_keys,
            input_responses: &task.input_responses,
            superseded_input_keys: &task.superseded_input_keys,
        },
        limit,
        TaskRetentionLimitKind::AggregateBytes,
    )
}

fn validate_payload<T: serde::Serialize + ?Sized>(
    payload: &T,
    limits: TaskRetentionLimits,
) -> Result<usize> {
    let (limit, kind) = if limits.max_payload_bytes <= limits.max_retained_bytes {
        (
            limits.max_payload_bytes,
            TaskRetentionLimitKind::PayloadBytes,
        )
    } else {
        (
            limits.max_retained_bytes,
            TaskRetentionLimitKind::AggregateBytes,
        )
    };
    encoded_size(payload, limit, kind)
}

fn validate_prefixed_string(
    value: &str,
    prefix: &str,
    limits: TaskRetentionLimits,
) -> Result<usize> {
    fn check(
        value: &str,
        prefix: &str,
        limit: usize,
        kind: TaskRetentionLimitKind,
    ) -> Result<usize> {
        let Some(value_limit) = limit.checked_sub(prefix.len()) else {
            return Err(TaskStoreError::RetentionLimitExceeded { kind, limit });
        };
        encoded_size(value, value_limit, kind)?
            .checked_add(prefix.len())
            .ok_or(TaskStoreError::RetentionLimitExceeded { kind, limit })
    }

    let (limit, kind) = if limits.max_payload_bytes <= limits.max_retained_bytes {
        (
            limits.max_payload_bytes,
            TaskRetentionLimitKind::PayloadBytes,
        )
    } else {
        (
            limits.max_retained_bytes,
            TaskRetentionLimitKind::AggregateBytes,
        )
    };
    check(value, prefix, limit, kind)
}

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
/// - [`cancel_task`](Self::cancel_task) and TTL expiry must signal the task's
///   [`CancellationToken`] even if the task is already terminal. The token is
///   a persistent signal and may be awaited, so it must be the same
///   cancellation domain returned from creation and carried through
///   [`TaskResumeContext`].
/// - [`wait_for_completion`](Self::wait_for_completion) blocks until the task
///   reaches a terminal state or expires. Expiry wakes an existing waiter,
///   which then returns `None`; how an implementation waits (notification,
///   polling, pub/sub) is an implementation detail and must not leak into the
///   trait.
///
/// # Implementing this trait
///
/// Three methods carry defaults so that stores written before the features
/// existed keep compiling: [`set_task_meta`](Self::set_task_meta),
/// [`discard_task`](Self::discard_task), and
/// [`resume_context`](Self::resume_context). Each default reports "not
/// supported" rather than quietly succeeding, and the router turns that into a
/// visible failure. A store that supports input requests must therefore
/// override `resume_context`, or its first `tasks/update` fails the task.
///
/// That also makes wrapping another store a trap worth naming. A decorator
/// that implements only the required methods inherits the defaults, which
/// silently disables resumption for the store it wraps even though the wrapped
/// store supports it. Forward every method, including the defaulted ones:
///
/// ```rust
/// use std::sync::Arc;
/// use std::sync::atomic::{AtomicUsize, Ordering};
///
/// use async_trait::async_trait;
/// use tower_mcp::CallToolResult;
/// use tower_mcp::async_task::{
///     AppliedInputResponses, CancellationToken, MemoryTaskStore, Result, TaskOwner,
///     TaskResumeContext, TaskSnapshot, TaskStore,
///     TaskPresence,
/// };
/// use tower_mcp::error::JsonRpcError;
/// use tower_mcp::protocol::{InputRequests, InputResponses, TaskObject, TaskStatus};
///
/// /// Counts the completions it actually applied, and delegates the rest.
/// struct CountingStore {
///     inner: MemoryTaskStore,
///     completed: Arc<AtomicUsize>,
/// }
///
/// #[async_trait]
/// impl TaskStore for CountingStore {
///     async fn complete_task(&self, id: &str, result: CallToolResult) -> Result<bool> {
///         let applied = self.inner.complete_task(id, result).await?;
///         // `false` means the task was already terminal, expired, or gone,
///         // so counting it would inflate the number of finished tasks.
///         if applied {
///             self.completed.fetch_add(1, Ordering::Relaxed);
///         }
///         Ok(applied)
///     }
///
///     // Required methods, forwarded unchanged.
///     async fn create_task(
///         &self,
///         tool_name: &str,
///         arguments: serde_json::Value,
///         ttl: Option<u64>,
///         owner: TaskOwner,
///     ) -> Result<(String, CancellationToken)> {
///         self.inner.create_task(tool_name, arguments, ttl, owner).await
///     }
///     async fn task_owner(&self, id: &str) -> Result<Option<TaskOwner>> {
///         self.inner.task_owner(id).await
///     }
///     // Forward this too when the wrapped store retains tombstones, or the
///     // default turns its `Expired` into `Missing` and the decorator
///     // silently removes the distinction (#1249).
///     async fn task_presence(&self, id: &str) -> Result<TaskPresence> {
///         self.inner.task_presence(id).await
///     }
///     async fn get_task(&self, id: &str) -> Result<Option<TaskObject>> {
///         self.inner.get_task(id).await
///     }
///     async fn get_task_result(&self, id: &str) -> Result<Option<TaskSnapshot>> {
///         self.inner.get_task_result(id).await
///     }
///     async fn wait_for_completion(&self, id: &str) -> Result<Option<TaskSnapshot>> {
///         self.inner.wait_for_completion(id).await
///     }
///     async fn list_tasks(&self, status: Option<TaskStatus>) -> Result<Vec<TaskObject>> {
///         self.inner.list_tasks(status).await
///     }
///     async fn require_input(
///         &self,
///         id: &str,
///         requests: InputRequests,
///         message: Option<&str>,
///     ) -> Result<bool> {
///         self.inner.require_input(id, requests, message).await
///     }
///     async fn outstanding_input_requests(&self, id: &str) -> Result<Option<InputRequests>> {
///         self.inner.outstanding_input_requests(id).await
///     }
///     async fn apply_input_responses(
///         &self,
///         id: &str,
///         responses: InputResponses,
///     ) -> Result<Option<AppliedInputResponses>> {
///         self.inner.apply_input_responses(id, responses).await
///     }
///     async fn set_ttl(&self, id: &str, ttl_ms: u64) -> Result<bool> {
///         self.inner.set_ttl(id, ttl_ms).await
///     }
///     async fn fail_task(&self, id: &str, error: JsonRpcError) -> Result<bool> {
///         self.inner.fail_task(id, error).await
///     }
///     async fn cancel_task(&self, id: &str, reason: Option<&str>) -> Result<Option<TaskObject>> {
///         self.inner.cancel_task(id, reason).await
///     }
///
///     // Defaulted methods. Omitting these would leave the wrapper reporting
///     // "not supported" for a store that supports them.
///     async fn resume_context(&self, id: &str) -> Result<Option<TaskResumeContext>> {
///         self.inner.resume_context(id).await
///     }
///     async fn set_task_meta(&self, id: &str, meta: serde_json::Value) -> Result<bool> {
///         self.inner.set_task_meta(id, meta).await
///     }
///     async fn discard_task(&self, id: &str) -> Result<bool> {
///         self.inner.discard_task(id).await
///     }
/// }
///
/// # #[tokio::main]
/// # async fn main() {
/// let completed = Arc::new(AtomicUsize::new(0));
/// let store: Arc<dyn TaskStore> = Arc::new(CountingStore {
///     inner: MemoryTaskStore::new(),
///     completed: completed.clone(),
/// });
/// // Ready to hand to `McpRouter::task_store`.
///
/// let (id, _cancel) = store
///     .create_task("build_report", serde_json::json!({}), None, None)
///     .await
///     .unwrap();
/// assert!(store.complete_task(&id, CallToolResult::text("done")).await.unwrap());
/// assert!(!store.complete_task(&id, CallToolResult::text("again")).await.unwrap());
/// assert_eq!(completed.load(Ordering::Relaxed), 1);
/// # }
/// ```
#[async_trait]
pub trait TaskStore: Send + Sync + 'static {
    /// Create and store a new task owned by `owner`.
    ///
    /// Returns the task ID and a cancellation token for the spawned work.
    /// The token is awaitable and must be raised both by explicit cancellation
    /// and when the task's TTL elapses. An external store backed by a remote
    /// database is responsible for bridging its durable expiry signal to this
    /// process-local token.
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

    /// Persist protocol `_meta` for a task.
    ///
    /// The default preserves source compatibility for external stores. Stores
    /// that want to support task preparation metadata must override it.
    async fn set_task_meta(&self, task_id: &str, meta: serde_json::Value) -> Result<bool> {
        let _ = (task_id, meta);
        Ok(false)
    }

    /// Remove a task that could not finish initialization.
    ///
    /// The default preserves source compatibility for external stores. Stores
    /// used with preparation callbacks should override it.
    async fn discard_task(&self, task_id: &str) -> Result<bool> {
        let _ = task_id;
        Ok(false)
    }

    /// Get a task's full snapshot (task object, result, error) by ID.
    async fn get_task_result(&self, task_id: &str) -> Result<Option<TaskSnapshot>>;

    /// Wait for a task to reach a terminal state, then return its snapshot.
    ///
    /// If the task is already terminal, returns immediately. Otherwise blocks
    /// until the task completes, fails, is cancelled, or expires. Returns
    /// `None` if the task is unknown or expires while waiting.
    async fn wait_for_completion(&self, task_id: &str) -> Result<Option<TaskSnapshot>>;

    /// List all tasks, optionally filtered by status.
    async fn list_tasks(&self, status_filter: Option<TaskStatus>) -> Result<Vec<TaskObject>>;

    /// Mark a task as requiring input, recording the requests to be answered.
    ///
    /// `requests` replaces the outstanding set. A key that was outstanding
    /// and does not appear in the new snapshot becomes superseded.
    ///
    /// Returns `Ok(false)` if the task is unknown, expired, or already
    /// terminal.
    ///
    /// # Key uniqueness
    ///
    /// SEP-2663 requires every request key to be unique over a single task's
    /// lifetime. A key is spent once its request has been answered or
    /// superseded, and must never name a second request; that guarantee is
    /// what lets a client deduplicate across polls and lets a server ignore
    /// responses for already-satisfied requests (#1246).
    ///
    /// Reissuing a spent key is a
    /// [`TaskStoreError::InvalidTransition`], not a backend failure: it is
    /// deterministic, and retrying cannot help.
    ///
    /// Carrying an unanswered request forward is not reuse. Because
    /// `requests` replaces the whole snapshot, a still-outstanding key has to
    /// be reissued to stay outstanding, and doing so names the same question
    /// rather than a second one. Repointing a live key at a *different*
    /// request is reuse and must be rejected.
    ///
    /// The uniqueness scope is the task. Keys from a preceding MRTR phase are
    /// a separate namespace and do not constrain a task's keys.
    ///
    /// # Implementing this
    ///
    /// An external store must enforce the same rule, and the check and the
    /// snapshot replacement must be atomic: two concurrent parks that each
    /// see a key as unspent would otherwise both admit it. Stores written
    /// before this rule existed keep compiling and keep their old permissive
    /// behaviour, so this is a behavioural migration rather than a
    /// compile-visible one.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::async_task::{MemoryTaskStore, TaskStore, TaskStoreError};
    /// use tower_mcp::protocol::{InputRequest, InputRequests, ListRootsParams, TaskStatus};
    ///
    /// fn ask(keys: &[&str]) -> InputRequests {
    ///     keys.iter()
    ///         .map(|key| {
    ///             (
    ///                 key.to_string(),
    ///                 InputRequest::ListRoots(ListRootsParams { meta: None }),
    ///             )
    ///         })
    ///         .collect()
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let store = MemoryTaskStore::new();
    /// let (id, _cancel) = store
    ///     .create_task("deploy", serde_json::json!({}), None, None)
    ///     .await
    ///     .unwrap();
    ///
    /// assert!(
    ///     store
    ///         .require_input(&id, ask(&["approval"]), Some("needs a decision"))
    ///         .await
    ///         .unwrap()
    /// );
    /// let task = store.get_task(&id).await.unwrap().unwrap();
    /// assert_eq!(task.status, TaskStatus::InputRequired);
    /// assert_eq!(task.status_message.as_deref(), Some("needs a decision"));
    ///
    /// // Asking a second thing means reissuing the first: the snapshot is
    /// // replaced wholesale, so a key left out becomes superseded.
    /// assert!(
    ///     store
    ///         .require_input(&id, ask(&["approval", "region"]), None)
    ///         .await
    ///         .unwrap()
    /// );
    ///
    /// // A spent key cannot come back. This one was superseded rather than
    /// // answered; both count as spent.
    /// store
    ///     .require_input(&id, ask(&["region"]), None)
    ///     .await
    ///     .unwrap();
    /// let error = store
    ///     .require_input(&id, ask(&["approval"]), None)
    ///     .await
    ///     .expect_err("a spent key must not name a second question");
    /// assert!(matches!(error, TaskStoreError::InvalidTransition(_)));
    /// # }
    /// ```
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
    ///
    /// Ignoring is deliberate rather than lenient parsing. A key that was
    /// never issued, one already answered, and one superseded by a later
    /// request are indistinguishable to a client that is retrying, and
    /// rejecting the whole update would fail a task over a duplicate delivery.
    /// They are reported in [`AppliedInputResponses::ignored`] so a server can
    /// still notice.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::async_task::{MemoryTaskStore, TaskStore};
    /// use tower_mcp::protocol::{
    ///     ElicitAction, ElicitResult, InputRequest, InputRequests, InputResponse,
    ///     ListRootsParams, TaskStatus,
    /// };
    ///
    /// fn ask(keys: &[&str]) -> InputRequests {
    ///     keys.iter()
    ///         .map(|key| {
    ///             (
    ///                 key.to_string(),
    ///                 InputRequest::ListRoots(ListRootsParams { meta: None }),
    ///             )
    ///         })
    ///         .collect()
    /// }
    ///
    /// fn accept(key: &str) -> (String, InputResponse) {
    ///     (
    ///         key.to_string(),
    ///         InputResponse::Elicit(ElicitResult {
    ///             action: ElicitAction::Accept,
    ///             content: None,
    ///             meta: None,
    ///         }),
    ///     )
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let store = MemoryTaskStore::new();
    /// let (id, _cancel) = store
    ///     .create_task("deploy", serde_json::json!({}), None, None)
    ///     .await
    ///     .unwrap();
    /// store
    ///     .require_input(&id, ask(&["approval", "region"]), None)
    ///     .await
    ///     .unwrap();
    ///
    /// // A partial answer is valid, and leaves the task parked.
    /// let applied = store
    ///     .apply_input_responses(&id, [accept("approval")].into_iter().collect())
    ///     .await
    ///     .unwrap()
    ///     .unwrap();
    /// assert_eq!(applied.accepted, ["approval".to_string()].into());
    /// assert_eq!(applied.still_outstanding, ["region".to_string()].into());
    /// assert!(!applied.is_complete());
    /// assert_eq!(
    ///     store.get_task(&id).await.unwrap().unwrap().status,
    ///     TaskStatus::InputRequired
    /// );
    ///
    /// // A replayed answer, plus a key nobody asked for: both ignored, so the
    /// // task is neither failed nor resumed early.
    /// let applied = store
    ///     .apply_input_responses(
    ///         &id,
    ///         [accept("approval"), accept("never-issued")].into_iter().collect(),
    ///     )
    ///     .await
    ///     .unwrap()
    ///     .unwrap();
    /// assert!(applied.accepted.is_empty());
    /// assert_eq!(
    ///     applied.ignored,
    ///     ["approval".to_string(), "never-issued".to_string()].into()
    /// );
    ///
    /// // Answering the last outstanding request is what resumes the task.
    /// let applied = store
    ///     .apply_input_responses(&id, [accept("region")].into_iter().collect())
    ///     .await
    ///     .unwrap()
    ///     .unwrap();
    /// assert!(applied.is_complete());
    /// assert_eq!(
    ///     store.get_task(&id).await.unwrap().unwrap().status,
    ///     TaskStatus::Working
    /// );
    /// # }
    /// ```
    async fn apply_input_responses(
        &self,
        task_id: &str,
        responses: InputResponses,
    ) -> Result<Option<AppliedInputResponses>>;

    /// Resolve a task to present, expired, or missing, with its owner.
    ///
    /// The router uses this both to authorize an operation and to classify an
    /// absent result afterwards, so a store that retains expired records can
    /// tell its owner "that task expired" instead of "no such task" (#1249).
    ///
    /// Defaults to today's behaviour, treating anything `task_owner` does not
    /// return as [`TaskPresence::Missing`], so existing stores compile and
    /// behave unchanged. Override it when the store retains tombstones.
    ///
    /// # Implementing this
    ///
    /// Report `Expired` only for a record the store still holds; dropping
    /// expired records and answering `Missing` is correct.
    ///
    /// The owner must be returned for `Expired` as well as `Present`. The
    /// router authorizes before disclosing the difference, and it cannot do
    /// that without knowing who owns an expired task.
    ///
    /// The lookup should be atomic with respect to expiry where the backend
    /// allows it: the router resolves a second time after an operation returns
    /// nothing, and a store whose active and tombstone lookups can disagree
    /// may answer `Missing` for a task that expired mid-operation.
    async fn task_presence(&self, task_id: &str) -> Result<TaskPresence> {
        Ok(match self.task_owner(task_id).await? {
            Some(owner) => TaskPresence::Present { owner },
            None => TaskPresence::Missing,
        })
    }

    /// Every answer accumulated for a task so far, keyed as issued.
    ///
    /// A live handler reads this after being woken, so that what it observes
    /// is exactly what was durably recorded (#1246). Defaults to reading
    /// through [`resume_context`](Self::resume_context), so a store that
    /// already supports resumption needs no change.
    async fn input_responses(&self, task_id: &str) -> Result<Option<InputResponses>> {
        Ok(self
            .resume_context(task_id)
            .await?
            .map(|resume| resume.input_responses))
    }

    /// Set a task's non-terminal status and message.
    ///
    /// Terminal states are reached through [`complete_task`](Self::complete_task),
    /// [`fail_task`](Self::fail_task), and [`cancel_task`](Self::cancel_task);
    /// this is for progress reporting while a task is still running. Returns
    /// `Ok(false)` if the task is unknown, expired, or already terminal.
    async fn set_status(
        &self,
        task_id: &str,
        status: TaskStatus,
        message: Option<&str>,
    ) -> Result<bool> {
        let _ = (task_id, status, message);
        Ok(false)
    }

    /// Everything needed to re-invoke a task's handler after its input
    /// requests were answered.
    ///
    /// Returning `None` means this store cannot resume, and the router fails
    /// the task with a message saying so rather than leaving it in `working`
    /// forever. The default returns `None` so an external store written
    /// before resumption existed keeps compiling and fails loudly instead of
    /// hanging; implement it to support the flow (#1208).
    ///
    /// The returned context must also carry the task's cancellation/expiry
    /// signal. Construct it with [`TaskResumeContext::new`] and attach the
    /// token with [`TaskResumeContext::with_cancellation_token`]. Reusing the
    /// signal returned from [`create_task`](Self::create_task) lets the router
    /// stop a replayed handler exactly when the task expires.
    ///
    /// # Example
    ///
    /// The answers accumulate across rounds, because the handler is re-run
    /// from the top and has to see everything it was told so far, not only the
    /// most recent answer:
    ///
    /// ```rust
    /// use tower_mcp::async_task::{MemoryTaskStore, TaskStore};
    /// use tower_mcp::protocol::{
    ///     ElicitAction, ElicitResult, InputRequest, InputRequests, InputResponse,
    ///     ListRootsParams,
    /// };
    ///
    /// # fn ask(keys: &[&str]) -> InputRequests {
    /// #     keys.iter()
    /// #         .map(|key| {
    /// #             (
    /// #                 key.to_string(),
    /// #                 InputRequest::ListRoots(ListRootsParams { meta: None }),
    /// #             )
    /// #         })
    /// #         .collect()
    /// # }
    /// # fn accept(key: &str) -> (String, InputResponse) {
    /// #     (
    /// #         key.to_string(),
    /// #         InputResponse::Elicit(ElicitResult {
    /// #             action: ElicitAction::Accept,
    /// #             content: None,
    /// #             meta: None,
    /// #         }),
    /// #     )
    /// # }
    /// # #[tokio::main]
    /// # async fn main() {
    /// let store = MemoryTaskStore::new();
    /// let (id, _cancel) = store
    ///     .create_task("deploy", serde_json::json!({"service": "api"}), None, None)
    ///     .await
    ///     .unwrap();
    ///
    /// store.require_input(&id, ask(&["approval"]), None).await.unwrap();
    /// store
    ///     .apply_input_responses(&id, [accept("approval")].into_iter().collect())
    ///     .await
    ///     .unwrap();
    /// store.require_input(&id, ask(&["region"]), None).await.unwrap();
    /// store
    ///     .apply_input_responses(&id, [accept("region")].into_iter().collect())
    ///     .await
    ///     .unwrap();
    ///
    /// let resume = store.resume_context(&id).await.unwrap().unwrap();
    /// assert_eq!(resume.tool_name, "deploy");
    /// assert_eq!(resume.arguments, serde_json::json!({"service": "api"}));
    /// assert_eq!(
    ///     resume.input_responses.keys().collect::<Vec<_>>(),
    ///     vec!["approval", "region"],
    ///     "an earlier round's answer is still there on the second resume"
    /// );
    /// # }
    /// ```
    async fn resume_context(&self, task_id: &str) -> Result<Option<TaskResumeContext>> {
        let _ = task_id;
        Ok(None)
    }

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
    ///
    /// [`MemoryTaskStore`] returns
    /// [`TaskStoreError::RetentionLimitExceeded`] for an oversized result
    /// only after atomically replacing the live record with a fixed,
    /// content-free `failed` snapshot and waking completion waiters. The
    /// rejected result is not retained. External stores may choose a
    /// different recovery policy, so generic callers should still handle the
    /// error rather than assuming every implementation terminalized.
    ///
    /// # Example
    ///
    /// A tool that ran and reported a problem is a completed task, and the
    /// error result is what `tasks/get` hands back:
    ///
    /// ```rust
    /// use tower_mcp::CallToolResult;
    /// use tower_mcp::async_task::{MemoryTaskStore, TaskStore};
    /// use tower_mcp::protocol::TaskStatus;
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let store = MemoryTaskStore::new();
    /// let (id, _cancel) = store
    ///     .create_task("deploy", serde_json::json!({}), None, None)
    ///     .await
    ///     .unwrap();
    ///
    /// let mut result = CallToolResult::text("region eu-west-3 is not enabled");
    /// result.is_error = true;
    /// assert!(store.complete_task(&id, result).await.unwrap());
    ///
    /// let (task, result, error) = store.get_task_result(&id).await.unwrap().unwrap();
    /// assert_eq!(task.status, TaskStatus::Completed);
    /// assert!(result.unwrap().is_error);
    /// assert!(error.is_none(), "isError is a result, not a JSON-RPC error");
    /// # }
    /// ```
    async fn complete_task(&self, task_id: &str, result: CallToolResult) -> Result<bool>;

    /// Mark a task as failed with a structured execution error.
    ///
    /// Returns `Ok(false)` if the task is unknown, expired, or already
    /// terminal.
    ///
    /// [`MemoryTaskStore`] returns
    /// [`TaskStoreError::RetentionLimitExceeded`] for an oversized error only
    /// after atomically storing its fixed, content-free `failed` snapshot and
    /// waking completion waiters. The rejected diagnostic is not retained.
    ///
    /// Reserved for a call that never produced a result at all. A tool that
    /// ran and reported a problem completes instead, carrying an `isError`
    /// result; see [`complete_task`](Self::complete_task).
    ///
    /// # Example
    ///
    /// The error is stored whole, not flattened to a message, because
    /// SEP-2663 requires `tasks/get` on a failed task to return a JSON-RPC
    /// error object:
    ///
    /// ```rust
    /// use tower_mcp::async_task::{MemoryTaskStore, TaskStore};
    /// use tower_mcp::error::JsonRpcError;
    /// use tower_mcp::protocol::TaskStatus;
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let store = MemoryTaskStore::new();
    /// let (id, _cancel) = store
    ///     .create_task("deploy", serde_json::json!({}), None, None)
    ///     .await
    ///     .unwrap();
    ///
    /// let mut error = JsonRpcError::invalid_params("unknown region");
    /// error.data = Some(serde_json::json!({"field": "region"}));
    /// assert!(store.fail_task(&id, error).await.unwrap());
    ///
    /// let (task, result, error) = store.get_task_result(&id).await.unwrap().unwrap();
    /// assert_eq!(task.status, TaskStatus::Failed);
    /// assert!(result.is_none());
    ///
    /// let error = error.unwrap();
    /// assert_eq!(error.code, -32602);
    /// assert_eq!(error.data.unwrap()["field"], "region");
    /// # }
    /// ```
    async fn fail_task(&self, task_id: &str, error: JsonRpcError) -> Result<bool>;

    /// Cancel a task.
    ///
    /// Signals the task's [`CancellationToken`] and, if the task is not
    /// already terminal, marks it cancelled. Returns the updated task object,
    /// or `None` if the task is unknown.
    ///
    /// The token is raised even for a task that already finished, so work
    /// still winding down behind a completed task is told to stop. That is
    /// also why the returned object may read `completed` rather than
    /// `cancelled`: the recorded outcome does not change, only the token.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::CallToolResult;
    /// use tower_mcp::async_task::{MemoryTaskStore, TaskStore};
    /// use tower_mcp::protocol::TaskStatus;
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let store = MemoryTaskStore::new();
    /// let (id, token) = store
    ///     .create_task("deploy", serde_json::json!({}), None, None)
    ///     .await
    ///     .unwrap();
    ///
    /// // A handler polls the token between steps; cancellation is
    /// // cooperative and interrupts nothing on its own.
    /// assert!(!token.is_cancelled());
    ///
    /// let task = store.cancel_task(&id, Some("user closed the tab")).await.unwrap();
    /// assert_eq!(task.unwrap().status, TaskStatus::Cancelled);
    /// assert!(token.is_cancelled());
    ///
    /// // Cancelling again is harmless, and a late result is refused.
    /// assert!(
    ///     !store
    ///         .complete_task(&id, CallToolResult::text("finished anyway"))
    ///         .await
    ///         .unwrap()
    /// );
    /// # }
    /// ```
    async fn cancel_task(&self, task_id: &str, reason: Option<&str>) -> Result<Option<TaskObject>>;
}

#[derive(Debug, Default)]
struct WorkerSignalState {
    generation: u64,
    shutdown: bool,
}

/// Condvar used only to reschedule or stop the memory-store worker.
///
/// It is deliberately separate from [`MemoryTaskStoreState`]. The worker may
/// hold this strongly while it sleeps, but holds only a [`Weak`] reference to
/// the actual task state, so dropping the final store clone is enough to stop
/// and release the state.
#[derive(Debug, Default)]
struct WorkerSignal {
    state: Mutex<WorkerSignalState>,
    changed: Condvar,
}

impl WorkerSignal {
    fn generation(&self) -> Option<u64> {
        let state = self.state.lock().ok()?;
        (!state.shutdown).then_some(state.generation)
    }

    fn wake(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.generation = state.generation.wrapping_add(1);
            self.changed.notify_one();
        }
    }

    fn shutdown(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.shutdown = true;
            state.generation = state.generation.wrapping_add(1);
            self.changed.notify_all();
        }
    }

    /// Returns true when shutdown was requested.
    fn wait_for_change(&self, generation: u64, timeout: Duration) -> bool {
        let Ok(state) = self.state.lock() else {
            return true;
        };
        if state.shutdown {
            return true;
        }
        if state.generation != generation {
            return false;
        }
        match self.changed.wait_timeout(state, timeout) {
            Ok((state, _)) => state.shutdown,
            Err(_) => true,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Retirement {
    signalled: usize,
    removed: usize,
}

#[derive(Debug, Default)]
struct MemoryTaskStoreData {
    tasks: HashMap<String, StoredTask>,
    retained_bytes: usize,
    reserved_bytes: usize,
}

impl MemoryTaskStoreData {
    fn usage(&self) -> TaskStoreUsage {
        TaskStoreUsage {
            task_count: self.tasks.len(),
            retained_bytes: self.retained_bytes,
            reserved_bytes: self.reserved_bytes,
        }
    }
}

#[derive(Debug)]
struct MemoryTaskStoreState {
    data: RwLock<MemoryTaskStoreData>,
    config: MemoryTaskStoreConfig,
    retention_limits: TaskRetentionLimits,
    worker_signal: Arc<WorkerSignal>,
    worker_started: OnceLock<std::result::Result<(), String>>,
}

impl MemoryTaskStoreState {
    fn retire_expired(&self, remove: bool) -> Retirement {
        let Ok(mut data) = self.data.write() else {
            return Retirement::default();
        };
        let now = Instant::now();
        let mut retirement = Retirement::default();
        let mut retired_old_retained = 0usize;
        let mut retired_new_retained = 0usize;
        let mut retired_old_reserved = 0usize;
        for task in data.tasks.values_mut() {
            if task.is_expired_at(now) && task.signal_expiry() {
                retirement.signalled += 1;
                let old_retained = task.retained_bytes;
                let old_reserved = task.reserved_bytes;
                task.scrub_expired_payload();
                task.retained_bytes = retained_payload_size(&task.task, usize::MAX)
                    .expect("built-in task payload serialization is infallible");
                retired_old_retained = retired_old_retained
                    .checked_add(old_retained)
                    .expect("task-store retained-byte accounting overflowed");
                retired_new_retained = retired_new_retained
                    .checked_add(task.retained_bytes)
                    .expect("task-store retained-byte accounting overflowed");
                retired_old_reserved = retired_old_reserved
                    .checked_add(old_reserved)
                    .expect("task-store reserved-byte accounting overflowed");
            }
        }
        data.retained_bytes = data
            .retained_bytes
            .checked_sub(retired_old_retained)
            .and_then(|bytes| bytes.checked_add(retired_new_retained))
            .expect("task-store retained-byte accounting invariant violated");
        data.reserved_bytes = data
            .reserved_bytes
            .checked_sub(retired_old_reserved)
            .expect("task-store reserved-byte accounting invariant violated");
        charged_bytes(
            data.retained_bytes,
            data.reserved_bytes,
            self.retention_limits.max_retained_bytes,
        )
        .expect("expiry scrubbing exceeded the task-store aggregate-byte invariant");
        if remove {
            let before = data.tasks.len();
            let mut removed_retained = 0usize;
            let mut removed_reserved = 0usize;
            data.tasks.retain(|_, task| {
                let keep = !task.is_expired_at(now);
                if !keep {
                    removed_retained = removed_retained
                        .checked_add(task.retained_bytes)
                        .expect("task-store retained-byte accounting overflowed");
                    removed_reserved = removed_reserved
                        .checked_add(task.reserved_bytes)
                        .expect("task-store reserved-byte accounting overflowed");
                }
                keep
            });
            data.retained_bytes = data
                .retained_bytes
                .checked_sub(removed_retained)
                .expect("task-store retained-byte accounting invariant violated");
            data.reserved_bytes = data
                .reserved_bytes
                .checked_sub(removed_reserved)
                .expect("task-store reserved-byte accounting invariant violated");
            retirement.removed = before - data.tasks.len();
        }
        retirement
    }

    fn next_expiry(&self) -> Option<Instant> {
        self.data
            .read()
            .ok()?
            .tasks
            .values()
            .filter_map(|task| {
                (!task.expiry_signalled)
                    .then(|| task.expires_at())
                    .flatten()
            })
            .min()
    }
}

impl Drop for MemoryTaskStoreState {
    fn drop(&mut self) {
        self.worker_signal.shutdown();
    }
}

fn next_deadline(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    }
}

fn memory_task_store_worker(state: Weak<MemoryTaskStoreState>, signal: Arc<WorkerSignal>) {
    const MAX_SLEEP: Duration = Duration::from_secs(60 * 60);

    let Some(initial) = state.upgrade() else {
        return;
    };
    let cleanup_interval = if initial.config.cleanup_interval.is_zero() {
        Duration::from_millis(1)
    } else {
        initial.config.cleanup_interval
    };
    let mut cleanup_at = Instant::now().checked_add(cleanup_interval);
    drop(initial);

    loop {
        let Some(state) = state.upgrade() else {
            break;
        };
        let Some(generation) = signal.generation() else {
            break;
        };

        let now = Instant::now();
        if cleanup_at.is_some_and(|deadline| now >= deadline) {
            state.retire_expired(true);
            // Measure the cadence from the end of the pass. With a short
            // interval and a large map, measuring from `now` above could make
            // an O(n) pass immediately overdue and keep the worker hot.
            cleanup_at = Instant::now().checked_add(cleanup_interval);
        } else {
            // Expiry signalling is independent from physical cleanup.
            state.retire_expired(false);
        }

        let deadline = next_deadline(cleanup_at, state.next_expiry());
        let timeout = deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(MAX_SLEEP)
            .min(MAX_SLEEP);
        drop(state);

        // Comparing generations under the condvar lock closes the gap between
        // computing `deadline` and beginning the wait: create_task/set_ttl
        // cannot deliver a notification that gets lost in that window.
        if signal.wait_for_change(generation, timeout) {
            break;
        }
    }
}

/// In-memory [`TaskStore`] backed by a `HashMap`.
///
/// This is the default store. Suitable for single-instance deployments. For
/// horizontal scaling, use an external store that shares state across
/// instances. A lazy background worker signals task expiry at its exact TTL
/// deadline and physically removes expired records at the configured cleanup
/// interval. It uses a standard thread rather than assuming construction
/// happens inside a Tokio runtime, and holds only weak task state while
/// sleeping.
///
/// Completion wakeups for
/// [`wait_for_completion`](TaskStore::wait_for_completion) use a per-task
/// [`tokio::sync::Notify`], which is an implementation detail of this store.
#[derive(Debug, Clone)]
pub struct MemoryTaskStore {
    state: Arc<MemoryTaskStoreState>,
}

impl Default for MemoryTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryTaskStore {
    /// Create a task store with the default lifecycle and retention policy.
    ///
    /// That is a five-minute TTL, one-minute cleanup interval, and the finite
    /// [`TaskRetentionLimits::default`] byte/count bounds.
    pub fn new() -> Self {
        Self::with_config(MemoryTaskStoreConfig::default())
    }

    /// Create a task store with an explicit lifecycle policy and the default
    /// finite retention limits.
    pub fn with_config(config: MemoryTaskStoreConfig) -> Self {
        Self::with_config_and_retention(config, TaskRetentionLimits::default())
    }

    /// Create a task store with the default lifecycle policy and explicit
    /// record and encoded-payload limits.
    pub fn with_retention_limits(retention_limits: TaskRetentionLimits) -> Self {
        Self::with_config_and_retention(MemoryTaskStoreConfig::default(), retention_limits)
    }

    /// Create a task store with explicit lifecycle and retention policies.
    pub fn with_config_and_retention(
        config: MemoryTaskStoreConfig,
        retention_limits: TaskRetentionLimits,
    ) -> Self {
        Self {
            state: Arc::new(MemoryTaskStoreState {
                data: RwLock::new(MemoryTaskStoreData::default()),
                config,
                retention_limits,
                worker_signal: Arc::new(WorkerSignal::default()),
                worker_started: OnceLock::new(),
            }),
        }
    }

    fn ensure_worker(&self) -> Result<()> {
        let weak = Arc::downgrade(&self.state);
        let signal = self.state.worker_signal.clone();
        match self.state.worker_started.get_or_init(|| {
            std::thread::Builder::new()
                .name("tower-mcp-task-expiry".to_string())
                .spawn(move || memory_task_store_worker(weak, signal))
                .map(drop)
                .map_err(|error| format!("failed to start task expiry worker: {error}"))
        }) {
            Ok(()) => Ok(()),
            Err(error) => Err(TaskStoreError::Backend(error.clone())),
        }
    }

    /// Remove expired tasks immediately.
    ///
    /// Returns the number removed. Not part of the [`TaskStore`] trait;
    /// external backends typically expire entries natively (e.g. Redis TTL).
    ///
    /// The configured worker already calls this retirement path periodically;
    /// applications may call it to reclaim memory sooner. Calling it is an
    /// optimization, not a correctness requirement: expiry has already
    /// cancelled work, woken waiters, and made the task read as absent.
    ///
    /// # Example
    ///
    /// ```rust
    /// use tower_mcp::async_task::{MemoryTaskStore, TaskStore};
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let store = MemoryTaskStore::new();
    /// // A one millisecond retention window, and no terminal state: the
    /// // clock runs from creation, so the task retires while still working.
    /// let (id, _cancel) = store
    ///     .create_task("deploy", serde_json::json!({}), Some(1), None)
    ///     .await
    ///     .unwrap();
    /// tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    ///
    /// // Already invisible, before anything has been reclaimed.
    /// assert!(store.get_task(&id).await.unwrap().is_none());
    /// assert!(store.list_tasks(None).await.unwrap().is_empty());
    ///
    /// // Cleanup only frees the memory the entry was still holding.
    /// assert_eq!(store.cleanup_expired(), 1);
    /// assert_eq!(store.cleanup_expired(), 0);
    /// # }
    /// ```
    pub fn cleanup_expired(&self) -> usize {
        self.state.retire_expired(true).removed
    }

    /// Return content-free count and encoded-byte gauges.
    ///
    /// The snapshot is taken under the same lock as task mutations, so its
    /// count and both byte totals always describe one committed store state.
    #[must_use]
    pub fn usage(&self) -> TaskStoreUsage {
        match self.state.data.read() {
            Ok(data) => data.usage(),
            Err(poisoned) => poisoned.into_inner().usage(),
        }
    }

    /// Get the number of tasks in the store
    #[cfg(test)]
    pub fn len(&self) -> usize {
        if let Ok(data) = self.state.data.read() {
            data.tasks.len()
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
        self.ensure_worker()?;
        let limits = self.state.retention_limits;
        validate_payload(tool_name, limits)?;
        validate_payload(&arguments, limits)?;
        validate_payload(&owner, limits)?;
        let id = generate_task_id();
        let ttl = ttl.unwrap_or_else(|| duration_millis_saturated(self.state.config.default_ttl));
        let task = Task::new(id.clone(), tool_name.to_string(), arguments, ttl, owner);
        let token = task.cancellation_token.clone();
        let stored = prepare_stored_task(task, limits)?;

        // Count admission reclaims expired tombstones first. The retirement
        // and the following admission each hold the same data lock; concurrent
        // creators can never both observe and consume one remaining slot.
        self.state.retire_expired(true);
        let mut data = self.state.data.write().map_err(|_| {
            TaskStoreError::Backend("in-memory task store lock poisoned".to_string())
        })?;
        if data.tasks.len() >= limits.max_tasks {
            return Err(TaskStoreError::RetentionLimitExceeded {
                kind: TaskRetentionLimitKind::TaskCount,
                limit: limits.max_tasks,
            });
        }
        let retained_bytes = data
            .retained_bytes
            .checked_add(stored.retained_bytes)
            .ok_or(TaskStoreError::RetentionLimitExceeded {
                kind: TaskRetentionLimitKind::AggregateBytes,
                limit: limits.max_retained_bytes,
            })?;
        let reserved_bytes = data
            .reserved_bytes
            .checked_add(stored.reserved_bytes)
            .ok_or(TaskStoreError::RetentionLimitExceeded {
                kind: TaskRetentionLimitKind::AggregateBytes,
                limit: limits.max_retained_bytes,
            })?;
        charged_bytes(retained_bytes, reserved_bytes, limits.max_retained_bytes)?;
        data.retained_bytes = retained_bytes;
        data.reserved_bytes = reserved_bytes;
        data.tasks.insert(id.clone(), stored);
        drop(data);
        self.state.worker_signal.wake();

        Ok((id, token))
    }

    async fn get_task(&self, task_id: &str) -> Result<Option<TaskObject>> {
        self.state.retire_expired(false);
        Ok(if let Ok(data) = self.state.data.read() {
            data.tasks
                .get(task_id)
                .filter(|t| !t.is_expired())
                .map(|t| t.to_task_object())
        } else {
            None
        })
    }

    async fn set_task_meta(&self, task_id: &str, meta: serde_json::Value) -> Result<bool> {
        self.state.retire_expired(false);
        let Ok(mut data) = self.state.data.write() else {
            return Ok(false);
        };
        let Some(current) = data.tasks.get(task_id).filter(|task| !task.is_expired()) else {
            return Ok(false);
        };
        let limits = self.state.retention_limits;
        validate_payload(&meta, limits)?;
        let mut task = current.task.clone();
        task.meta = Some(meta);
        let replacement = prepare_stored_task(task, limits)?;
        replace_stored_task(&mut data, task_id, replacement, limits.max_retained_bytes)
    }

    async fn discard_task(&self, task_id: &str) -> Result<bool> {
        let Ok(mut data) = self.state.data.write() else {
            return Ok(false);
        };
        let Some(removed) = data.tasks.remove(task_id) else {
            return Ok(false);
        };
        data.retained_bytes = data
            .retained_bytes
            .checked_sub(removed.retained_bytes)
            .expect("task-store retained-byte accounting invariant violated");
        data.reserved_bytes = data
            .reserved_bytes
            .checked_sub(removed.reserved_bytes)
            .expect("task-store reserved-byte accounting invariant violated");
        Ok(true)
    }

    async fn task_owner(&self, task_id: &str) -> Result<Option<TaskOwner>> {
        self.state.retire_expired(false);
        Ok(if let Ok(data) = self.state.data.read() {
            data.tasks
                .get(task_id)
                .filter(|t| !t.is_expired())
                .map(|t| t.owner.clone())
        } else {
            None
        })
    }

    /// This store keeps expired records until `cleanup_expired` runs, so it
    /// can tell an owner that a task expired rather than that it never
    /// existed (#1249). One read resolves both, so expiry cannot change
    /// between deciding presence and reading the owner.
    async fn task_presence(&self, task_id: &str) -> Result<TaskPresence> {
        self.state.retire_expired(false);
        let Ok(data) = self.state.data.read() else {
            return Ok(TaskPresence::Missing);
        };
        Ok(match data.tasks.get(task_id) {
            Some(task) if task.is_expired() => TaskPresence::Expired {
                owner: task.owner.clone(),
            },
            Some(task) => TaskPresence::Present {
                owner: task.owner.clone(),
            },
            None => TaskPresence::Missing,
        })
    }

    async fn get_task_result(&self, task_id: &str) -> Result<Option<TaskSnapshot>> {
        self.state.retire_expired(false);
        Ok(if let Ok(data) = self.state.data.read() {
            data.tasks
                .get(task_id)
                .filter(|t| !t.is_expired())
                .map(|t| (t.to_task_object(), t.result.clone(), t.error.clone()))
        } else {
            None
        })
    }

    async fn wait_for_completion(&self, task_id: &str) -> Result<Option<TaskSnapshot>> {
        self.state.retire_expired(false);
        // Register the wait while holding the read lock, so a transition
        // cannot notify between the state check and waiter registration.
        let (mut notified, cancellation) = {
            let Ok(data) = self.state.data.read() else {
                return Ok(None);
            };
            let Some(task) = data.tasks.get(task_id).filter(|t| !t.is_expired()) else {
                return Ok(None);
            };
            if task.status.is_terminal() {
                return Ok(Some((
                    task.to_task_object(),
                    task.result.clone(),
                    task.error.clone(),
                )));
            }
            let mut notified = Box::pin(task.completion_notify.clone().notified_owned());
            let _ = notified.as_mut().enable();
            (notified, task.cancellation_token.clone())
        };

        let cancellation_fired = tokio::select! {
            _ = &mut notified => false,
            _ = cancellation.cancelled() => true,
        };

        if cancellation_fired {
            match self.get_task_result(task_id).await? {
                // A live handler receives the store token directly and owns
                // cooperative teardown. Explicit cancellation can therefore
                // raise the token before the handler confirms a terminal
                // state. Keep waiting on the already-enabled notification in
                // that case; expiry returned `None` above and terminal
                // cancellation returned a terminal snapshot.
                Some((task, _, _)) if !task.status.is_terminal() => {
                    notified.await;
                }
                snapshot => return Ok(snapshot),
            }
        }

        // Read the result
        self.get_task_result(task_id).await
    }

    async fn list_tasks(&self, status_filter: Option<TaskStatus>) -> Result<Vec<TaskObject>> {
        self.state.retire_expired(false);
        Ok(if let Ok(data) = self.state.data.read() {
            data.tasks
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
        self.state.retire_expired(false);
        let Ok(mut data) = self.state.data.write() else {
            return Ok(false);
        };
        let Some(current) = data.tasks.get(task_id).filter(|task| !task.is_expired()) else {
            return Ok(false);
        };
        if current.status.is_terminal() {
            return Ok(false);
        }
        let limits = self.state.retention_limits;
        validate_payload(&requests, limits)?;
        if let Some(message) = message {
            validate_payload(message, limits)?;
        }
        let mut task = current.task.clone();

        // SEP-2663: "Each request key in `inputRequests` MUST be unique over
        // the lifetime of a single task. A server MUST NOT reuse a key for a
        // subsequent server-to-client request after a response for that key
        // has been delivered, and MUST NOT use the same key to refer to two
        // distinct requests over a task's lifetime."
        //
        // That guarantee is what lets a client deduplicate across polls and
        // lets a server ignore responses for already-satisfied requests, so
        // reissuing a key is rejected rather than quietly accepted (#1246).
        //
        // A key still outstanding is a different case. `requests` replaces
        // the whole snapshot, so carrying an unanswered request forward
        // reissues its key without naming a second request. That is only
        // reuse if the request behind the key changed.
        let reused: Vec<String> = requests
            .iter()
            .filter(|(key, request)| {
                if task.answered_input_keys.contains(*key)
                    || task.superseded_input_keys.contains(*key)
                {
                    return true;
                }
                match task.input_requests.get(*key) {
                    Some(current) => !same_input_request(current, request),
                    None => false,
                }
            })
            .map(|(key, _)| key.clone())
            .collect();
        if !reused.is_empty() {
            return Err(TaskStoreError::InvalidTransition(format!(
                "input request keys must be unique over a task's lifetime, but {} \
                 already {} used by this task; use a new key to ask again",
                reused.join(", "),
                if reused.len() == 1 { "was" } else { "were" },
            )));
        }

        // Outstanding requests the server dropped from the snapshot are
        // superseded, and stay recorded because a superseded key is spent for
        // the rest of the task's lifetime. A key carried forward is not.
        for key in std::mem::take(&mut task.input_requests).into_keys() {
            if !requests.contains_key(&key) {
                task.superseded_input_keys.insert(key);
            }
        }

        task.input_requests = requests;
        task.status = TaskStatus::InputRequired;
        task.status_message = Some(
            message
                .map(str::to_string)
                .unwrap_or_else(|| "Awaiting client input".to_string()),
        );
        task.last_updated_at_str = chrono_now_iso8601();
        let replacement = prepare_stored_task(task, limits)?;
        replace_stored_task(&mut data, task_id, replacement, limits.max_retained_bytes)
    }

    async fn outstanding_input_requests(&self, task_id: &str) -> Result<Option<InputRequests>> {
        self.state.retire_expired(false);
        Ok(if let Ok(data) = self.state.data.read() {
            data.tasks
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
        self.state.retire_expired(false);
        let Ok(mut data) = self.state.data.write() else {
            return Ok(None);
        };
        let Some(current) = data.tasks.get(task_id).filter(|task| !task.is_expired()) else {
            return Ok(None);
        };
        if current.status.is_terminal() {
            return Ok(None);
        }
        let limits = self.state.retention_limits;
        let accepted_payload: std::collections::BTreeMap<&str, &crate::protocol::InputResponse> =
            responses
                .iter()
                .filter(|(key, _)| current.input_requests.contains_key(*key))
                .map(|(key, response)| (key.as_str(), response))
                .collect();
        if !accepted_payload.is_empty() {
            validate_payload(&accepted_payload, limits)?;
        }
        let mut task = current.task.clone();

        let mut applied = AppliedInputResponses::default();
        for (key, response) in responses {
            if task.input_requests.remove(&key).is_some() {
                task.answered_input_keys.insert(key.clone());
                task.input_responses.insert(key.clone(), response);
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

        let replacement = prepare_stored_task(task, limits)?;
        if replace_stored_task(&mut data, task_id, replacement, limits.max_retained_bytes)? {
            Ok(Some(applied))
        } else {
            Ok(None)
        }
    }

    async fn set_status(
        &self,
        task_id: &str,
        status: TaskStatus,
        message: Option<&str>,
    ) -> Result<bool> {
        if status.is_terminal() {
            return Err(TaskStoreError::InvalidTransition(format!(
                "set_status is for non-terminal progress; use complete_task, fail_task, or cancel_task to reach {status:?}"
            )));
        }
        self.state.retire_expired(false);
        let Ok(mut data) = self.state.data.write() else {
            return Ok(false);
        };
        let Some(current) = data.tasks.get(task_id).filter(|task| !task.is_expired()) else {
            return Ok(false);
        };
        if current.status.is_terminal() {
            return Ok(false);
        }
        let limits = self.state.retention_limits;
        if let Some(message) = message {
            validate_payload(message, limits)?;
        }
        let mut task = current.task.clone();
        task.status = status;
        if let Some(message) = message {
            task.status_message = Some(message.to_string());
        }
        task.last_updated_at_str = chrono_now_iso8601();
        let replacement = prepare_stored_task(task, limits)?;
        replace_stored_task(&mut data, task_id, replacement, limits.max_retained_bytes)
    }

    async fn resume_context(&self, task_id: &str) -> Result<Option<TaskResumeContext>> {
        self.state.retire_expired(false);
        let Ok(data) = self.state.data.read() else {
            return Ok(None);
        };
        Ok(data
            .tasks
            .get(task_id)
            .filter(|task| !task.is_expired())
            .map(|task| {
                TaskResumeContext::new(
                    task.tool_name.clone(),
                    task.arguments.clone(),
                    task.input_responses.clone(),
                )
                .with_cancellation_token(task.cancellation_token.clone())
            }))
    }

    async fn set_ttl(&self, task_id: &str, ttl_ms: u64) -> Result<bool> {
        let Ok(mut data) = self.state.data.write() else {
            return Ok(false);
        };
        let Some(task) = data.tasks.get_mut(task_id) else {
            return Ok(false);
        };
        if task.is_expired() {
            drop(data);
            self.state.retire_expired(false);
            return Ok(false);
        }
        task.ttl = ttl_ms;
        task.last_updated_at_str = chrono_now_iso8601();
        drop(data);
        self.state.retire_expired(false);
        self.state.worker_signal.wake();
        Ok(true)
    }

    async fn complete_task(&self, task_id: &str, result: CallToolResult) -> Result<bool> {
        self.state.retire_expired(false);
        let Ok(mut data) = self.state.data.write() else {
            return Ok(false);
        };
        let Some(mut task) = data.tasks.remove(task_id) else {
            return Ok(false);
        };
        if task.status.is_terminal() {
            data.tasks.insert(task_id.to_string(), task);
            return Ok(false);
        }
        if task.is_expired() {
            data.tasks.insert(task_id.to_string(), task);
            drop(data);
            self.state.retire_expired(false);
            return Ok(false);
        }
        let limits = self.state.retention_limits;
        let old_retained = task.retained_bytes;
        let old_reserved = task.reserved_bytes;
        if let Err(error) = validate_payload(&result, limits) {
            drop(result);
            commit_retention_failure(&mut data, task_id, task, old_retained, old_reserved, limits);
            return Err(error);
        }
        task.status = TaskStatus::Completed;
        task.status_message = Some("Task completed".to_string());
        task.result = Some(result);
        task.input_requests = InputRequests::new();
        task.completed_at = Some(Instant::now());
        task.last_updated_at_str = chrono_now_iso8601();
        task.retained_bytes = match retained_payload_size(&task.task, limits.max_retained_bytes) {
            Ok(bytes) => bytes,
            Err(error) => {
                commit_retention_failure(
                    &mut data,
                    task_id,
                    task,
                    old_retained,
                    old_reserved,
                    limits,
                );
                return Err(error);
            }
        };
        task.reserved_bytes = 0;
        let (retained_bytes, reserved_bytes) = match replacement_totals(
            &data,
            old_retained,
            old_reserved,
            task.retained_bytes,
            0,
            limits.max_retained_bytes,
        ) {
            Ok(totals) => totals,
            Err(error) => {
                commit_retention_failure(
                    &mut data,
                    task_id,
                    task,
                    old_retained,
                    old_reserved,
                    limits,
                );
                return Err(error);
            }
        };
        let notify = task.completion_notify.clone();
        data.retained_bytes = retained_bytes;
        data.reserved_bytes = reserved_bytes;
        data.tasks.insert(task_id.to_string(), task);
        notify.notify_waiters();
        Ok(true)
    }

    async fn fail_task(&self, task_id: &str, error: JsonRpcError) -> Result<bool> {
        self.state.retire_expired(false);
        let Ok(mut data) = self.state.data.write() else {
            return Ok(false);
        };
        let Some(mut task) = data.tasks.remove(task_id) else {
            return Ok(false);
        };
        if task.status.is_terminal() {
            data.tasks.insert(task_id.to_string(), task);
            return Ok(false);
        }
        if task.is_expired() {
            data.tasks.insert(task_id.to_string(), task);
            drop(data);
            self.state.retire_expired(false);
            return Ok(false);
        }
        let limits = self.state.retention_limits;
        let old_retained = task.retained_bytes;
        let old_reserved = task.reserved_bytes;
        let error_validation = validate_payload(&error, limits);
        if let Err(retention_error) = error_validation {
            drop(error);
            commit_retention_failure(&mut data, task_id, task, old_retained, old_reserved, limits);
            return Err(retention_error);
        }
        if let Err(retention_error) =
            validate_prefixed_string(&error.message, "Task failed: ", limits)
        {
            drop(error);
            commit_retention_failure(&mut data, task_id, task, old_retained, old_reserved, limits);
            return Err(retention_error);
        }
        let status_message = format!("Task failed: {}", error.message);
        task.status = TaskStatus::Failed;
        task.status_message = Some(status_message);
        task.error = Some(error);
        task.input_requests = InputRequests::new();
        task.completed_at = Some(Instant::now());
        task.last_updated_at_str = chrono_now_iso8601();
        task.retained_bytes = match retained_payload_size(&task.task, limits.max_retained_bytes) {
            Ok(bytes) => bytes,
            Err(error) => {
                commit_retention_failure(
                    &mut data,
                    task_id,
                    task,
                    old_retained,
                    old_reserved,
                    limits,
                );
                return Err(error);
            }
        };
        task.reserved_bytes = 0;
        let (retained_bytes, reserved_bytes) = match replacement_totals(
            &data,
            old_retained,
            old_reserved,
            task.retained_bytes,
            0,
            limits.max_retained_bytes,
        ) {
            Ok(totals) => totals,
            Err(error) => {
                commit_retention_failure(
                    &mut data,
                    task_id,
                    task,
                    old_retained,
                    old_reserved,
                    limits,
                );
                return Err(error);
            }
        };
        let notify = task.completion_notify.clone();
        data.retained_bytes = retained_bytes;
        data.reserved_bytes = reserved_bytes;
        data.tasks.insert(task_id.to_string(), task);
        notify.notify_waiters();
        Ok(true)
    }

    async fn cancel_task(&self, task_id: &str, reason: Option<&str>) -> Result<Option<TaskObject>> {
        self.state.retire_expired(false);
        let Ok(mut data) = self.state.data.write() else {
            return Ok(None);
        };
        let Some(mut task) = data.tasks.remove(task_id) else {
            return Ok(None);
        };
        if task.is_expired() {
            data.tasks.insert(task_id.to_string(), task);
            drop(data);
            self.state.retire_expired(false);
            return Ok(None);
        }

        // Signal cancellation
        task.cancellation_token.cancel();

        // If not already terminal, mark as cancelled
        if !task.status.is_terminal() {
            let limits = self.state.retention_limits;
            let old_retained = task.retained_bytes;
            let old_reserved = task.reserved_bytes;
            let bounded_reason = match reason {
                Some(reason) => validate_prefixed_string(reason, "Cancelled: ", limits).is_ok(),
                None => validate_payload("Task cancelled", limits).is_ok(),
            };
            if bounded_reason {
                let requested_status = reason
                    .map(|reason| format!("Cancelled: {reason}"))
                    .unwrap_or_else(|| "Task cancelled".to_string());
                task.input_requests = InputRequests::new();
                task.status = TaskStatus::Cancelled;
                task.status_message = Some(requested_status);
                task.completed_at = Some(Instant::now());
                task.last_updated_at_str = chrono_now_iso8601();
            } else {
                cancel_with_bounded_status(
                    &mut task.task,
                    "Task cancelled: retention limit exceeded",
                    true,
                );
            }

            let candidate_bytes = retained_payload_size(&task.task, limits.max_retained_bytes);
            let totals = candidate_bytes.and_then(|bytes| {
                replacement_totals(
                    &data,
                    old_retained,
                    old_reserved,
                    bytes,
                    0,
                    limits.max_retained_bytes,
                )
                .map(|totals| (bytes, totals))
            });
            let (new_retained, (retained_bytes, reserved_bytes)) = match totals {
                Ok(totals) => totals,
                Err(_) => {
                    cancel_with_bounded_status(
                        &mut task.task,
                        "Task cancelled: retention limit exceeded",
                        true,
                    );
                    let bytes = retained_payload_size(&task.task, limits.max_retained_bytes)
                        .expect("bounded cancellation must fit the aggregate byte limit");
                    let totals = replacement_totals(
                        &data,
                        old_retained,
                        old_reserved,
                        bytes,
                        0,
                        limits.max_retained_bytes,
                    )
                    .expect("bounded cancellation must fit reserved global accounting");
                    (bytes, totals)
                }
            };
            task.retained_bytes = new_retained;
            task.reserved_bytes = 0;
            let notify = task.completion_notify.clone();
            data.retained_bytes = retained_bytes;
            data.reserved_bytes = reserved_bytes;
            let object = task.to_task_object();
            data.tasks.insert(task_id.to_string(), task);
            notify.notify_waiters();
            return Ok(Some(object));
        }
        let object = task.to_task_object();
        data.tasks.insert(task_id.to_string(), task);
        Ok(Some(object))
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
    /// `io.modelcontextprotocol/tasks`, elect to return tasks from ordinary
    /// `tools/call` requests, or serve the final task methods. Legacy
    /// 2025-11-25 task behavior is unaffected either way.
    ///
    /// Adding this cannot change what an existing client sees. Both peers must
    /// declare the extension for it to be negotiated, so a client that did not
    /// keeps receiving the synchronous result.
    ///
    /// ```rust
    /// use tower_mcp::McpRouter;
    ///
    /// let router = McpRouter::new()
    ///     .server_info("my-server", "1.0.0")
    ///     .with_tasks();
    /// ```
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
        ElicitAction, ElicitFieldValue, ElicitFormParams, ElicitFormSchema, ElicitRequestParams,
        ElicitResult, InputRequest, InputResponse, ListRootsParams,
    };

    #[test]
    fn task_resume_context_constructor_preserves_every_field() {
        let input_responses = InputResponses::from([(
            "approval".to_string(),
            InputResponse::Elicit(ElicitResult {
                action: ElicitAction::Accept,
                content: None,
                meta: None,
            }),
        )]);
        let context = TaskResumeContext::new(
            "build_report",
            serde_json::json!({"format": "pdf"}),
            input_responses.clone(),
        );

        assert_eq!(context.tool_name, "build_report");
        assert_eq!(context.arguments, serde_json::json!({"format": "pdf"}));
        assert!(context.cancellation_token.is_none());
        assert_eq!(
            serde_json::to_value(&context.input_responses).unwrap(),
            serde_json::to_value(&input_responses).unwrap()
        );
    }

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
        assert_eq!(info.ttl, Some(300_000));
    }

    #[tokio::test]
    async fn configured_default_ttl_is_used_when_creation_omits_one() {
        let store = MemoryTaskStore::with_config(
            MemoryTaskStoreConfig::default().default_ttl(Duration::from_secs(42)),
        );
        let (id, _) = store
            .create_task("test-tool", serde_json::json!({}), None, None)
            .await
            .unwrap();

        assert_eq!(
            store.get_task(&id).await.unwrap().unwrap().ttl,
            Some(42_000)
        );
    }

    #[tokio::test]
    async fn working_task_expiry_cancels_and_wakes_completion_waiter() {
        let store = MemoryTaskStore::with_config(
            MemoryTaskStoreConfig::default()
                .default_ttl(Duration::from_secs(60))
                .cleanup_interval(Duration::from_secs(60)),
        );
        let (id, cancellation) = store
            .create_task("test-tool", serde_json::json!({}), None, None)
            .await
            .unwrap();

        let waiting_store = store.clone();
        let waiting_id = id.clone();
        let waiter = tokio::spawn(async move {
            waiting_store
                .wait_for_completion(&waiting_id)
                .await
                .unwrap()
        });

        // Poll the waiter to pending while the long initial TTL guarantees
        // that expiry cannot win setup under a stalled CI process.
        let mut waiter = waiter;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiter)
                .await
                .is_err()
        );
        assert!(store.set_ttl(&id, 0).await.unwrap());

        tokio::time::timeout(Duration::from_secs(2), cancellation.cancelled())
            .await
            .expect("expiry did not cancel the task token");
        let snapshot = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("expiry did not wake the completion waiter")
            .unwrap();
        assert!(
            snapshot.is_none(),
            "an expired task has no visible snapshot"
        );
        assert!(matches!(
            store.task_presence(&id).await.unwrap(),
            TaskPresence::Expired { .. }
        ));
        assert!(
            !store
                .complete_task(&id, CallToolResult::text("late"))
                .await
                .unwrap(),
            "a terminal write must not resurrect an expired task"
        );
    }

    #[tokio::test]
    async fn automatic_cleanup_physically_reclaims_expired_tasks() {
        let store = MemoryTaskStore::with_config(
            MemoryTaskStoreConfig::default()
                .default_ttl(Duration::from_secs(60))
                .cleanup_interval(Duration::from_millis(25)),
        );
        let (id, _) = store
            .create_task("test-tool", serde_json::json!({}), None, None)
            .await
            .unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.set_ttl(&id, 0).await.unwrap());

        tokio::time::timeout(Duration::from_secs(2), async {
            while !store.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("automatic cleanup did not reclaim the expired record");
    }

    #[tokio::test]
    async fn shortening_ttl_reschedules_expiry_from_creation() {
        let store = MemoryTaskStore::with_config(
            MemoryTaskStoreConfig::default()
                .default_ttl(Duration::from_secs(60))
                .cleanup_interval(Duration::from_secs(60)),
        );
        let (id, cancellation) = store
            .create_task("test-tool", serde_json::json!({}), None, None)
            .await
            .unwrap();

        assert!(store.set_ttl(&id, 250).await.unwrap());
        tokio::time::timeout(Duration::from_secs(3), cancellation.cancelled())
            .await
            .expect("shorter TTL did not reschedule the expiry wakeup");
        assert!(store.get_task(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn manual_and_scheduled_retirement_signal_expiry_only_once() {
        let store = MemoryTaskStore::with_config(
            MemoryTaskStoreConfig::default()
                .default_ttl(Duration::from_secs(60))
                .cleanup_interval(Duration::from_secs(60)),
        );
        let (id, cancellation) = store
            .create_task("test-tool", serde_json::json!({}), None, None)
            .await
            .unwrap();

        // Mutate under the private store lock without waking the worker. This
        // makes the two retirement passes deterministic while exercising the
        // same one-shot path used by both manual and scheduled cleanup.
        store
            .state
            .data
            .write()
            .unwrap()
            .tasks
            .get_mut(&id)
            .unwrap()
            .ttl = 0;
        let first = store.state.retire_expired(false);
        let second = store.state.retire_expired(false);
        assert_eq!(first.signalled + second.signalled, 1);
        assert!(cancellation.is_cancelled());
        assert_eq!(store.cleanup_expired(), 1);
        assert_eq!(store.cleanup_expired(), 0);
    }

    #[tokio::test]
    async fn dropping_the_last_store_clone_releases_worker_state() {
        let store = MemoryTaskStore::with_config(
            MemoryTaskStoreConfig::default().cleanup_interval(Duration::from_secs(60)),
        );
        store
            .create_task("test-tool", serde_json::json!({}), None, None)
            .await
            .unwrap();
        let weak = Arc::downgrade(&store.state);
        drop(store);
        tokio::time::timeout(Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the expiry worker retained the store's task state");
    }

    #[test]
    fn creating_a_task_does_not_require_a_tokio_runtime() {
        let store = MemoryTaskStore::with_config(
            MemoryTaskStoreConfig::default()
                .default_ttl(Duration::from_secs(60))
                .cleanup_interval(Duration::from_secs(60)),
        );
        let (id, token) = futures::executor::block_on(store.create_task(
            "test-tool",
            serde_json::json!({}),
            None,
            None,
        ))
        .expect("the standard-thread worker should start without Tokio");

        assert!(!token.is_cancelled());
        assert!(
            futures::executor::block_on(store.get_task(&id))
                .unwrap()
                .is_some()
        );
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
        assert_eq!(
            info.status_message.as_deref(),
            Some("Task failed: Something went wrong")
        );
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
    async fn wait_for_completion_does_not_treat_live_cancellation_signal_as_terminal() {
        let store = MemoryTaskStore::new();
        let (id, cancellation) = store
            .create_task("test-tool", serde_json::json!({}), None, None)
            .await
            .unwrap();
        let waiter_store = store.clone();
        let waiter_id = id.clone();
        let mut waiter =
            tokio::spawn(
                async move { waiter_store.wait_for_completion(&waiter_id).await.unwrap() },
            );

        cancellation.cancel();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiter)
                .await
                .is_err(),
            "the cooperative signal alone must not return a working snapshot"
        );

        store
            .cancel_task(&id, Some("teardown complete"))
            .await
            .unwrap();
        let (task, _, _) = waiter.await.unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Cancelled);
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

    /// SEP-2663: a key is spent for the rest of the task once it has been
    /// answered. Treating a reissue as a fresh question, which this store
    /// used to do, breaks the guarantee clients rely on to deduplicate
    /// across polls (#1246).
    #[tokio::test]
    async fn an_answered_key_cannot_be_reissued() {
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

        let error = store
            .require_input(&id, requests(&["approval"]), None)
            .await
            .expect_err("an answered key must not be reissued");
        assert!(
            error.to_string().contains("approval"),
            "the message must name the offending key: {error}"
        );
    }

    /// `requests` replaces the whole snapshot, so an unanswered request has
    /// to be reissued to stay outstanding. Carrying it forward alongside a
    /// new one is not reuse: the key still names the same question.
    #[tokio::test]
    async fn an_outstanding_request_can_be_carried_forward() {
        let store = MemoryTaskStore::new();
        let id = working_task(&store, None).await;
        store
            .require_input(&id, requests(&["approval"]), None)
            .await
            .unwrap();

        // {approval} -> {approval, region}: approval is retained, not reused.
        assert!(
            store
                .require_input(&id, requests(&["approval", "region"]), None)
                .await
                .unwrap()
        );

        let outstanding = store
            .outstanding_input_requests(&id)
            .await
            .unwrap()
            .unwrap();
        assert!(outstanding.contains_key("approval"));
        assert!(outstanding.contains_key("region"));

        // Both still answer normally.
        let applied = store
            .apply_input_responses(
                &id,
                [accept("approval"), accept("region")].into_iter().collect(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            applied.accepted,
            ["approval".to_string(), "region".to_string()].into()
        );
        assert!(applied.is_complete());
    }

    /// Carrying a key forward is only legitimate while it names the same
    /// question. Pointing a live key at a different request is the reuse the
    /// SEP forbids.
    #[tokio::test]
    async fn an_outstanding_key_cannot_change_what_it_asks() {
        use crate::protocol::{ElicitFormParams, ElicitFormSchema, ElicitRequestParams};

        let store = MemoryTaskStore::new();
        let id = working_task(&store, None).await;
        store
            .require_input(&id, requests(&["approval"]), None)
            .await
            .unwrap();

        let mut changed: InputRequests = Default::default();
        changed.insert(
            "approval".to_string(),
            InputRequest::Elicit(ElicitRequestParams::Form(ElicitFormParams {
                mode: None,
                message: "a different question".to_string(),
                requested_schema: ElicitFormSchema::new(),
                meta: None,
            })),
        );
        store
            .require_input(&id, changed, None)
            .await
            .expect_err("a live key must not be repointed at another request");
    }

    /// A key issued and then superseded without an answer is spent too: the
    /// SEP forbids one key naming two distinct requests, regardless of
    /// whether the first was answered.
    #[tokio::test]
    async fn a_superseded_key_cannot_be_reissued() {
        let store = MemoryTaskStore::new();
        let id = working_task(&store, None).await;
        store
            .require_input(&id, requests(&["approval"]), None)
            .await
            .unwrap();
        // Asking something else supersedes the unanswered `approval`.
        store
            .require_input(&id, requests(&["region"]), None)
            .await
            .unwrap();

        store
            .require_input(&id, requests(&["approval"]), None)
            .await
            .expect_err("a superseded key must not be reissued");
    }

    /// Distinct keys are the normal case and stay unaffected, including
    /// asking again after an answer under a new name.
    #[tokio::test]
    async fn distinct_keys_across_rounds_are_fine() {
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

        assert!(
            store
                .require_input(&id, requests(&["approval_2"]), None)
                .await
                .unwrap()
        );
        let applied = store
            .apply_input_responses(&id, [accept("approval_2")].into_iter().collect())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(applied.accepted, ["approval_2".to_string()].into());
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

    fn store_with_limits(limits: TaskRetentionLimits) -> MemoryTaskStore {
        MemoryTaskStore::with_retention_limits(limits)
    }

    fn assert_accounting(store: &MemoryTaskStore) {
        let data = store.state.data.read().unwrap();
        let retained_bytes = data
            .tasks
            .values()
            .map(|task| {
                let measured = retained_payload_size(&task.task, usize::MAX).unwrap();
                assert_eq!(task.retained_bytes, measured);
                measured
            })
            .sum::<usize>();
        let reserved_bytes = data
            .tasks
            .values()
            .map(|task| task.reserved_bytes)
            .sum::<usize>();
        assert_eq!(data.retained_bytes, retained_bytes);
        assert_eq!(data.reserved_bytes, reserved_bytes);
        let expected = data.usage();
        drop(data);
        assert_eq!(store.usage(), expected);
        assert!(store.usage().charged_bytes() <= store.state.retention_limits.max_retained_bytes);
    }

    fn large_request(key: &str, message: String) -> InputRequests {
        [(
            key.to_string(),
            InputRequest::Elicit(ElicitRequestParams::Form(ElicitFormParams {
                mode: None,
                message,
                requested_schema: ElicitFormSchema::new(),
                meta: None,
            })),
        )]
        .into_iter()
        .collect()
    }

    fn accept_text(key: &str, value: String) -> (String, InputResponse) {
        (
            key.to_string(),
            InputResponse::Elicit(ElicitResult::accept(std::collections::HashMap::from([(
                "value".to_string(),
                ElicitFieldValue::String(value),
            )]))),
        )
    }

    #[test]
    fn retention_policy_defaults_are_finite_and_unbounded_is_explicit() {
        assert_eq!(TaskRetentionLimits::new(), TaskRetentionLimits::default());
        let limits = TaskRetentionLimits::default();
        assert_eq!(limits.max_tasks, 1_024);
        assert_eq!(limits.max_payload_bytes, 4 * 1024 * 1024);
        assert_eq!(limits.max_retained_bytes, 64 * 1024 * 1024);

        let unbounded = TaskRetentionLimits::unbounded();
        assert_eq!(unbounded.max_tasks, usize::MAX);
        assert_eq!(unbounded.max_payload_bytes, usize::MAX);
        assert_eq!(unbounded.max_retained_bytes, usize::MAX);

        let synthetic_overflow = TaskStoreUsage {
            retained_bytes: usize::MAX,
            reserved_bytes: 1,
            ..TaskStoreUsage::default()
        };
        assert_eq!(synthetic_overflow.charged_bytes(), usize::MAX);

        // Keep the lifecycle config's original two-field struct-literal API.
        let config = MemoryTaskStoreConfig {
            default_ttl: Duration::from_secs(30),
            cleanup_interval: Duration::from_secs(10),
        };
        let default_limited = MemoryTaskStore::with_config(config);
        assert_eq!(
            default_limited.state.retention_limits,
            TaskRetentionLimits::default()
        );
        let explicitly_unbounded =
            MemoryTaskStore::with_config_and_retention(config, TaskRetentionLimits::unbounded());
        assert_eq!(
            explicitly_unbounded.state.retention_limits,
            TaskRetentionLimits::unbounded()
        );
    }

    #[test]
    fn payload_validation_uses_one_stricter_counting_pass() {
        struct Counted<'a>(&'a std::sync::atomic::AtomicUsize);

        impl serde::Serialize for Counted<'_> {
            fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                serializer.serialize_str("payload")
            }
        }

        let calls = std::sync::atomic::AtomicUsize::new(0);
        let limits = TaskRetentionLimits::unbounded().max_retained_bytes(64);
        assert_eq!(validate_payload(&Counted(&calls), limits).unwrap(), 9);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);

        let tied = TaskRetentionLimits::unbounded()
            .max_payload_bytes(1)
            .max_retained_bytes(1);
        assert!(matches!(
            validate_payload("x", tied),
            Err(TaskStoreError::RetentionLimitExceeded {
                kind: TaskRetentionLimitKind::PayloadBytes,
                limit: 1
            })
        ));
        let aggregate_is_stricter = TaskRetentionLimits::unbounded().max_retained_bytes(4);
        assert!(matches!(
            validate_prefixed_string("x", "prefix: ", aggregate_is_stricter),
            Err(TaskStoreError::RetentionLimitExceeded {
                kind: TaskRetentionLimitKind::AggregateBytes,
                limit: 4
            })
        ));
    }

    #[tokio::test]
    async fn payload_limit_accepts_exact_boundary_and_rejects_without_mutation() {
        let arguments = serde_json::json!({"data": "abcd"});
        let exact = serde_json::to_vec(&arguments).unwrap().len();
        let store = store_with_limits(TaskRetentionLimits::unbounded().max_payload_bytes(exact));
        store
            .create_task("tool", arguments.clone(), None, None)
            .await
            .expect("the exact encoded-byte boundary is inclusive");
        let before = store.usage();

        let error = store
            .create_task("tool", serde_json::json!({"data": "abcde"}), None, None)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            TaskStoreError::RetentionLimitExceeded {
                kind: TaskRetentionLimitKind::PayloadBytes,
                limit
            } if limit == exact
        ));
        assert_eq!(store.usage(), before);
        assert_accounting(&store);

        let zero = store_with_limits(TaskRetentionLimits::unbounded().max_payload_bytes(0));
        assert!(matches!(
            zero.create_task("tool", serde_json::Value::Null, None, None)
                .await,
            Err(TaskStoreError::RetentionLimitExceeded {
                kind: TaskRetentionLimitKind::PayloadBytes,
                limit: 0
            })
        ));
    }

    #[tokio::test]
    async fn oversized_owner_rejects_create_atomically() {
        let store = store_with_limits(
            TaskRetentionLimits::unbounded()
                .max_payload_bytes(64)
                .max_retained_bytes(4 * 1024),
        );
        let error = store
            .create_task(
                "tool",
                serde_json::Value::Null,
                None,
                Some("owner-secret".repeat(128)),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            TaskStoreError::RetentionLimitExceeded {
                kind: TaskRetentionLimitKind::PayloadBytes,
                limit: 64
            }
        ));
        assert_eq!(store.usage(), TaskStoreUsage::default());
        assert_accounting(&store);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_task_count_admission_never_overshoots() {
        let store = store_with_limits(TaskRetentionLimits::unbounded().max_tasks(1));
        let barrier = Arc::new(tokio::sync::Barrier::new(17));
        let mut creates = Vec::new();
        for _ in 0..16 {
            let store = store.clone();
            let barrier = barrier.clone();
            creates.push(tokio::spawn(async move {
                barrier.wait().await;
                store
                    .create_task("tool", serde_json::json!({}), None, None)
                    .await
            }));
        }
        barrier.wait().await;

        let mut accepted = 0;
        for create in creates {
            match create.await.unwrap() {
                Ok(_) => accepted += 1,
                Err(TaskStoreError::RetentionLimitExceeded {
                    kind: TaskRetentionLimitKind::TaskCount,
                    limit: 1,
                }) => {}
                Err(other) => panic!("unexpected create error: {other}"),
            }
        }
        assert_eq!(accepted, 1);
        assert_eq!(store.usage().task_count, 1);
        assert_accounting(&store);
    }

    #[tokio::test]
    async fn replacement_and_input_accumulation_account_exactly() {
        let store = store_with_limits(TaskRetentionLimits::unbounded());
        let id = working_task(&store, None).await;
        let initial = store.usage();

        assert!(
            store
                .set_task_meta(&id, serde_json::json!({"note": "x".repeat(2_000)}))
                .await
                .unwrap()
        );
        let large_meta = store.usage();
        assert!(large_meta.retained_bytes > initial.retained_bytes);
        assert_accounting(&store);

        assert!(
            store
                .set_task_meta(&id, serde_json::json!({"note": "small"}))
                .await
                .unwrap()
        );
        let small_meta = store.usage();
        assert!(small_meta.retained_bytes < large_meta.retained_bytes);

        store
            .require_input(&id, large_request("first", "question".repeat(20)), None)
            .await
            .unwrap();
        let requested = store.usage();
        assert!(requested.retained_bytes > small_meta.retained_bytes);
        store
            .apply_input_responses(
                &id,
                [accept_text("first", "answer".repeat(30))]
                    .into_iter()
                    .collect(),
            )
            .await
            .unwrap()
            .unwrap();
        let first_answer = store.usage();
        assert_accounting(&store);

        store
            .require_input(&id, large_request("second", "next".repeat(20)), None)
            .await
            .unwrap();
        store
            .apply_input_responses(
                &id,
                [accept_text("second", "more".repeat(40))]
                    .into_iter()
                    .collect(),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(store.usage().retained_bytes > first_answer.retained_bytes);
        assert_accounting(&store);
    }

    #[tokio::test]
    async fn rejected_payload_mutations_are_atomic_and_ignored_input_is_not_charged() {
        let limits = TaskRetentionLimits::unbounded().max_payload_bytes(256);
        let store = store_with_limits(limits);
        let id = working_task(&store, None).await;

        let before_meta = store.usage();
        assert!(matches!(
            store
                .set_task_meta(&id, serde_json::json!({"secret": "m".repeat(1_024)}))
                .await,
            Err(TaskStoreError::RetentionLimitExceeded {
                kind: TaskRetentionLimitKind::PayloadBytes,
                ..
            })
        ));
        assert_eq!(store.usage(), before_meta);

        assert!(matches!(
            store
                .require_input(&id, large_request("large", "q".repeat(1_024)), None)
                .await,
            Err(TaskStoreError::RetentionLimitExceeded {
                kind: TaskRetentionLimitKind::PayloadBytes,
                ..
            })
        ));
        assert_eq!(
            store.get_task(&id).await.unwrap().unwrap().status,
            TaskStatus::Working
        );

        store
            .require_input(&id, requests(&["approval"]), None)
            .await
            .unwrap();
        let accepted = store
            .apply_input_responses(
                &id,
                [
                    accept("approval"),
                    accept_text("ignored", "never-retained".repeat(1_024)),
                ]
                .into_iter()
                .collect(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(accepted.accepted, ["approval".to_string()].into());
        assert_eq!(accepted.ignored, ["ignored".to_string()].into());
        assert_accounting(&store);

        let other = working_task(&store, None).await;
        store
            .require_input(&other, requests(&["approval"]), None)
            .await
            .unwrap();
        let before_response = store.usage();
        assert!(matches!(
            store
                .apply_input_responses(
                    &other,
                    [accept_text("approval", "secret".repeat(1_024))]
                        .into_iter()
                        .collect(),
                )
                .await,
            Err(TaskStoreError::RetentionLimitExceeded {
                kind: TaskRetentionLimitKind::PayloadBytes,
                ..
            })
        ));
        assert_eq!(store.usage(), before_response);
        assert_eq!(
            store
                .outstanding_input_requests(&other)
                .await
                .unwrap()
                .unwrap()
                .len(),
            1
        );
        assert_accounting(&store);
    }

    #[tokio::test]
    async fn oversized_terminal_result_records_bounded_failure_and_wakes_waiter() {
        let store = store_with_limits(
            TaskRetentionLimits::unbounded()
                .max_payload_bytes(256)
                .max_retained_bytes(8 * 1024),
        );
        let id = working_task(&store, None).await;
        let waiting_store = store.clone();
        let waiting_id = id.clone();
        let mut waiter = tokio::spawn(async move {
            waiting_store
                .wait_for_completion(&waiting_id)
                .await
                .unwrap()
                .unwrap()
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiter)
                .await
                .is_err()
        );

        let secret = "terminal-secret".repeat(1_024);
        let error = store
            .complete_task(&id, CallToolResult::text(&secret))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            TaskStoreError::RetentionLimitExceeded {
                kind: TaskRetentionLimitKind::PayloadBytes,
                limit: 256
            }
        ));

        let (task, result, error) = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("bounded failure did not wake completion waiter")
            .unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(
            task.status_message.as_deref(),
            Some(RETENTION_FAILURE_STATUS)
        );
        assert!(result.is_none());
        assert_eq!(error.unwrap().message, RETENTION_FAILURE_MESSAGE);
        let data = store.state.data.read().unwrap();
        let stored = data.tasks.get(&id).unwrap();
        assert_eq!(stored.reserved_bytes, 0);
        assert!(!format!("{:?}", stored.task).contains("terminal-secret"));
        drop(data);
        assert_accounting(&store);
    }

    #[tokio::test]
    async fn oversized_failure_and_cancel_reason_store_only_bounded_terminal_state() {
        let store = store_with_limits(
            TaskRetentionLimits::unbounded()
                .max_payload_bytes(128)
                .max_retained_bytes(8 * 1024),
        );
        let failed = working_task(&store, None).await;
        let secret = "diagnostic-secret".repeat(1_024);
        assert!(matches!(
            store
                .fail_task(&failed, JsonRpcError::internal_error(&secret))
                .await,
            Err(TaskStoreError::RetentionLimitExceeded { .. })
        ));
        let (task, _, error) = store.get_task_result(&failed).await.unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(error.unwrap().message, RETENTION_FAILURE_MESSAGE);

        let cancelled = working_task(&store, None).await;
        let object = store
            .cancel_task(&cancelled, Some(&secret))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(object.status, TaskStatus::Cancelled);
        assert_eq!(
            object.status_message.as_deref(),
            Some("Task cancelled: retention limit exceeded")
        );
        let data = store.state.data.read().unwrap();
        assert!(
            !format!("{:?}", data.tasks.get(&failed).unwrap().task).contains("diagnostic-secret")
        );
        assert!(
            !format!("{:?}", data.tasks.get(&cancelled).unwrap().task)
                .contains("diagnostic-secret")
        );
        drop(data);
        assert_accounting(&store);
    }

    fn working_charge(tool: &str, arguments: serde_json::Value) -> usize {
        let limits = TaskRetentionLimits::unbounded();
        let task = Task::new(
            "prototype".to_string(),
            tool.to_string(),
            arguments,
            60_000,
            None,
        );
        let stored = prepare_stored_task(task, limits).unwrap();
        stored.retained_bytes + stored.reserved_bytes
    }

    #[tokio::test]
    async fn aggregate_capacity_recovers_after_deletion_and_expiry() {
        let charge = working_charge("tool", serde_json::json!({}));
        let limits = TaskRetentionLimits::unbounded()
            .max_tasks(2)
            .max_retained_bytes(charge);
        let store = store_with_limits(limits);
        let first = working_task(&store, None).await;
        assert_eq!(store.usage().charged_bytes(), charge);
        assert!(matches!(
            store
                .create_task("tool", serde_json::json!({}), None, None)
                .await,
            Err(TaskStoreError::RetentionLimitExceeded {
                kind: TaskRetentionLimitKind::AggregateBytes,
                limit
            }) if limit == charge
        ));
        assert_accounting(&store);
        assert!(store.discard_task(&first).await.unwrap());
        assert_eq!(store.usage(), TaskStoreUsage::default());

        let expired = working_task(&store, None).await;
        let before_expiry = store.usage();
        assert!(matches!(
            store
                .create_task("tool", serde_json::json!({}), None, None)
                .await,
            Err(TaskStoreError::RetentionLimitExceeded {
                kind: TaskRetentionLimitKind::AggregateBytes,
                limit
            }) if limit == charge
        ));
        assert_accounting(&store);
        assert!(store.set_ttl(&expired, 0).await.unwrap());
        let tombstone = store.usage();
        assert_eq!(tombstone.task_count, 1);
        assert_eq!(tombstone.reserved_bytes, 0);
        assert!(tombstone.retained_bytes < before_expiry.retained_bytes);
        assert!(matches!(
            store.task_presence(&expired).await.unwrap(),
            TaskPresence::Expired { .. }
        ));
        assert_accounting(&store);

        let replacement = working_task(&store, None).await;
        assert_ne!(replacement, expired);
        assert!(matches!(
            store.task_presence(&expired).await.unwrap(),
            TaskPresence::Missing
        ));
        assert_eq!(store.usage().task_count, 1);
        assert_eq!(store.usage().charged_bytes(), charge);
        assert_accounting(&store);
    }

    #[tokio::test]
    async fn expiry_accounting_allows_scrubbed_encoding_to_grow() {
        let store = MemoryTaskStore::with_config_and_retention(
            MemoryTaskStoreConfig::default().cleanup_interval(Duration::from_secs(60)),
            TaskRetentionLimits::unbounded(),
        );
        let (id, _) = store
            .create_task("x", serde_json::Value::Null, None, None)
            .await
            .unwrap();
        assert!(
            store
                .set_status(&id, TaskStatus::Working, Some(""))
                .await
                .unwrap()
        );
        let before_expiry = store.usage();

        assert!(store.set_ttl(&id, 0).await.unwrap());
        let after_expiry = store.usage();
        assert_eq!(after_expiry.task_count, 1);
        assert_eq!(after_expiry.reserved_bytes, 0);
        assert_eq!(
            after_expiry.retained_bytes,
            before_expiry.retained_bytes + 1,
            "the empty status encodes two bytes smaller than None, while the one-byte tool name is scrubbed"
        );
        assert!(matches!(
            store.task_presence(&id).await.unwrap(),
            TaskPresence::Expired { .. }
        ));
        assert_accounting(&store);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_completions_atomically_enforce_aggregate_limit() {
        let result_text = "r".repeat(2_048);
        let unbounded = TaskRetentionLimits::unbounded();
        let working = prepare_stored_task(
            Task::new(
                "prototype".to_string(),
                "tool".to_string(),
                serde_json::json!({}),
                60_000,
                None,
            ),
            unbounded,
        )
        .unwrap();
        let mut completed = Task::new(
            "prototype".to_string(),
            "tool".to_string(),
            serde_json::json!({}),
            60_000,
            None,
        );
        completed.status = TaskStatus::Completed;
        completed.status_message = Some("Task completed".to_string());
        completed.result = Some(CallToolResult::text(&result_text));
        completed.completed_at = Some(Instant::now());
        let completed = prepare_stored_task(completed, unbounded).unwrap();
        let working_charge = working.retained_bytes + working.reserved_bytes;
        assert!(completed.retained_bytes > working_charge);
        let cap = completed.retained_bytes + working_charge;
        let store = store_with_limits(
            TaskRetentionLimits::unbounded()
                .max_tasks(2)
                .max_payload_bytes(4 * 1024)
                .max_retained_bytes(cap),
        );
        let first = working_task(&store, None).await;
        let second = working_task(&store, None).await;
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let mut completions = Vec::new();
        for id in [first.clone(), second.clone()] {
            let store = store.clone();
            let barrier = barrier.clone();
            let result_text = result_text.clone();
            completions.push(tokio::spawn(async move {
                barrier.wait().await;
                store
                    .complete_task(&id, CallToolResult::text(result_text))
                    .await
            }));
        }
        barrier.wait().await;

        let mut completed_count = 0;
        let mut rejected_count = 0;
        for completion in completions {
            match completion.await.unwrap() {
                Ok(true) => completed_count += 1,
                Err(TaskStoreError::RetentionLimitExceeded {
                    kind: TaskRetentionLimitKind::AggregateBytes,
                    limit,
                }) if limit == cap => rejected_count += 1,
                other => panic!("unexpected completion outcome: {other:?}"),
            }
        }
        assert_eq!((completed_count, rejected_count), (1, 1));
        let states = [
            store.get_task(&first).await.unwrap().unwrap().status,
            store.get_task(&second).await.unwrap().unwrap().status,
        ];
        assert_eq!(
            states
                .iter()
                .filter(|status| **status == TaskStatus::Completed)
                .count(),
            1
        );
        assert_eq!(
            states
                .iter()
                .filter(|status| **status == TaskStatus::Failed)
                .count(),
            1
        );
        assert_accounting(&store);
    }
}
