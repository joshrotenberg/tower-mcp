//! Task operations for [`McpRouter`](super::McpRouter).
//!
//! The SEP-2663 task surface: authorization, presence classification,
//! `tasks/get` shaping, input parking, and the live-handler registry that
//! backs a non-replay execution (#1246).
//!
//! Split out of `router.rs` in #1256. An `impl` block in a child module, so
//! neither the type nor its API changed. This group went first because it is
//! self-contained and still growing. Not to be confused with
//! [`crate::tasks`], which holds the protocol types these methods build.

use super::*;

type DetailedTaskSnapshot = (
    crate::tasks::DetailedTask,
    Option<serde_json::Map<String, serde_json::Value>>,
);

impl McpRouter {
    pub(super) fn task_json_rpc_error(
        &self,
        operation: TaskOperation,
        task_id: Option<&str>,
        failure: TaskFailure,
    ) -> JsonRpcError {
        let context = TaskErrorContext::new(operation, task_id, failure);
        self.inner.task_error_policy.map(&context)
    }

    pub(super) fn task_error(
        &self,
        operation: TaskOperation,
        task_id: Option<&str>,
        failure: TaskFailure,
    ) -> Error {
        Error::JsonRpc(self.task_json_rpc_error(operation, task_id, failure))
    }

    pub(super) fn task_store_error(
        &self,
        operation: TaskOperation,
        task_id: Option<&str>,
        error: TaskStoreError,
    ) -> Error {
        self.task_error(operation, task_id, TaskFailure::Store(error))
    }

    pub(super) async fn task_presence(
        &self,
        operation: TaskOperation,
        task_id: &str,
    ) -> Result<crate::async_task::TaskPresence> {
        self.inner
            .task_store
            .task_presence(task_id)
            .await
            .map_err(|error| self.task_store_error(operation, Some(task_id), error))
    }

    pub(super) fn classify_absent_presence(
        &self,
        operation: TaskOperation,
        task_id: &str,
        extensions: &crate::context::Extensions,
        presence: crate::async_task::TaskPresence,
    ) -> Error {
        let principal = (self.inner.task_owner_resolver)(extensions);
        let owns = presence
            .owner()
            .is_some_and(|owner| principal.matches(owner));
        match presence {
            crate::async_task::TaskPresence::Expired { .. } if owns => {
                self.task_error(operation, Some(task_id), TaskFailure::Expired)
            }
            _ => self.task_error(operation, Some(task_id), TaskFailure::NotFound),
        }
    }

    /// Classify an operation that found nothing, after authorization passed.
    ///
    /// The task was present when it was authorized, so an operation that then
    /// found nothing means it expired in between. Resolving a second time
    /// tells the owner that, rather than reporting a task that existed moments
    /// ago as though it never had (#1249).
    ///
    /// Ownership is rechecked rather than assumed. The first resolution
    /// established it, and task ids are unguessable and not reused, so this is
    /// belt and braces; it costs one lookup and removes any argument about
    /// whether a store could return a differently-owned record here.
    pub(super) async fn classify_absent_task(
        &self,
        operation: TaskOperation,
        task_id: &str,
        extensions: &crate::context::Extensions,
    ) -> Error {
        let presence = match self.task_presence(operation, task_id).await {
            Ok(presence) => presence,
            Err(error) => return error,
        };
        self.classify_absent_presence(operation, task_id, extensions, presence)
    }

    /// Reject a final task method that was not negotiated by both peers.
    ///
    /// An unnegotiated method is reported as absent rather than forbidden:
    /// the server genuinely does not serve it for this client.
    pub(super) fn require_negotiated_tasks(
        &self,
        extensions: &crate::context::Extensions,
        method: &str,
    ) -> Result<()> {
        if !self.final_tasks_enabled() {
            return Err(Error::JsonRpc(JsonRpcError::method_not_found(method)));
        }
        if client_declares_tasks(extensions) {
            return Ok(());
        }
        Err(Error::JsonRpc(
            JsonRpcError::missing_required_client_capability(tasks_client_capabilities()),
        ))
    }

    /// Verify the caller may act on this task.
    ///
    /// A task the caller does not own is reported exactly as an unknown task.
    /// Distinguishing the two would confirm that an ID is real, which is the
    /// thing unguessable IDs exist to prevent.
    pub(super) async fn authorize_task(
        &self,
        operation: TaskOperation,
        task_id: &str,
        extensions: &crate::context::Extensions,
    ) -> Result<()> {
        // Resolved through presence so an expired task can be reported as
        // such to its owner. Everyone else must still see exactly what they
        // see for an id that was never issued, so the owner check happens
        // before any of that distinction escapes (#1249).
        let presence = self.task_presence(operation, task_id).await?;
        let Some(owner) = presence.owner() else {
            return Err(self.task_error(operation, Some(task_id), TaskFailure::NotFound));
        };

        if (self.inner.task_owner_resolver)(extensions).matches(owner) {
            match presence {
                // The owner is told the difference; nobody else reaches here.
                crate::async_task::TaskPresence::Expired { .. } => {
                    Err(self.task_error(operation, Some(task_id), TaskFailure::Expired))
                }
                _ => Ok(()),
            }
        } else {
            tracing::debug!(
                target: "mcp::tasks",
                task_id = %task_id,
                "task operation refused: principal does not own the task"
            );
            Err(self.task_error(operation, Some(task_id), TaskFailure::NotFound))
        }
    }

    /// Verify the caller may subscribe to status changes for this task.
    ///
    /// Listen filters deliberately collapse missing, expired, and foreign
    /// task IDs into the same not-found response. Unlike a direct task method,
    /// a subscription request must not reveal which candidate IDs were once
    /// valid, even to their former owner.
    #[cfg(feature = "stateless")]
    pub(super) async fn authorize_task_subscription(
        &self,
        task_id: &str,
        extensions: &crate::context::Extensions,
    ) -> Result<()> {
        let owner = self
            .inner
            .task_store
            .task_owner(task_id)
            .await
            .map_err(|error| self.task_store_error(TaskOperation::Get, Some(task_id), error))?;
        let principal = (self.inner.task_owner_resolver)(extensions);
        let allowed = owner.as_ref().is_some_and(|owner| principal.matches(owner));
        if allowed {
            return Ok(());
        }

        tracing::debug!(
            target: "mcp::tasks",
            task_id = %task_id,
            "task subscription refused: task is absent or principal does not own it"
        );
        Err(self.task_error(TaskOperation::Get, Some(task_id), TaskFailure::NotFound))
    }

    /// Serve a final `tasks/get` as a status-discriminated `DetailedTask`.
    pub(super) async fn final_get_task(
        &self,
        task_id: &str,
        extensions: &crate::context::Extensions,
    ) -> Result<McpResponse> {
        let Some((detailed, meta)) = self.detailed_task(task_id).await? else {
            return Err(self
                .classify_absent_task(TaskOperation::Get, task_id, extensions)
                .await);
        };
        let mut result = crate::tasks::GetTaskResult::new(detailed);
        result.meta = meta;
        Ok(McpResponse::FinalGetTask(result))
    }

    /// Build the complete status-discriminated view of a task.
    ///
    /// Both `tasks/get` and `notifications/tasks` render a task through this
    /// one path, which is what makes a pushed notification identical to the
    /// poll response a client would have received at that moment.
    pub(super) async fn detailed_task(
        &self,
        task_id: &str,
    ) -> Result<Option<DetailedTaskSnapshot>> {
        // A final Task view takes two store reads while input is outstanding:
        // the status/result snapshot, then the requests map. `tasks/update`
        // can move the Task back to `working` between them. Re-read once when
        // that race produces an empty map rather than emitting the impossible
        // wire shape `input_required` with no requests.
        for attempt in 0..2 {
            let Some((task, result, error)) = self
                .inner
                .task_store
                .get_task_result(task_id)
                .await
                .map_err(|error| self.task_store_error(TaskOperation::Get, Some(task_id), error))?
            else {
                return Ok(None);
            };

            let mut metadata = crate::tasks::TaskMetadata::new(
                task.task_id.clone(),
                task.created_at.clone(),
                task.last_updated_at.clone(),
                task.ttl,
            );
            metadata.status_message = task.status_message.clone();
            metadata.poll_interval_ms = task.poll_interval;

            let meta = task.meta.and_then(|value| value.as_object().cloned());
            let detailed = match task.status {
                TaskStatus::Working => crate::tasks::DetailedTask::working(metadata),
                TaskStatus::InputRequired => {
                    // Every request still awaiting a response, not just the
                    // most recent one.
                    let outstanding = self
                        .inner
                        .task_store
                        .outstanding_input_requests(task_id)
                        .await
                        .map_err(|error| {
                            self.task_store_error(TaskOperation::Get, Some(task_id), error)
                        })?;
                    let Some(outstanding) = outstanding else {
                        return Ok(None);
                    };
                    if outstanding.is_empty() {
                        if attempt == 0 {
                            continue;
                        }
                        return Err(self.task_error(
                            TaskOperation::Get,
                            Some(task_id),
                            TaskFailure::Internal(
                                "task store returned input_required without outstanding requests",
                            ),
                        ));
                    }
                    crate::tasks::DetailedTask::input_required(metadata, outstanding)
                }
                TaskStatus::Completed => {
                    // The exact object the synchronous call would have returned,
                    // including `isError: true` results.
                    let mut object = result
                        .map(serde_json::to_value)
                        .transpose()
                        .map_err(|_| {
                            self.task_error(
                                TaskOperation::Get,
                                Some(task_id),
                                TaskFailure::Internal("failed to encode task result"),
                            )
                        })?
                        .and_then(|value| value.as_object().cloned())
                        .unwrap_or_default();
                    // This object is nested inside tasks/get, so it does not
                    // pass through the JSON-RPC response stamper that adds the
                    // final protocol's required complete discriminator.
                    object.insert(
                        "resultType".to_string(),
                        serde_json::Value::String("complete".to_string()),
                    );
                    crate::tasks::DetailedTask::completed(metadata, object)
                }
                TaskStatus::Failed => crate::tasks::DetailedTask::failed(
                    metadata,
                    error.unwrap_or_else(|| JsonRpcError::internal_error("Task failed")),
                ),
                TaskStatus::Cancelled => crate::tasks::DetailedTask::cancelled(metadata),
                // `TaskStatus` is non_exhaustive. Report an unrecognized status
                // as working rather than inventing a terminal state.
                _ => crate::tasks::DetailedTask::working(metadata),
            };
            return Ok(Some((detailed, meta)));
        }
        unreachable!("bounded Task snapshot retry always returns")
    }

    /// Park a task on the input its handler asked for.
    ///
    /// The handler has returned; the task waits in `input_required` until the
    /// client answers with `tasks/update`, at which point [`Self::resume_task`]
    /// runs it again (#1208).
    pub(super) async fn park_task_for_input(
        &self,
        task_id: &str,
        input_required: crate::protocol::InputRequiredResult,
    ) {
        let requests = input_required.input_requests.unwrap_or_default();
        if requests.is_empty() {
            // Parking here would strand the task: no `tasks/update` can ever
            // complete an empty request set.
            let error = self.task_json_rpc_error(
                TaskOperation::ParkInput,
                Some(task_id),
                TaskFailure::Internal(
                    "handler asked for input without naming any requests, so the task has \
                     nothing to wait for",
                ),
            );
            if let Err(e) = self.inner.task_store.fail_task(task_id, error).await {
                tracing::warn!(task_id = %task_id, error = %e, "failed to record task failure");
            }
            self.notify_task_state(task_id).await;
            return;
        }

        // A park that does not take leaves the task working with nothing
        // outstanding, which no `tasks/update` can ever move. Failing it says
        // why instead of stranding it, matching the empty-request case above
        // (#1246).
        match self
            .inner
            .task_store
            .require_input(task_id, requests, input_required.request_state.as_deref())
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(
                    task_id = %task_id,
                    "task was already terminal or gone when parking for input"
                );
            }
            Err(e) => {
                // Either way the park is lost and the task can never be
                // answered, so both end it. They are not the same fault
                // though: an invalid transition is the handler asking for
                // something the protocol forbids, which no retry fixes,
                // while a backend failure is infrastructure (#1246).
                match &e {
                    crate::async_task::TaskStoreError::InvalidTransition(message) => {
                        tracing::error!(
                            task_id = %task_id,
                            error = %message,
                            "handler asked for input the protocol does not allow"
                        );
                    }
                    other => {
                        tracing::warn!(
                            task_id = %task_id,
                            error = %other,
                            "task store could not park the task for input"
                        );
                    }
                }
                let error = self.task_json_rpc_error(
                    TaskOperation::ParkInput,
                    Some(task_id),
                    TaskFailure::Store(e),
                );
                if let Err(e) = self.inner.task_store.fail_task(task_id, error).await {
                    tracing::warn!(task_id = %task_id, error = %e, "failed to record task failure");
                }
            }
        }
        self.notify_task_state(task_id).await;
    }

    /// Wake a live task whose input has just been committed.
    ///
    /// Returns false when the task is not live, which is how the caller knows
    /// to fall back to replay (#1246).
    pub(super) fn wake_live_task(&self, task_id: &str) -> bool {
        match self.inner.live_task_executions.registry.get(task_id) {
            Some(handle) => {
                // `notify_one`, not `notify_waiters`: a waiter only registers
                // when its future is polled, so a `notify_waiters` landing
                // between the store write and the await is lost. `notify_one`
                // leaves a permit that the next await consumes.
                handle.input_ready.notify_one();
                true
            }
            None => false,
        }
    }

    /// Signal a live task that cancellation was requested.
    ///
    /// The task stays non-terminal: its handler decides when it has finished
    /// unwinding and says so by returning `TaskOutcome::Cancelled`.
    pub(super) fn signal_live_cancellation(&self, task_id: &str, reason: Option<&str>) -> bool {
        match self.inner.live_task_executions.registry.get(task_id) {
            Some(handle) => {
                handle.cancellation.cancel(reason.map(str::to_owned));
                true
            }
            None => false,
        }
    }

    /// Persist a structured failure without exposing a store error if that
    /// terminal write itself fails.
    pub(super) async fn record_task_failure(&self, task_id: &str, error: JsonRpcError) -> bool {
        match self.inner.task_store.fail_task(task_id, error).await {
            Ok(true) => true,
            Ok(false) => {
                tracing::debug!(
                    task_id = %task_id,
                    "task failure was not applied because the task is absent or terminal"
                );
                false
            }
            Err(_) => {
                tracing::warn!(
                    task_id = %task_id,
                    "failed to record task failure"
                );
                false
            }
        }
    }

    /// Persist cancellation without claiming success for an absent or
    /// already-terminal Task and without displaying a backend error.
    pub(super) async fn record_task_cancellation(
        &self,
        task_id: &str,
        reason: Option<&str>,
    ) -> bool {
        match self.inner.task_store.cancel_task(task_id, reason).await {
            Ok(Some(_)) => true,
            Ok(None) => {
                tracing::debug!(
                    task_id = %task_id,
                    "task cancellation was not applied because the task is absent"
                );
                false
            }
            Err(_) => {
                tracing::warn!(
                    task_id = %task_id,
                    "failed to record task cancellation"
                );
                false
            }
        }
    }

    /// Commit a completed result, converting a backend write failure into one
    /// bounded attempt to persist a structured, policy-mapped Task failure.
    pub(super) async fn complete_task_or_fail(
        &self,
        task_id: &str,
        result: CallToolResult,
    ) -> bool {
        match self.inner.task_store.complete_task(task_id, result).await {
            Ok(true) => true,
            Ok(false) => {
                // A terminal outcome may have won concurrently. Never rewrite
                // it, and do not claim this completion was applied.
                tracing::debug!(
                    task_id = %task_id,
                    "task completion was not applied because the task is absent or terminal"
                );
                false
            }
            Err(error) => {
                tracing::warn!(
                    task_id = %task_id,
                    "failed to record task completion; recording a task failure"
                );
                let error = self.task_json_rpc_error(
                    TaskOperation::Finalize,
                    Some(task_id),
                    TaskFailure::Store(error),
                );
                self.record_task_failure(task_id, error).await;
                false
            }
        }
    }

    /// Re-invoke a task's handler after its input requests were answered.
    ///
    /// A task's client answers through `tasks/update` rather than by retrying
    /// `tools/call`, so the server performs the retry. The handler runs from
    /// the top with the accumulated answers readable through
    /// `RequestContext::input_responses`, exactly as a non-task MRTR handler
    /// sees them on the client's retry.
    pub(super) async fn resume_task(&self, task_id: &str) {
        let resume = match self.inner.task_store.resume_context(task_id).await {
            Ok(Some(resume)) => resume,
            Ok(None) => {
                // Either the task vanished, or the store predates resumption
                // and cannot supply what a re-invocation needs. Fail loudly
                // rather than leave the task working forever.
                let error = self.task_json_rpc_error(
                    TaskOperation::Resume,
                    Some(task_id),
                    TaskFailure::Internal(
                        "this task store cannot resume a task after input was provided; \
                     implement TaskStore::resume_context to support handlers that ask \
                     for input",
                    ),
                );
                self.record_task_failure(task_id, error).await;
                self.notify_task_state(task_id).await;
                return;
            }
            Err(error) => {
                tracing::warn!(task_id = %task_id, "failed to read resume context");
                let error = self.task_json_rpc_error(
                    TaskOperation::Resume,
                    Some(task_id),
                    TaskFailure::Store(error),
                );
                self.record_task_failure(task_id, error).await;
                self.notify_task_state(task_id).await;
                return;
            }
        };

        // Static tools first, then dynamic, matching `tools/call`.
        let tool = self.inner.tools.get(&resume.tool_name).cloned();
        #[cfg(feature = "dynamic-tools")]
        let tool = tool.or_else(|| {
            self.inner
                .dynamic_tools
                .as_ref()
                .and_then(|d| d.get(&resume.tool_name))
        });
        let Some(tool) = tool else {
            tracing::warn!(
                task_id = %task_id,
                tool = %resume.tool_name,
                "task cannot resume because its tool is no longer registered"
            );
            let error = self.task_json_rpc_error(
                TaskOperation::Resume,
                Some(task_id),
                TaskFailure::Internal(
                    "the task cannot resume because its tool is no longer registered",
                ),
            );
            self.record_task_failure(task_id, error).await;
            self.notify_task_state(task_id).await;
            return;
        };

        let mut ctx = RequestContext::new(RequestId::String(task_id.to_string()));
        // Replay rebuilds a request context, but it is still an invocation of
        // the same task. Restore its stable identity without a process-local
        // live handle so MRTR handlers can correlate every input round.
        ctx.extensions_mut()
            .insert(crate::tool::TaskContext::new(task_id.to_string()));
        // The answers reach the handler through the same MRTR extension a
        // client retry populates. Only a `stateless` build can register an
        // `mrtr_handler`, so a build without it can never park a task and
        // never reaches this.
        #[cfg(feature = "stateless")]
        ctx.extensions_mut().insert(crate::mrtr::MrtrRequest::new(
            Some(resume.input_responses),
            None,
        ));
        if let Some(tx) = &self.inner.notification_tx {
            ctx = ctx.with_notification_sender(tx.clone());
        }

        let task_id = task_id.to_string();
        let notifier = self.clone();
        tokio::spawn(async move {
            let outcome = notifier
                .invoke_tool(&tool, ctx, resume.arguments, &resume.tool_name)
                .await;
            let result = match outcome {
                Ok(crate::protocol::RequestOutcome::Complete(result)) => result,
                // A handler may ask again; each round parks and resumes the
                // same way, so multi-step interactions need no special casing.
                Ok(crate::protocol::RequestOutcome::InputRequired(input_required)) => {
                    notifier.park_task_for_input(&task_id, input_required).await;
                    return;
                }
                Err(error) => CallToolResult::error(error.to_string()),
            };

            notifier.complete_task_or_fail(&task_id, result).await;
            notifier.notify_task_state(&task_id).await;
        });
    }

    /// Push the current state of a task to subscribed listen streams.
    ///
    /// Best effort by design. A task outlives the request that created it, so
    /// there may be no subscriber at all, and SEP-2663 keeps `tasks/get`
    /// authoritative precisely so a dropped notification costs a client
    /// nothing beyond a slower poll. A failure to read the task back is
    /// therefore logged rather than propagated: the caller has already
    /// committed the state change this announces.
    pub(super) async fn notify_task_state(&self, task_id: &str) {
        if !self.final_tasks_enabled() {
            return;
        }

        let (detailed, meta) = match self.detailed_task(task_id).await {
            Ok(Some(detailed)) => detailed,
            Ok(None) => {
                tracing::debug!(
                    target: "mcp::tasks",
                    task_id = %task_id,
                    "skipping task notification: task state no longer exists"
                );
                return;
            }
            Err(error) => {
                tracing::debug!(
                    target: "mcp::tasks",
                    task_id = %task_id,
                    %error,
                    "skipping task notification: task state unavailable"
                );
                return;
            }
        };

        let notification = ServerNotification::FinalTaskStatusChanged(
            crate::tasks::TaskStatusNotificationParams {
                task: detailed,
                meta,
            },
        );

        // Delivery goes through the transport-lifetime sink rather than the
        // originating request's sender: the `tools/call` that created the task
        // has usually completed by the time a terminal transition happens, and
        // its stream is gone.
        #[cfg(all(feature = "http", feature = "stateless"))]
        if let Ok(active) = self.inner.modern_notification_sink.read()
            && let Some(sink) = active.as_ref()
        {
            sink(&notification);
            return;
        }

        if let Some(tx) = &self.inner.notification_tx {
            let _ = tx.try_send(notification);
        }
    }
}

// ===========================================================================
// Task helpers
// ===========================================================================
//
// Free functions whose only callers are the methods above. They sat at the top
// of `router.rs`, above the type they serve, rather than beside the group that
// uses them (#1256).

pub(super) async fn discard_unprepared_task(store: &Arc<dyn TaskStore>, task_id: &str) {
    if !matches!(store.discard_task(task_id).await, Ok(true)) {
        let _ = store
            .cancel_task(task_id, Some("task preparation failed"))
            .await;
    }
}

/// Whether this request's client declared the final Tasks extension.
///
/// Final requests carry client capabilities per request, so negotiation is
/// decided from the request itself rather than from session state.
#[cfg(feature = "stateless")]
pub(super) fn client_declares_tasks(extensions: &crate::context::Extensions) -> bool {
    final_client_capabilities(extensions).is_some_and(|capabilities| {
        capabilities.extensions.as_ref().is_some_and(|declared| {
            declared.contains_key(tower_mcp_types::protocol::TASKS_EXTENSION_ID)
        })
    })
}

#[cfg(not(feature = "stateless"))]
pub(super) fn client_declares_tasks(_extensions: &crate::context::Extensions) -> bool {
    false
}

/// Decode the wire `inputResponses` map into typed responses.
///
/// Unknown, already-answered, and superseded keys remain the store's
/// idempotency concern. A value that is not any valid protocol response shape
/// is malformed input, however, and rejects the complete request instead of
/// being silently reclassified as an unknown key.
pub(super) fn decode_input_responses(
    router: &McpRouter,
    task_id: &str,
    responses: &std::collections::HashMap<String, serde_json::Value>,
) -> Result<crate::protocol::InputResponses> {
    responses
        .iter()
        .map(|(key, value)| {
            serde_json::from_value(value.clone())
                .map(|response| (key.clone(), response))
                .map_err(|_| {
                    router.task_error(
                        TaskOperation::Update,
                        Some(task_id),
                        TaskFailure::InvalidArguments("inputResponses contains a malformed value"),
                    )
                })
        })
        .collect()
}

/// The client capability shape a server names in a `-32021` when it cannot
/// service a request without the Tasks extension.
pub(crate) fn tasks_client_capabilities() -> crate::protocol::ClientCapabilities {
    crate::protocol::ClientCapabilities {
        extensions: Some(
            [(
                tower_mcp_types::protocol::TASKS_EXTENSION_ID.to_string(),
                serde_json::json!({}),
            )]
            .into_iter()
            .collect(),
        ),
        ..Default::default()
    }
}

#[cfg(feature = "stateless")]
pub(super) fn validate_input_required_result(
    extensions: &crate::context::Extensions,
    result: &InputRequiredResult,
) -> Result<()> {
    result.validate().map_err(|message| {
        Error::invalid_params(format!("invalid InputRequiredResult: {message}"))
    })?;

    let meta = extensions
        .get::<crate::stateless::StatelessRequestMeta>()
        .filter(|meta| {
            meta.protocol_version.as_deref() == Some(crate::protocol::PROTOCOL_VERSION_2026_07_28)
        })
        .ok_or_else(|| {
            Error::invalid_params(
                "InputRequiredResult is only supported by the 2026-07-28 request lifecycle",
            )
        })?;
    let actual = meta.client_capabilities.as_ref().ok_or_else(|| {
        Error::invalid_params("clientCapabilities is required for InputRequiredResult")
    })?;

    if let Some(requests) = &result.input_requests {
        for request in requests.values() {
            let (supported, required) = match request {
                InputRequest::CreateMessage(params) => {
                    let requires_tools = params.tools.is_some();
                    let requires_context = params
                        .include_context
                        .is_some_and(|mode| mode != IncludeContext::None);
                    let required_sampling = SamplingCapability {
                        tools: requires_tools.then(SamplingToolsCapability::default),
                        context: requires_context.then(SamplingContextCapability::default),
                        ..SamplingCapability::default()
                    };
                    let supported = actual.sampling.as_ref().is_some_and(|sampling| {
                        (!requires_tools || sampling.tools.is_some())
                            && (!requires_context || sampling.context.is_some())
                    });
                    (
                        supported,
                        ClientCapabilities {
                            sampling: Some(required_sampling),
                            ..ClientCapabilities::default()
                        },
                    )
                }
                InputRequest::ListRoots(_) => (
                    actual.roots.is_some(),
                    ClientCapabilities {
                        roots: Some(RootsCapability::default()),
                        ..ClientCapabilities::default()
                    },
                ),
                InputRequest::Elicit(ElicitRequestParams::Form(_)) => {
                    let supported = actual.elicitation.as_ref().is_some_and(|elicitation| {
                        elicitation.form.is_some()
                            || (elicitation.form.is_none() && elicitation.url.is_none())
                    });
                    (
                        supported,
                        ClientCapabilities {
                            elicitation: Some(ElicitationCapability {
                                form: Some(ElicitationFormCapability::default()),
                                ..ElicitationCapability::default()
                            }),
                            ..ClientCapabilities::default()
                        },
                    )
                }
                InputRequest::Elicit(ElicitRequestParams::Url(_)) => (
                    actual
                        .elicitation
                        .as_ref()
                        .is_some_and(|elicitation| elicitation.url.is_some()),
                    ClientCapabilities {
                        elicitation: Some(ElicitationCapability {
                            url: Some(ElicitationUrlCapability::default()),
                            ..ElicitationCapability::default()
                        }),
                        ..ClientCapabilities::default()
                    },
                ),
                _ => {
                    return Err(Error::invalid_params(
                        "unsupported input request method in InputRequiredResult",
                    ));
                }
            };
            if !supported {
                return Err(Error::JsonRpc(
                    JsonRpcError::missing_required_client_capability(required),
                ));
            }
        }
    }
    Ok(())
}
