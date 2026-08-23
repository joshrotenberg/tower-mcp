//! The identity and state of one task-backed tool execution.
//!
//! A live tool is invoked as a Task rather than answered inline, so it needs
//! somewhere to carry the task id, the outstanding input requests, and the
//! outcome. These grow with the Tasks extension, which is why they are their
//! own module (#1256).

use super::*;

/// Identity allocated for one task-backed tool execution.
///
/// The same value is supplied to task preparation and inserted into the
/// background handler's request extensions.
pub struct TaskContext {
    task_id: String,
    live: Option<Arc<LiveTask>>,
    cancellation: Option<Arc<crate::task_execution::LiveTaskCancellation>>,
}

impl std::fmt::Debug for TaskContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskContext")
            .field("task_id", &self.task_id)
            .field("live", &self.live.is_some())
            .field("cancellable", &self.cancellation.is_some())
            .finish()
    }
}

/// Identity comparison. A task is its id; the live handle is machinery.
impl PartialEq for TaskContext {
    fn eq(&self, other: &Self) -> bool {
        self.task_id == other.task_id
    }
}

impl Eq for TaskContext {}

/// What a live handler needs in order to park and be woken.
///
/// Held by both the running handler, through its [`TaskContext`], and the
/// router, which signals it once `tasks/update` has committed.
pub(crate) struct LiveTask {
    pub(crate) store: Arc<dyn crate::async_task::TaskStore>,
    pub(crate) error_policy: crate::router::TaskErrorPolicy,
    /// Signalled after responses are durably recorded, never before.
    pub(crate) input_ready: tokio::sync::Notify,
    pub(crate) cancellation: Arc<crate::task_execution::LiveTaskCancellation>,
}

/// How a live task ended.
///
/// The handler returns this and the router applies it. Nothing else writes
/// terminal state, so completion cannot race the handler and no transition
/// needs a compare-and-swap. It also makes "returned without terminalizing"
/// unrepresentable: the return type is the terminal state (#1246).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum TaskOutcome {
    /// The tool ran and produced a result.
    ///
    /// A result carrying `isError: true` still completes the task: the tool
    /// ran and reported a domain error, which SEP-2663 distinguishes from an
    /// execution failure.
    Completed(CallToolResult),
    /// Execution failed. The structured error reaches `tasks/get` intact.
    Failed(crate::error::JsonRpcError),
    /// The handler observed cancellation and finished unwinding.
    ///
    /// Returned by the handler rather than imposed by the store, so a task
    /// stays non-terminal between the `tasks/cancel` acknowledgement and the
    /// worker confirming it stopped. If `message` is absent, the router uses
    /// the reason from the first client or host cancellation request.
    Cancelled {
        /// Optional detail for the task's status message.
        message: Option<String>,
    },
}

impl TaskContext {
    pub(crate) fn new(task_id: String) -> Self {
        Self {
            task_id,
            live: None,
            cancellation: None,
        }
    }

    pub(crate) fn with_cancellation(
        task_id: String,
        cancellation: Arc<crate::task_execution::LiveTaskCancellation>,
    ) -> Self {
        Self {
            task_id,
            live: None,
            cancellation: Some(cancellation),
        }
    }

    pub(crate) fn with_live(task_id: String, live: Arc<LiveTask>) -> Self {
        let cancellation = live.cancellation.clone();
        Self {
            task_id,
            live: Some(live),
            cancellation: Some(cancellation),
        }
    }

    /// The server-generated task identifier.
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// Whether this context can park and await client input.
    ///
    /// True only inside a live handler. A replay handler returns
    /// `RequestOutcome::InputRequired` instead and is re-invoked once the
    /// answers arrive.
    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }

    /// Ask the client for input and wait for it.
    ///
    /// Records the requests, parks the task in `input_required`, and returns
    /// once every one of them is answered. The handler future stays alive
    /// throughout, so whatever it owns (a subprocess, a stream, an in-flight
    /// request) is still there when this returns (#1246).
    ///
    /// Only the answers to `requests` come back, keyed as they were sent.
    /// Earlier answers stay in the store rather than being handed over again.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::TaskCancelled`] if the
    /// task is cancelled while waiting, so a handler that propagates with `?`
    /// unwinds correctly without writing a `select!`. The router maps that
    /// error to [`TaskOutcome::Cancelled`].
    ///
    /// Request keys must be unique over the task's lifetime (SEP-2663), so
    /// reusing a spent key is an error rather than a fresh question.
    pub async fn require_input(&self, requests: InputRequests) -> Result<InputResponses> {
        self.park_input(requests).await?.wait().await
    }

    /// [`require_input`](Self::require_input) with a status message for
    /// clients polling the task.
    pub async fn require_input_with_message(
        &self,
        requests: InputRequests,
        message: impl Into<String>,
    ) -> Result<InputResponses> {
        self.park_input_with_message(requests, message)
            .await?
            .wait()
            .await
    }

    /// Commit the request for input, and return without waiting for it.
    ///
    /// The first half of [`require_input`](Self::require_input), which is
    /// exactly these two calls back to back:
    ///
    /// ```rust,no_run
    /// # use tower_mcp::{InputRequests, InputResponses, TaskContext};
    /// # async fn example(ctx: TaskContext, requests: InputRequests)
    /// #     -> tower_mcp::Result<InputResponses> {
    /// let pending = ctx.park_input(requests).await?;
    /// // Whatever has to happen once the task is durably parked but before
    /// // this handler suspends: release admission permits, drop a lock, hand
    /// // a worker slot back to a scheduler.
    /// let responses = pending.wait().await?;
    /// # Ok(responses)
    /// # }
    /// ```
    ///
    /// The split exists because those two things are not the same moment. An
    /// execution owner running under admission control has to release its
    /// permits *after* the `input_required` state is durable and *before* it
    /// suspends, and a single combined call gives it nowhere to stand
    /// (#1246).
    ///
    /// # The gap is safe
    ///
    /// Arbitrary code runs between this returning and
    /// [`PendingInput::wait`], including code that awaits. A response or a
    /// cancellation arriving in that window is not lost:
    ///
    /// - `wait` reads outstanding requests from the store before it awaits
    ///   anything, and the store is what `tasks/update` commits to. Answers
    ///   that landed in the gap are already there.
    /// - The wakeup is a [`tokio::sync::Notify`] signalled with `notify_one`,
    ///   which stores a permit when nobody is waiting. A signal in the gap is
    ///   held, not dropped.
    /// - Cancellation is a flag rather than an event, so `wait` observes one
    ///   that was set in the gap.
    ///
    /// # Errors
    ///
    /// Same as [`require_input`](Self::require_input) for the commit half: a
    /// replay handler, an empty request set, a task already cancelled, or a
    /// store that refuses the transition.
    pub async fn park_input(&self, requests: InputRequests) -> Result<PendingInput> {
        self.park_input_inner(requests, None).await
    }

    /// [`park_input`](Self::park_input) with a status message for clients
    /// polling the task.
    pub async fn park_input_with_message(
        &self,
        requests: InputRequests,
        message: impl Into<String>,
    ) -> Result<PendingInput> {
        self.park_input_inner(requests, Some(message.into())).await
    }

    async fn park_input_inner(
        &self,
        requests: InputRequests,
        message: Option<String>,
    ) -> Result<PendingInput> {
        let live = self.live.as_ref().ok_or_else(|| {
            crate::error::Error::Tool(crate::error::ToolError::new(
                "require_input needs a live task handler; a replay handler returns RequestOutcome::InputRequired instead",
            ))
        })?;
        if requests.is_empty() {
            return Err(crate::error::Error::JsonRpc(
                live.error_policy.map_internal_error(
                    crate::router::TaskOperation::ParkInput,
                    &self.task_id,
                    "require_input needs at least one request, or the task would wait for something that can never arrive",
                ),
            ));
        }
        let asked: Vec<String> = requests.keys().cloned().collect();

        if live.cancellation.is_cancelled() {
            return Err(crate::error::Error::TaskCancelled);
        }

        let accepted = live
            .store
            .require_input(&self.task_id, requests, message.as_deref())
            .await
            .map_err(|error| {
                crate::error::Error::JsonRpc(live.error_policy.map_store_error(
                    crate::router::TaskOperation::ParkInput,
                    &self.task_id,
                    error,
                ))
            })?;
        if !accepted {
            return Err(crate::error::Error::JsonRpc(
                live.error_policy.map_internal_error(
                    crate::router::TaskOperation::ParkInput,
                    &self.task_id,
                    "the task is already terminal, so it cannot ask for input",
                ),
            ));
        }

        Ok(PendingInput {
            live: live.clone(),
            task_id: self.task_id.clone(),
            asked,
        })
    }

    /// Record a non-terminal status for clients polling this task.
    pub async fn working(&self, message: impl Into<String>) -> Result<()> {
        let live = self.live.as_ref().ok_or_else(|| {
            crate::error::Error::Tool(crate::error::ToolError::new(
                "working needs a live task handler",
            ))
        })?;
        let updated = live
            .store
            .set_status(&self.task_id, TaskStatus::Working, Some(&message.into()))
            .await
            .map_err(|error| {
                crate::error::Error::JsonRpc(live.error_policy.map_store_error(
                    crate::router::TaskOperation::Execute,
                    &self.task_id,
                    error,
                ))
            })?;
        if !updated {
            return Err(crate::error::Error::JsonRpc(
                live.error_policy.map_internal_error(
                    crate::router::TaskOperation::Execute,
                    &self.task_id,
                    "the task is already terminal, so its status cannot be updated",
                ),
            ));
        }
        Ok(())
    }

    /// Whether this task has been asked to cancel.
    ///
    /// Available during live-task preparation as well as handler execution.
    /// A live task stays non-terminal until its handler returns, so this being
    /// true means the request arrived, not that the task is over.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(|cancellation| cancellation.is_cancelled())
    }

    /// Return the reason supplied with the first cancellation request.
    ///
    /// Both client `tasks/cancel` reasons and host reasons from
    /// [`crate::LiveTaskExecutionHandle::cancel_all`] reach this method. The
    /// returned string is a snapshot because cancellation state is shared
    /// across the handler and host control handle.
    pub fn cancellation_reason(&self) -> Option<String> {
        self.cancellation
            .as_ref()
            .and_then(|cancellation| cancellation.reason())
    }

    /// Resolves when this task is asked to cancel.
    ///
    /// A live handler uses this when it interleaves teardown with its own
    /// work. Task preparation receives the same signal, so a cooperative
    /// preparer can release resources and return when a host cancels its
    /// anonymous admission reservation. Replay contexts have no live
    /// cancellation source and wait forever here.
    ///
    /// A handler that awaits [`require_input`](Self::require_input) gets
    /// correct behaviour from the error it returns without calling this.
    pub async fn cancelled(&self) {
        match self.cancellation.as_ref() {
            Some(cancellation) => cancellation.cancelled().await,
            None => std::future::pending().await,
        }
    }
}

impl Clone for TaskContext {
    fn clone(&self) -> Self {
        Self {
            task_id: self.task_id.clone(),
            live: self.live.clone(),
            cancellation: self.cancellation.clone(),
        }
    }
}

/// A task durably parked in `input_required`, not yet waited on.
///
/// Returned by [`TaskContext::park_input`]. The task is already parked when
/// this exists: the store transition has committed and a client polling
/// `tasks/get` can already see the questions. What has not happened yet is
/// this handler suspending, which is the point of the split (#1246).
///
/// Deliberately not [`Clone`]. Two holders would both wait on one set of
/// answers and one of them would consume them, so the type makes a second
/// waiter unrepresentable rather than leaving it to a comment.
#[must_use = "the task is parked in `input_required` until this is awaited; \
              dropping it leaves the task parked with nothing waiting to \
              resume it"]
pub struct PendingInput {
    pub(super) live: Arc<LiveTask>,
    pub(super) task_id: String,
    /// The keys this park asked about. Answers to earlier questions stay in
    /// the store rather than being handed over again.
    pub(super) asked: Vec<String>,
}

impl std::fmt::Debug for PendingInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingInput")
            .field("task_id", &self.task_id)
            .field("asked", &self.asked)
            .finish_non_exhaustive()
    }
}

impl PendingInput {
    /// The server-generated task identifier.
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// The request keys this park is waiting on.
    pub fn asked(&self) -> &[String] {
        &self.asked
    }

    /// Suspend until every request in this park is answered.
    ///
    /// Only the answers to the keys this park asked about come back, keyed as
    /// they were sent. A partial answer leaves the rest outstanding and this
    /// keeps waiting rather than reissuing, which would be key reuse
    /// (SEP-2663).
    ///
    /// Takes `self`, so a park cannot be waited on twice.
    ///
    /// # Nothing is lost in the gap
    ///
    /// Arbitrary code, including code that awaits, runs between
    /// [`TaskContext::park_input`] and this call. Three things make that
    /// window safe, and the order below is the order they are relied on:
    ///
    /// 1. Cancellation is a flag, so one raised in the gap is seen here.
    /// 2. The loop reads outstanding requests from the store *before* it
    ///    awaits anything. `tasks/update` commits to that store, so an answer
    ///    that landed in the gap is already visible and this returns without
    ///    suspending at all.
    /// 3. Within the loop the wakeup is created before the read, so an answer
    ///    landing between the read and the suspend is not missed either. The
    ///    wakeup is `notify_one`, which stores a permit when nobody is
    ///    waiting, so it is held rather than dropped.
    ///
    /// # Errors
    ///
    /// [`crate::Error::TaskCancelled`] if the task is cancelled while
    /// waiting, so a handler that propagates with `?` unwinds correctly
    /// without writing a `select!`. The router maps that to
    /// [`TaskOutcome::Cancelled`].
    pub async fn wait(self) -> Result<InputResponses> {
        let live = &self.live;

        if live.cancellation.is_cancelled() {
            return Err(crate::error::Error::TaskCancelled);
        }

        loop {
            // Created before the read, so an answer landing between the two
            // is not missed: `notify_one` holds a permit for us.
            let woken = live.input_ready.notified();

            let outstanding = live
                .store
                .outstanding_input_requests(&self.task_id)
                .await
                .map_err(|error| {
                    crate::error::Error::JsonRpc(live.error_policy.map_store_error(
                        crate::router::TaskOperation::Execute,
                        &self.task_id,
                        error,
                    ))
                })?;
            let Some(outstanding) = outstanding else {
                if live.cancellation.is_cancelled() {
                    return Err(crate::error::Error::TaskCancelled);
                }
                return Err(crate::error::Error::JsonRpc(
                    live.error_policy.map_internal_error(
                        crate::router::TaskOperation::Execute,
                        &self.task_id,
                        "the task disappeared while waiting for input",
                    ),
                ));
            };
            if !outstanding.keys().any(|key| self.asked.contains(key)) {
                break;
            }

            tokio::select! {
                _ = woken => {}
                _ = live.cancellation.cancelled() => {
                    return Err(crate::error::Error::TaskCancelled);
                }
            }
        }

        let all = live
            .store
            .input_responses(&self.task_id)
            .await
            .map_err(|error| {
                crate::error::Error::JsonRpc(live.error_policy.map_store_error(
                    crate::router::TaskOperation::Execute,
                    &self.task_id,
                    error,
                ))
            })?;
        let Some(all) = all else {
            if live.cancellation.is_cancelled() {
                return Err(crate::error::Error::TaskCancelled);
            }
            return Err(crate::error::Error::JsonRpc(
                live.error_policy.map_internal_error(
                    crate::router::TaskOperation::Execute,
                    &self.task_id,
                    "the task disappeared before its input responses could be read",
                ),
            ));
        };
        Ok(all
            .into_iter()
            .filter(|(key, _)| self.asked.contains(key))
            .collect())
    }
}

/// Metadata and application state produced before task execution begins.
#[derive(Debug, Clone, Default)]
pub struct TaskPreparation {
    pub(crate) meta: Option<Map<String, Value>>,
    pub(crate) extensions: Extensions,
}

impl TaskPreparation {
    /// Create an empty preparation result.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach protocol `_meta` to every view of this task.
    pub fn with_meta(mut self, meta: Map<String, Value>) -> Self {
        self.meta = Some(meta);
        self
    }

    /// Make application state available to the background handler through
    /// [`crate::extract::Extension`].
    pub fn with_extension<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.extensions.insert(value);
        self
    }
}

pub(crate) trait TaskPreparer: Send + Sync {
    fn prepare(
        &self,
        context: TaskContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<TaskPreparation>>;
}

impl<F, Fut> TaskPreparer for F
where
    F: Fn(TaskContext, Value) -> Fut + Send + Sync,
    Fut: Future<Output = Result<TaskPreparation>> + Send + 'static,
{
    fn prepare(
        &self,
        context: TaskContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<TaskPreparation>> {
        Box::pin((self)(context, arguments))
    }
}

pub(super) struct TypedTaskPreparer<I, F> {
    pub(super) prepare: F,
    pub(super) _phantom: std::marker::PhantomData<I>,
}

impl<I, F, Fut> TaskPreparer for TypedTaskPreparer<I, F>
where
    I: DeserializeOwned + Send + Sync + 'static,
    F: Fn(TaskContext, I) -> Fut + Send + Sync,
    Fut: Future<Output = Result<TaskPreparation>> + Send + 'static,
{
    fn prepare(
        &self,
        context: TaskContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<TaskPreparation>> {
        let input = serde_json::from_value(arguments)
            .map_err(|error| Error::invalid_params(format!("Invalid input: {error}")));
        match input {
            Ok(input) => Box::pin((self.prepare)(context, input)),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
}
