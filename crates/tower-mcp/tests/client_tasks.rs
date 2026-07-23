//! Client tasks API acceptance tests (#957): task-augmented tool calls,
//! polling, result retrieval, and cancellation through McpClient.

use std::time::Duration;

use tower_mcp::client::{ChannelTransport, McpClient};
use tower_mcp::extract::RawArgs;
use tower_mcp::protocol::TaskStatus;
use tower_mcp::{CallToolResult, McpRouter, TaskSupportMode, ToolBuilder};

fn task_router() -> McpRouter {
    McpRouter::new()
        .server_info("tasks-test", "1.0.0")
        .tool(
            ToolBuilder::new("compute")
                .description("Finishes quickly with a result")
                .task_support(TaskSupportMode::Optional)
                .extractor_handler((), |RawArgs(_): RawArgs| async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Ok(CallToolResult::text("the answer is 42"))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("forever")
                .description("Runs until cancelled")
                .task_support(TaskSupportMode::Optional)
                .extractor_handler((), |RawArgs(_): RawArgs| async move {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Ok(CallToolResult::text("never happens"))
                })
                .build(),
        )
}

/// Acceptance: a client starts a task-augmented call against a
/// TaskStore-backed router, polls tasks/get, and retrieves the result.
#[tokio::test]
async fn task_augmented_call_polls_and_retrieves_result() {
    let client = McpClient::connect(ChannelTransport::new(task_router()))
        .await
        .expect("connect");
    client
        .initialize("test", "1.0.0")
        .await
        .expect("initialize");

    let created = client
        .call_tool_as_task("compute", serde_json::json!({}), Some(60_000))
        .await
        .expect("task-augmented call");
    assert!(created.task.task_id.starts_with("task-"));
    assert_eq!(created.task.status, TaskStatus::Working);
    assert!(
        created.task.result.is_none(),
        "no result payload while working"
    );

    // Direct poll works while running or after completion.
    let polled = client
        .task_get(&created.task.task_id)
        .await
        .expect("tasks/get");
    assert_eq!(polled.task_id, created.task.task_id);

    // Wait for the terminal state; the completed task carries the result
    // the synchronous call would have returned (SEP-2663 DetailedTask).
    let done = tokio::time::timeout(
        Duration::from_secs(5),
        client.task_wait(&created.task.task_id),
    )
    .await
    .expect("task_wait timed out")
    .expect("task_wait");
    assert_eq!(done.status, TaskStatus::Completed);
    let result = done.result.expect("completed task must carry its result");
    match &result.content[0] {
        tower_mcp::protocol::Content::Text { text, .. } => {
            assert_eq!(text, "the answer is 42")
        }
        other => panic!("expected text content, got {other:?}"),
    }
    assert!(done.error.is_none());
}

/// Acceptance: the cancel path resolves the task and the client observes the
/// terminal state.
#[tokio::test]
async fn task_cancel_resolves_to_terminal_state() {
    let client = McpClient::connect(ChannelTransport::new(task_router()))
        .await
        .expect("connect");
    client
        .initialize("test", "1.0.0")
        .await
        .expect("initialize");

    let created = client
        .call_tool_as_task("forever", serde_json::json!({}), None)
        .await
        .expect("task-augmented call");

    // Empty ack (final SEP-2663 shape).
    client
        .task_cancel(&created.task.task_id, Some("test cancellation".into()))
        .await
        .expect("tasks/cancel");

    let done = tokio::time::timeout(
        Duration::from_secs(5),
        client.task_wait(&created.task.task_id),
    )
    .await
    .expect("task_wait timed out")
    .expect("task_wait");
    assert_eq!(done.status, TaskStatus::Cancelled);
    assert!(done.result.is_none());
}

/// Unknown task ids surface as an error, not a hang.
#[tokio::test]
async fn task_get_unknown_id_errors() {
    let client = McpClient::connect(ChannelTransport::new(task_router()))
        .await
        .expect("connect");
    client
        .initialize("test", "1.0.0")
        .await
        .expect("initialize");

    let err = client
        .task_get("task-does-not-exist")
        .await
        .expect_err("unknown task id must error");
    assert!(err.to_string().contains("not found"), "got: {err}");
}
