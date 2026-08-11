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
//! [`MemoryTaskStore`], which keeps tasks in an in-process map (behavior
//! identical to earlier versions). External stores (Redis, Postgres, etc.) can
//! be plugged in so `tasks/get` works on any instance behind a load balancer
//! in the sessionless 2026-07-28 flows (SEP-2663).
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
//! never existed.
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
//! [`AuthConfig::public_path`](crate::auth::AuthConfig::public_path)) should
//! expect a task created anonymously to be unreachable once a token is
//! presented.
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
///
/// The record [`MemoryTaskStore`] keeps, exposed because its fields describe
/// what a store has to track. There is no public constructor: tasks come from
/// [`TaskStore::create_task`], and an external store is free to persist an
/// entirely different shape as long as it answers the trait's methods the same
/// way.
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
            meta: None,
            result: None,
            error: None,
            owner,
            input_requests: InputRequests::new(),
            answered_input_keys: BTreeSet::new(),
            input_responses: InputResponses::new(),
            superseded_input_keys: BTreeSet::new(),
            cancellation_token: CancellationToken { cancelled },
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
}

impl AppliedInputResponses {
    /// Whether every outstanding request has now been answered.
    pub fn is_complete(&self) -> bool {
        self.still_outstanding.is_empty()
    }
}

/// A shareable cancellation token for task management
///
/// Handed back by [`TaskStore::create_task`] and raised by
/// [`TaskStore::cancel_task`]. It is cooperative: setting it does not
/// interrupt anything, so long-running work has to check it between steps to
/// notice. Every clone observes the same flag, and it is never lowered again.
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
    /// until the task completes, fails, or is cancelled. Returns `None` if
    /// the task is unknown.
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

    /// Everything needed to re-invoke a task's handler after its input
    /// requests were answered.
    ///
    /// Returning `None` means this store cannot resume, and the router fails
    /// the task with a message saying so rather than leaving it in `working`
    /// forever. The default returns `None` so an external store written
    /// before resumption existed keeps compiling and fails loudly instead of
    /// hanging; implement it to support the flow (#1208).
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

    async fn set_task_meta(&self, task_id: &str, meta: serde_json::Value) -> Result<bool> {
        let Ok(mut tasks) = self.tasks.write() else {
            return Ok(false);
        };
        let Some(task) = tasks.get_mut(task_id).filter(|task| !task.is_expired()) else {
            return Ok(false);
        };
        task.meta = Some(meta);
        Ok(true)
    }

    async fn discard_task(&self, task_id: &str) -> Result<bool> {
        Ok(self
            .tasks
            .write()
            .ok()
            .and_then(|mut tasks| tasks.remove(task_id))
            .is_some())
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
        Ok(Some(applied))
    }

    async fn resume_context(&self, task_id: &str) -> Result<Option<TaskResumeContext>> {
        let Ok(tasks) = self.tasks.read() else {
            return Ok(None);
        };
        Ok(tasks
            .get(task_id)
            .filter(|task| !task.is_expired())
            .map(|task| TaskResumeContext {
                tool_name: task.tool_name.clone(),
                arguments: task.arguments.clone(),
                input_responses: task.input_responses.clone(),
            }))
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
}
