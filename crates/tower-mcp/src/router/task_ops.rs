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

impl McpRouter {
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
        task_id: &str,
        extensions: &crate::context::Extensions,
    ) -> Error {
        let Ok(presence) = self.inner.task_store.task_presence(task_id).await else {
            return Error::JsonRpc(unknown_task_error(task_id));
        };
        let owns = presence.owner().is_some_and(|owner| {
            crate::async_task::owner_matches(owner, request_principal(extensions).as_deref())
        });
        match presence {
            crate::async_task::TaskPresence::Expired { .. } if owns => {
                Error::JsonRpc(expired_task_error(task_id))
            }
            _ => Error::JsonRpc(unknown_task_error(task_id)),
        }
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
        task_id: &str,
        extensions: &crate::context::Extensions,
    ) -> Result<()> {
        // Resolved through presence so an expired task can be reported as
        // such to its owner. Everyone else must still see exactly what they
        // see for an id that was never issued, so the owner check happens
        // before any of that distinction escapes (#1249).
        let presence = self
            .inner
            .task_store
            .task_presence(task_id)
            .await
            .map_err(task_store_error)?;
        let Some(owner) = presence.owner() else {
            return Err(Error::JsonRpc(unknown_task_error(task_id)));
        };

        if crate::async_task::owner_matches(owner, request_principal(extensions).as_deref()) {
            match presence {
                // The owner is told the difference; nobody else reaches here.
                crate::async_task::TaskPresence::Expired { .. } => {
                    Err(Error::JsonRpc(expired_task_error(task_id)))
                }
                _ => Ok(()),
            }
        } else {
            tracing::debug!(
                target: "mcp::tasks",
                task_id = %task_id,
                "task operation refused: principal does not own the task"
            );
            Err(Error::JsonRpc(unknown_task_error(task_id)))
        }
    }

    /// Serve a final `tasks/get` as a status-discriminated `DetailedTask`.
    pub(super) async fn final_get_task(&self, task_id: &str) -> Result<McpResponse> {
        let (detailed, meta) = self.detailed_task(task_id).await?;
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
    ) -> Result<(
        crate::tasks::DetailedTask,
        Option<serde_json::Map<String, serde_json::Value>>,
    )> {
        let (task, result, error) = self
            .inner
            .task_store
            .get_task_result(task_id)
            .await
            .map_err(task_store_error)?
            .ok_or_else(|| Error::JsonRpc(unknown_task_error(task_id)))?;

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
                // Every request still awaiting a response, not just the most
                // recent one.
                let outstanding = self
                    .inner
                    .task_store
                    .outstanding_input_requests(task_id)
                    .await
                    .map_err(task_store_error)?
                    .unwrap_or_default();
                crate::tasks::DetailedTask::input_required(metadata, outstanding)
            }
            TaskStatus::Completed => {
                // The exact object the synchronous call would have returned,
                // including `isError: true` results.
                let mut object = result
                    .map(serde_json::to_value)
                    .transpose()
                    .map_err(|e| {
                        Error::JsonRpc(JsonRpcError::internal_error(format!(
                            "failed to encode task result: {e}"
                        )))
                    })?
                    .and_then(|value| value.as_object().cloned())
                    .unwrap_or_default();
                // This object is nested inside tasks/get, so it does not pass
                // through the JSON-RPC response stamper that adds the final
                // protocol's required complete discriminator.
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
            // `TaskStatus` is non_exhaustive. Report an unrecognized status as
            // working rather than inventing a terminal state.
            _ => crate::tasks::DetailedTask::working(metadata),
        };
        Ok((detailed, meta))
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
            let error = JsonRpcError::internal_error(
                "handler asked for input without naming any requests, so the task has \
                 nothing to wait for",
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
                let error = match &e {
                    crate::async_task::TaskStoreError::InvalidTransition(message) => {
                        tracing::error!(
                            task_id = %task_id,
                            error = %message,
                            "handler asked for input the protocol does not allow"
                        );
                        JsonRpcError::internal_error(format!(
                            "handler asked for input the protocol does not allow: {message}"
                        ))
                    }
                    other => {
                        tracing::warn!(
                            task_id = %task_id,
                            error = %other,
                            "task store could not park the task for input"
                        );
                        JsonRpcError::internal_error(format!(
                            "could not park the task for input: {other}"
                        ))
                    }
                };
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
        let Ok(live) = self.inner.live_tasks.lock() else {
            return false;
        };
        match live.get(task_id) {
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
    pub(super) fn signal_live_cancellation(&self, task_id: &str) -> bool {
        let Ok(live) = self.inner.live_tasks.lock() else {
            return false;
        };
        match live.get(task_id) {
            Some(handle) => {
                handle.cancelled.cancel();
                true
            }
            None => false,
        }
    }

    pub(super) fn register_live_task(&self, task_id: &str, handle: Arc<crate::tool::LiveTask>) {
        if let Ok(mut live) = self.inner.live_tasks.lock() {
            live.insert(task_id.to_string(), handle);
        }
    }

    pub(super) fn unregister_live_task(&self, task_id: &str) {
        if let Ok(mut live) = self.inner.live_tasks.lock() {
            live.remove(task_id);
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
                let error = JsonRpcError::internal_error(
                    "this task store cannot resume a task after input was provided; \
                     implement TaskStore::resume_context to support handlers that ask \
                     for input",
                );
                if let Err(e) = self.inner.task_store.fail_task(task_id, error).await {
                    tracing::warn!(task_id = %task_id, error = %e, "failed to record task failure");
                }
                self.notify_task_state(task_id).await;
                return;
            }
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "failed to read resume context");
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
            let error = JsonRpcError::internal_error(format!(
                "tool '{}' is no longer registered, so the task cannot resume",
                resume.tool_name
            ));
            if let Err(e) = self.inner.task_store.fail_task(task_id, error).await {
                tracing::warn!(task_id = %task_id, error = %e, "failed to record task failure");
            }
            self.notify_task_state(task_id).await;
            return;
        };

        let mut ctx = RequestContext::new(RequestId::String(task_id.to_string()));
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
        let task_store = self.inner.task_store.clone();
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

            if let Err(e) = task_store.complete_task(&task_id, result).await {
                tracing::warn!(task_id = %task_id, error = %e, "failed to record task completion");
            }
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
            Ok(detailed) => detailed,
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
