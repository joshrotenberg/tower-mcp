//! Protocol-revision-aware structured content checks through the router (#1540).

#[cfg(any(feature = "testing", feature = "stateless"))]
use serde_json::{Value, json};
#[cfg(any(feature = "testing", feature = "stateless"))]
use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

#[cfg(any(feature = "testing", feature = "stateless"))]
fn shape_router(value: Value) -> McpRouter {
    McpRouter::new().tool(
        ToolBuilder::new("snapshots")
            .handler(move |_args: Value| {
                let value = value.clone();
                async move { Ok(CallToolResult::json(value)) }
            })
            .build(),
    )
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn a_tools_call_returning_an_array_is_an_error_result_on_a_2025_session() {
    let mut client = tower_mcp::TestClient::from_router(shape_router(json!([1, 2])));
    client.initialize().await;
    let result = client.call_tool("snapshots", json!({})).await;
    assert!(result.is_error);
    assert!(result.structured_content.is_none());
    let text = result.all_text();
    assert!(text.contains("snapshots"));
    assert!(text.contains("object"));
    assert!(text.contains("from_list"));
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn a_tools_call_returning_an_object_is_returned_unchanged() {
    let value = json!({"items": [1, 2]});
    let mut client = tower_mcp::TestClient::from_router(shape_router(value.clone()));
    client.initialize().await;
    assert_serialized_eq(
        client.call_tool("snapshots", json!({})).await,
        CallToolResult::json(value),
    );
}

#[cfg(feature = "stateless")]
mod final_protocol {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tower_mcp::async_task::{MemoryTaskStore, TaskStore};
    use tower_mcp::client::{ChannelTransport, McpClient, TaskAwareCallToolOutcome};
    use tower_mcp::protocol::{
        ElicitAction, ElicitFormParams, ElicitFormSchema, ElicitRequestParams, ElicitResult,
        InputRequest, InputRequiredResult, InputResponse, RequestOutcome, TaskStatus,
    };
    use tower_mcp::{ProtocolSupport, RequestContext};

    async fn connect(router: McpRouter, tasks: bool) -> McpClient {
        let builder = McpClient::builder()
            .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap());
        let builder = if tasks { builder.with_tasks() } else { builder };
        let client = builder
            .connect_simple(ChannelTransport::new(router))
            .await
            .unwrap();
        client.discover("shape-tests", "1.0.0").await.unwrap();
        client
    }

    async fn await_status(store: &MemoryTaskStore, id: &str, expected: TaskStatus) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let task = store.get_task(id).await.unwrap().unwrap();
                assert_ne!(task.status, TaskStatus::Failed, "task failed: {task:?}");
                if task.status == expected {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("task did not reach expected status");
    }

    #[tokio::test]
    async fn a_2026_07_28_tools_call_keeps_an_array_structured_content() {
        let client = connect(shape_router(json!([1, 2])), false).await;
        let result = client.call_tool("snapshots", json!({})).await.unwrap();
        assert_serialized_eq(result, CallToolResult::json(json!([1, 2])));
    }

    #[tokio::test]
    async fn a_resumed_mrtr_task_that_completes_with_an_array_is_not_rejected() {
        // Recorded rather than asserted inside the handler: resume_task runs the
        // handler in a spawned task, where a panic surfaces as a status timeout.
        let replay_meta_absent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = replay_meta_absent.clone();
        let tool = ToolBuilder::new("snapshots")
            .task_support(tower_mcp::protocol::TaskSupportMode::Optional)
            .mrtr_handler(move |ctx: RequestContext, _args: Value| {
                let observed = observed.clone();
                async move {
                    if ctx
                        .input_responses()
                        .is_some_and(|responses| responses.contains_key("approval"))
                    {
                        observed.store(
                            ctx.per_request_meta().is_none(),
                            std::sync::atomic::Ordering::SeqCst,
                        );
                        return Ok(RequestOutcome::Complete(CallToolResult::json(json!([
                            1, 2
                        ]))));
                    }
                    let requests = [(
                        "approval".to_string(),
                        InputRequest::Elicit(ElicitRequestParams::Form(ElicitFormParams {
                            mode: None,
                            message: "Return snapshots?".to_string(),
                            requested_schema: ElicitFormSchema::new(),
                            meta: None,
                        })),
                    )]
                    .into_iter()
                    .collect();
                    Ok(RequestOutcome::InputRequired(
                        InputRequiredResult::with_requests(requests),
                    ))
                }
            })
            .build();
        let store = Arc::new(MemoryTaskStore::new());
        let router = McpRouter::new()
            .task_store(store.clone())
            .tool(tool)
            .with_tasks();
        let client = connect(router, true).await;
        let outcome = client
            .call_tool_once_task_aware("snapshots", json!({}), None, None)
            .await
            .unwrap();
        let TaskAwareCallToolOutcome::Task(created) = outcome else {
            panic!("expected task")
        };
        let id = &created.task.metadata.task_id;
        await_status(&store, id, TaskStatus::InputRequired).await;
        client
            .task_update(
                id,
                [(
                    "approval".to_string(),
                    InputResponse::Elicit(ElicitResult {
                        action: ElicitAction::Accept,
                        content: None,
                        meta: None,
                    }),
                )]
                .into_iter()
                .collect(),
            )
            .await
            .unwrap();
        await_status(&store, id, TaskStatus::Completed).await;
        assert!(
            replay_meta_absent.load(std::sync::atomic::Ordering::SeqCst),
            "replay has no transport metadata"
        );
        let (_, result, error) = store.get_task_result(id).await.unwrap().unwrap();
        assert!(error.is_none());
        assert_serialized_eq(result.unwrap(), CallToolResult::json(json!([1, 2])));
    }
}

#[cfg(any(feature = "testing", feature = "stateless"))]
fn assert_serialized_eq(actual: impl serde::Serialize, expected: impl serde::Serialize) {
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}
