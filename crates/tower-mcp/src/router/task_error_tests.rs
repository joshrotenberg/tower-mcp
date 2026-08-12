//! Focused tests for Task error mapping and multi-read races.

use super::*;
use crate::tool::ToolBuilder;

fn tasks_client_extensions() -> Extensions {
    let mut extensions = Extensions::new();
    extensions.insert(crate::stateless::StatelessRequestMeta {
        protocol_version: Some(PROTOCOL_VERSION_2026_07_28.to_string()),
        client_capabilities: Some(ClientCapabilities {
            extensions: Some(
                [(TASKS_EXTENSION_ID.to_string(), serde_json::json!({}))]
                    .into_iter()
                    .collect(),
            ),
            ..Default::default()
        }),
        ..Default::default()
    });
    extensions
}

// A deterministic store for Task error-policy and multi-read race tests. Each
// method consumes the next scripted answer, so a changed call order fails the
// test instead of relying on timing.
#[cfg(feature = "stateless")]
#[derive(Default)]
struct ScriptedTaskStore {
    creates: std::sync::Mutex<
        std::collections::VecDeque<
            crate::async_task::Result<(String, crate::async_task::CancellationToken)>,
        >,
    >,
    presences: std::sync::Mutex<
        std::collections::VecDeque<crate::async_task::Result<crate::async_task::TaskPresence>>,
    >,
    snapshots: std::sync::Mutex<
        std::collections::VecDeque<
            crate::async_task::Result<Option<crate::async_task::TaskSnapshot>>,
        >,
    >,
    outstanding: std::sync::Mutex<
        std::collections::VecDeque<
            crate::async_task::Result<Option<crate::protocol::InputRequests>>,
        >,
    >,
    cancellations:
        std::sync::Mutex<std::collections::VecDeque<crate::async_task::Result<Option<TaskObject>>>>,
    resumes: std::sync::Mutex<
        std::collections::VecDeque<
            crate::async_task::Result<Option<crate::async_task::TaskResumeContext>>,
        >,
    >,
    completions: std::sync::Mutex<std::collections::VecDeque<crate::async_task::Result<bool>>>,
    failure_results: std::sync::Mutex<std::collections::VecDeque<crate::async_task::Result<bool>>>,
    recorded_failures: std::sync::Mutex<Vec<JsonRpcError>>,
}

#[cfg(feature = "stateless")]
impl ScriptedTaskStore {
    fn with_creates(
        self,
        steps: impl IntoIterator<
            Item = crate::async_task::Result<(String, crate::async_task::CancellationToken)>,
        >,
    ) -> Self {
        *self.creates.lock().unwrap() = steps.into_iter().collect();
        self
    }

    fn with_presences(
        self,
        steps: impl IntoIterator<Item = crate::async_task::Result<crate::async_task::TaskPresence>>,
    ) -> Self {
        *self.presences.lock().unwrap() = steps.into_iter().collect();
        self
    }

    fn with_snapshots(
        self,
        steps: impl IntoIterator<
            Item = crate::async_task::Result<Option<crate::async_task::TaskSnapshot>>,
        >,
    ) -> Self {
        *self.snapshots.lock().unwrap() = steps.into_iter().collect();
        self
    }

    fn with_outstanding(
        self,
        steps: impl IntoIterator<
            Item = crate::async_task::Result<Option<crate::protocol::InputRequests>>,
        >,
    ) -> Self {
        *self.outstanding.lock().unwrap() = steps.into_iter().collect();
        self
    }

    fn with_cancellations(
        self,
        steps: impl IntoIterator<Item = crate::async_task::Result<Option<TaskObject>>>,
    ) -> Self {
        *self.cancellations.lock().unwrap() = steps.into_iter().collect();
        self
    }

    fn with_resumes(
        self,
        steps: impl IntoIterator<
            Item = crate::async_task::Result<Option<crate::async_task::TaskResumeContext>>,
        >,
    ) -> Self {
        *self.resumes.lock().unwrap() = steps.into_iter().collect();
        self
    }

    fn with_completions(
        self,
        steps: impl IntoIterator<Item = crate::async_task::Result<bool>>,
    ) -> Self {
        *self.completions.lock().unwrap() = steps.into_iter().collect();
        self
    }

    fn with_failure_results(
        self,
        steps: impl IntoIterator<Item = crate::async_task::Result<bool>>,
    ) -> Self {
        *self.failure_results.lock().unwrap() = steps.into_iter().collect();
        self
    }

    fn pop<T>(queue: &std::sync::Mutex<std::collections::VecDeque<T>>, method: &str) -> T {
        queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected {method} call"))
    }
}

#[cfg(feature = "stateless")]
#[async_trait::async_trait]
impl crate::async_task::TaskStore for ScriptedTaskStore {
    async fn create_task(
        &self,
        _tool_name: &str,
        _arguments: serde_json::Value,
        _ttl: Option<u64>,
        _owner: crate::async_task::TaskOwner,
    ) -> crate::async_task::Result<(String, crate::async_task::CancellationToken)> {
        Self::pop(&self.creates, "create_task")
    }

    async fn task_owner(
        &self,
        _task_id: &str,
    ) -> crate::async_task::Result<Option<crate::async_task::TaskOwner>> {
        panic!("task_presence is overridden")
    }

    async fn task_presence(
        &self,
        _task_id: &str,
    ) -> crate::async_task::Result<crate::async_task::TaskPresence> {
        Self::pop(&self.presences, "task_presence")
    }

    async fn get_task(&self, _task_id: &str) -> crate::async_task::Result<Option<TaskObject>> {
        panic!("unexpected get_task call")
    }

    async fn get_task_result(
        &self,
        _task_id: &str,
    ) -> crate::async_task::Result<Option<crate::async_task::TaskSnapshot>> {
        Self::pop(&self.snapshots, "get_task_result")
    }

    async fn wait_for_completion(
        &self,
        _task_id: &str,
    ) -> crate::async_task::Result<Option<crate::async_task::TaskSnapshot>> {
        panic!("unexpected wait_for_completion call")
    }

    async fn list_tasks(
        &self,
        _status_filter: Option<TaskStatus>,
    ) -> crate::async_task::Result<Vec<TaskObject>> {
        panic!("unexpected list_tasks call")
    }

    async fn require_input(
        &self,
        _task_id: &str,
        _requests: crate::protocol::InputRequests,
        _message: Option<&str>,
    ) -> crate::async_task::Result<bool> {
        panic!("unexpected require_input call")
    }

    async fn outstanding_input_requests(
        &self,
        _task_id: &str,
    ) -> crate::async_task::Result<Option<crate::protocol::InputRequests>> {
        Self::pop(&self.outstanding, "outstanding_input_requests")
    }

    async fn apply_input_responses(
        &self,
        _task_id: &str,
        _responses: crate::protocol::InputResponses,
    ) -> crate::async_task::Result<Option<crate::async_task::AppliedInputResponses>> {
        panic!("unexpected apply_input_responses call")
    }

    async fn set_ttl(&self, _task_id: &str, _ttl_ms: u64) -> crate::async_task::Result<bool> {
        panic!("unexpected set_ttl call")
    }

    async fn complete_task(
        &self,
        _task_id: &str,
        _result: CallToolResult,
    ) -> crate::async_task::Result<bool> {
        Self::pop(&self.completions, "complete_task")
    }

    async fn fail_task(
        &self,
        _task_id: &str,
        error: JsonRpcError,
    ) -> crate::async_task::Result<bool> {
        self.recorded_failures.lock().unwrap().push(error);
        Self::pop(&self.failure_results, "fail_task")
    }

    async fn cancel_task(
        &self,
        _task_id: &str,
        _reason: Option<&str>,
    ) -> crate::async_task::Result<Option<TaskObject>> {
        Self::pop(&self.cancellations, "cancel_task")
    }

    async fn resume_context(
        &self,
        _task_id: &str,
    ) -> crate::async_task::Result<Option<crate::async_task::TaskResumeContext>> {
        Self::pop(&self.resumes, "resume_context")
    }
}

#[cfg(feature = "stateless")]
fn scripted_task(status: TaskStatus) -> TaskObject {
    TaskObject {
        task_id: "task_scripted".to_string(),
        status,
        status_message: None,
        created_at: "2026-08-12T00:00:00Z".to_string(),
        last_updated_at: "2026-08-12T00:00:01Z".to_string(),
        ttl: Some(60_000),
        poll_interval: Some(100),
        result: None,
        error: None,
        meta: None,
    }
}

#[cfg(feature = "stateless")]
fn final_get_request() -> McpRequest {
    McpRequest::GetTaskInfo(GetTaskInfoParams {
        task_id: "task_scripted".to_string(),
        meta: None,
    })
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn the_default_task_policy_redacts_backend_errors() {
    const SECRET: &str = "/private/db/tasks.sqlite: password=hunter2";
    let store = Arc::new(ScriptedTaskStore::default().with_presences([Err(
        crate::async_task::TaskStoreError::Backend(SECRET.to_string()),
    )]));
    let router = McpRouter::new().task_store(store).with_tasks();

    let Err(Error::JsonRpc(error)) = router
        .handle(
            RequestId::Number(1),
            final_get_request(),
            tasks_client_extensions(),
        )
        .await
    else {
        panic!("a backend failure must reject tasks/get")
    };
    assert_eq!(error.code, -32603);
    assert_eq!(error.message, "Task store operation failed");
    assert!(error.data.is_none());
    assert!(!format!("{error:?}").contains(SECRET));
}

#[tokio::test]
async fn merge_and_nest_keep_the_receiving_routers_task_policy() {
    let store = Arc::new(ScriptedTaskStore::default().with_presences([
        Ok(crate::async_task::TaskPresence::Missing),
        Ok(crate::async_task::TaskPresence::Missing),
    ]));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let recorded = calls.clone();
    let root = McpRouter::new()
        .task_store(store)
        .task_error_policy(TaskErrorPolicy::new(move |context| {
            recorded.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(context.operation(), TaskOperation::Get);
            assert!(matches!(context.failure(), TaskFailure::NotFound));
            JsonRpcError::invalid_params("root policy")
        }))
        .with_tasks();
    let child = || {
        McpRouter::new().task_error_policy(TaskErrorPolicy::new(|_| {
            JsonRpcError::invalid_params("child policy")
        }))
    };

    for router in [root.clone().merge(child()), root.nest("child", child())] {
        let Err(Error::JsonRpc(error)) = router
            .handle(
                RequestId::Number(1),
                final_get_request(),
                tasks_client_extensions(),
            )
            .await
        else {
            panic!("an unknown task must be mapped")
        };
        assert_eq!(error.message, "root policy");
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn cancel_does_not_mask_a_backend_failure_during_absence_classification() {
    use crate::async_task::{TaskPresence, TaskStoreError};

    const SECRET: &str = "redis://admin:secret@private-host";
    let store = Arc::new(
        ScriptedTaskStore::default()
            .with_presences([
                Ok(TaskPresence::Present { owner: None }),
                Err(TaskStoreError::Backend(SECRET.to_string())),
            ])
            .with_cancellations([Ok(None)]),
    );
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let recorded = calls.clone();
    let router = McpRouter::new()
        .task_store(store)
        .task_error_policy(TaskErrorPolicy::new(move |context| {
            recorded.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(context.operation(), TaskOperation::Cancel);
            assert_eq!(context.task_id(), Some("task_scripted"));
            assert!(matches!(
                context.failure(),
                TaskFailure::Store(TaskStoreError::Backend(message)) if message == SECRET
            ));
            JsonRpcError::internal_error("mapped task failure")
                .with_data(serde_json::json!({"kind": "task_store", "operation": "cancel"}))
        }))
        .with_tasks();

    let Err(Error::JsonRpc(error)) = router
        .handle(
            RequestId::Number(1),
            McpRequest::CancelTask(CancelTaskParams {
                task_id: "task_scripted".to_string(),
                reason: None,
                meta: None,
            }),
            tasks_client_extensions(),
        )
        .await
    else {
        panic!("the classification lookup failure must be returned")
    };
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(error.message, "mapped task failure");
    assert!(!format!("{error:?}").contains(SECRET));
    assert_eq!(error.data.unwrap()["operation"], "cancel");
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn final_get_reclassifies_a_disappeared_snapshot_as_expired() {
    use crate::async_task::TaskPresence;

    let store = Arc::new(
        ScriptedTaskStore::default()
            .with_presences([
                Ok(TaskPresence::Present { owner: None }),
                Ok(TaskPresence::Expired { owner: None }),
            ])
            .with_snapshots([Ok(None)]),
    );
    let router = McpRouter::new().task_store(store).with_tasks();

    let Err(Error::JsonRpc(error)) = router
        .handle(
            RequestId::Number(1),
            final_get_request(),
            tasks_client_extensions(),
        )
        .await
    else {
        panic!("the owner must see that the task expired between reads")
    };
    assert_eq!(error.message, "Task expired: task_scripted");
    assert_eq!(
        error.data,
        Some(serde_json::json!({"reason": "task_expired"}))
    );
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn final_get_reclassifies_disappeared_outstanding_inputs_as_expired() {
    use crate::async_task::TaskPresence;

    let store = Arc::new(
        ScriptedTaskStore::default()
            .with_presences([
                Ok(TaskPresence::Present { owner: None }),
                Ok(TaskPresence::Expired { owner: None }),
            ])
            .with_snapshots([Ok(Some((
                scripted_task(TaskStatus::InputRequired),
                None,
                None,
            )))])
            .with_outstanding([Ok(None)]),
    );
    let router = McpRouter::new().task_store(store).with_tasks();

    let Err(Error::JsonRpc(error)) = router
        .handle(
            RequestId::Number(1),
            final_get_request(),
            tasks_client_extensions(),
        )
        .await
    else {
        panic!("the owner must see expiry rather than an empty input-required task")
    };
    assert_eq!(error.message, "Task expired: task_scripted");
    assert_eq!(
        error.data,
        Some(serde_json::json!({"reason": "task_expired"}))
    );
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn final_get_rereads_after_an_empty_outstanding_input_race() {
    use crate::async_task::TaskPresence;

    let store = Arc::new(
        ScriptedTaskStore::default()
            .with_presences([Ok(TaskPresence::Present { owner: None })])
            .with_snapshots([
                Ok(Some((scripted_task(TaskStatus::InputRequired), None, None))),
                Ok(Some((scripted_task(TaskStatus::Working), None, None))),
            ])
            .with_outstanding([Ok(Some(Default::default()))]),
    );
    let router = McpRouter::new().task_store(store).with_tasks();

    let response = router
        .handle(
            RequestId::Number(1),
            final_get_request(),
            tasks_client_extensions(),
        )
        .await
        .expect("the stabilized snapshot is valid");
    let McpResponse::FinalGetTask(result) = response else {
        panic!("expected a final tasks/get result")
    };
    assert_eq!(result.task.status(), TaskStatus::Working);
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn malformed_input_responses_flow_through_the_custom_task_policy() {
    use crate::async_task::TaskStore;

    let store = Arc::new(crate::async_task::MemoryTaskStore::new());
    let (task_id, _) = store
        .create_task("tool", serde_json::json!({}), None, None)
        .await
        .unwrap();
    let router = McpRouter::new()
        .task_store(store)
        .task_error_policy(TaskErrorPolicy::new(|context| {
            assert_eq!(context.operation(), TaskOperation::Update);
            assert!(matches!(
                context.failure(),
                TaskFailure::InvalidArguments(message)
                    if *message == "inputResponses contains a malformed value"
            ));
            JsonRpcError::invalid_params("mapped invalid arguments")
                .with_data(serde_json::json!({"kind": "invalid_arguments"}))
        }))
        .with_tasks();

    let Err(Error::JsonRpc(error)) = router
        .handle(
            RequestId::Number(1),
            McpRequest::UpdateTask(UpdateTaskParams {
                task_id,
                input_responses: [("approval".to_string(), serde_json::json!({"bogus": true}))]
                    .into_iter()
                    .collect(),
                meta: None,
            }),
            tasks_client_extensions(),
        )
        .await
    else {
        panic!("a malformed response must be rejected")
    };
    assert_eq!(error.code, -32602);
    assert_eq!(error.message, "mapped invalid arguments");
    assert_eq!(error.data.unwrap()["kind"], "invalid_arguments");
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn a_panicking_task_policy_uses_a_fixed_redacted_fallback() {
    let store = Arc::new(
        ScriptedTaskStore::default().with_presences([Ok(crate::async_task::TaskPresence::Missing)]),
    );
    let router = McpRouter::new()
        .task_store(store)
        .task_error_policy(TaskErrorPolicy::new(|_| {
            panic!("secret policy panic payload")
        }))
        .with_tasks();

    let Err(Error::JsonRpc(error)) = router
        .handle(
            RequestId::Number(1),
            final_get_request(),
            tasks_client_extensions(),
        )
        .await
    else {
        panic!("the policy panic must become a JSON-RPC error")
    };
    assert_eq!(error.code, -32603);
    assert_eq!(error.message, "Task error policy failed");
    assert!(error.data.is_none());
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn create_store_failures_include_the_create_operation() {
    let store = Arc::new(ScriptedTaskStore::default().with_creates([Err(
        crate::async_task::TaskStoreError::Backend("private create detail".to_string()),
    )]));
    let router = McpRouter::new()
        .task_store(store)
        .task_error_policy(TaskErrorPolicy::new(|context| {
            assert_eq!(context.operation(), TaskOperation::Create);
            assert_eq!(context.task_id(), None);
            assert!(matches!(context.failure(), TaskFailure::Store(_)));
            JsonRpcError::internal_error("mapped create failure")
        }))
        .tool(
            ToolBuilder::new("task_tool")
                .task_support(TaskSupportMode::Optional)
                .handler(|_: serde_json::Value| async { Ok(CallToolResult::text("unused")) })
                .build(),
        )
        .with_tasks();

    let Err(Error::JsonRpc(error)) = router
        .handle(
            RequestId::Number(1),
            McpRequest::CallTool(CallToolParams {
                name: "task_tool".to_string(),
                arguments: serde_json::json!({}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
            tasks_client_extensions(),
        )
        .await
    else {
        panic!("create_task must surface its mapped store failure")
    };
    assert_eq!(error.message, "mapped create failure");
}

#[tokio::test]
async fn resume_store_failures_are_mapped_and_persisted_without_backend_details() {
    use crate::async_task::TaskStoreError;

    const SECRET: &str = "postgres://private-user:secret@database/tasks";
    let store = Arc::new(
        ScriptedTaskStore::default()
            .with_resumes([Err(TaskStoreError::Backend(SECRET.to_string()))])
            .with_failure_results([Ok(true)]),
    );
    let router = McpRouter::new()
        .task_store(store.clone())
        .task_error_policy(TaskErrorPolicy::new(|context| {
            assert_eq!(context.operation(), TaskOperation::Resume);
            assert_eq!(context.task_id(), Some("task_scripted"));
            assert!(matches!(
                context.failure(),
                TaskFailure::Store(TaskStoreError::Backend(message)) if message == SECRET
            ));
            JsonRpcError::internal_error("mapped resume failure")
                .with_data(serde_json::json!({"phase": "resume"}))
        }));

    router.resume_task("task_scripted").await;

    let failures = store.recorded_failures.lock().unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].message, "mapped resume failure");
    assert_eq!(failures[0].data.as_ref().unwrap()["phase"], "resume");
    assert!(!format!("{:?}", failures[0]).contains(SECRET));
}

#[tokio::test]
async fn absent_resume_state_and_missing_tools_use_typed_safe_failures() {
    const SECRET_TOOL: &str = "private.provider.secret-tool";
    let store = Arc::new(
        ScriptedTaskStore::default()
            .with_resumes([
                Ok(None),
                Ok(Some(crate::async_task::TaskResumeContext {
                    tool_name: SECRET_TOOL.to_string(),
                    arguments: serde_json::json!({}),
                    input_responses: Default::default(),
                })),
            ])
            .with_failure_results([Ok(true), Ok(true)]),
    );
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let recorded = calls.clone();
    let router = McpRouter::new()
        .task_store(store.clone())
        .task_error_policy(TaskErrorPolicy::new(move |context| {
            recorded.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(context.operation(), TaskOperation::Resume);
            assert!(matches!(context.failure(), TaskFailure::Internal(_)));
            JsonRpcError::internal_error("mapped safe resume failure")
        }));

    router.resume_task("task_scripted").await;
    router.resume_task("task_scripted").await;

    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    let failures = store.recorded_failures.lock().unwrap();
    assert_eq!(failures.len(), 2);
    assert!(
        failures
            .iter()
            .all(|error| error.message == "mapped safe resume failure")
    );
    assert!(!format!("{failures:?}").contains(SECRET_TOOL));
}

#[tokio::test]
async fn completion_write_failures_become_one_policy_mapped_task_failure() {
    use crate::async_task::TaskStoreError;

    const SECRET: &str = "sqlite write failed at /private/tasks.sqlite";
    let store = Arc::new(
        ScriptedTaskStore::default()
            .with_completions([Err(TaskStoreError::Backend(SECRET.to_string())), Ok(false)])
            .with_failure_results([Ok(true)]),
    );
    let router = McpRouter::new()
        .task_store(store.clone())
        .task_error_policy(TaskErrorPolicy::new(|context| {
            assert_eq!(context.operation(), TaskOperation::Finalize);
            assert!(matches!(
                context.failure(),
                TaskFailure::Store(TaskStoreError::Backend(message)) if message == SECRET
            ));
            JsonRpcError::internal_error("mapped finalization failure")
                .with_data(serde_json::json!({"phase": "finalize"}))
        }));

    assert!(
        !router
            .complete_task_or_fail("task_scripted", CallToolResult::text("done"))
            .await
    );
    let failures_after_error = store.recorded_failures.lock().unwrap().len();
    assert_eq!(failures_after_error, 1);

    // `Ok(false)` means a terminal outcome won the race. It must not be
    // rewritten with a synthetic failure or reported as an applied success.
    assert!(
        !router
            .complete_task_or_fail("task_scripted", CallToolResult::text("late"))
            .await
    );
    let failures = store.recorded_failures.lock().unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].message, "mapped finalization failure");
    assert_eq!(failures[0].data.as_ref().unwrap()["phase"], "finalize");
    assert!(!format!("{:?}", failures[0]).contains(SECRET));
}
