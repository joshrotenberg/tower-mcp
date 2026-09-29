//! Tool schema validation through the router (#1512, #1513).
//!
//! The unit tests next to `tool::validation` cover each handler kind. These
//! check that a `tools/call` and a task-backed call reach the same check, and
//! that a rejected call never runs the handler.

#![cfg(feature = "schema-validation")]
// The cases below need `testing` (a client) or `stateless` (tasks). With
// neither, the shared fixtures have no user.
#![cfg_attr(
    not(any(feature = "testing", feature = "stateless")),
    allow(dead_code, unused_imports)
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tower_mcp::protocol::TaskSupportMode;
use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

#[derive(Debug, Deserialize, JsonSchema)]
struct Ticket {
    #[schemars(range(min = 1, max = 5))]
    priority: u32,
}

fn counting_router() -> (McpRouter, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let tool = ToolBuilder::new("file_ticket")
        .description("File a ticket")
        .task_support(TaskSupportMode::Optional)
        .output_schema(json!({
            "type": "object",
            "properties": { "id": { "type": "integer" } },
            "required": ["id"]
        }))
        .handler(move |input: Ticket| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                // Priority 5 is filed with a malformed id.
                let id = if input.priority == 5 {
                    json!("T-5")
                } else {
                    json!(7)
                };
                Ok(CallToolResult::json(json!({ "id": id })))
            }
        })
        .build();
    let router = McpRouter::new().server_info("tickets", "1.0.0").tool(tool);
    (router, calls)
}

#[cfg(feature = "testing")]
mod tools_call {
    use super::*;
    use tower_mcp::TestClient;

    #[tokio::test]
    async fn a_rejected_call_is_an_error_result_and_never_runs_the_handler() {
        let (router, calls) = counting_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let result = client
            .call_tool("file_ticket", json!({ "priority": 9 }))
            .await;
        assert!(result.is_error);
        let text = result.all_text();
        assert!(text.contains("/priority"), "got: {text}");
        assert!(text.contains("maximum"), "got: {text}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_valid_call_reaches_the_handler_and_its_result_is_returned() {
        let (router, calls) = counting_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let result = client
            .call_tool("file_ticket", json!({ "priority": 2 }))
            .await;
        assert!(!result.is_error);
        assert_eq!(result.structured_content, Some(json!({ "id": 7 })));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_result_that_breaks_the_output_schema_is_an_error_result() {
        let (router, calls) = counting_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let result = client
            .call_tool("file_ticket", json!({ "priority": 5 }))
            .await;
        assert!(result.is_error);
        assert!(result.structured_content.is_none());
        let text = result.all_text();
        assert!(text.contains("/id"), "got: {text}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

#[cfg(feature = "stateless")]
mod tasks {
    use super::*;
    use tower_mcp::async_task::{MemoryTaskStore, TaskStore};
    use tower_mcp::client::{ChannelTransport, McpClient};
    use tower_mcp::protocol::TaskStatus;
    use tower_mcp::{TaskContext, TaskOutcome};

    async fn await_completed(store: &MemoryTaskStore, id: &str) {
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if store.get_task(id).await.unwrap().unwrap().status == TaskStatus::Completed {
                return;
            }
        }
        panic!("task {id} did not complete");
    }

    async fn connect(router: McpRouter) -> McpClient {
        let client = McpClient::connect(ChannelTransport::new(router))
            .await
            .expect("connect");
        client.initialize("t", "1.0.0").await.expect("init");
        client
    }

    #[tokio::test]
    async fn a_task_backed_replay_call_is_validated() {
        let (router, calls) = counting_router();
        let store = Arc::new(MemoryTaskStore::new());
        let router = router.task_store(store.clone()).with_tasks();
        let client = connect(router).await;

        let task_id = client
            .call_tool_as_task("file_ticket", json!({ "priority": 9 }), None)
            .await
            .expect("task created")
            .task
            .task_id;
        await_completed(&store, &task_id).await;

        let (_, result, _) = store.get_task_result(&task_id).await.unwrap().unwrap();
        let result = result.expect("a completed task has a result");
        assert!(result.is_error);
        assert!(result.all_text().contains("/priority"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_live_task_call_is_validated() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let tool = ToolBuilder::new("live_ticket")
            .live_task_handler(move |_task: TaskContext, _input: Ticket| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(TaskOutcome::Completed(CallToolResult::text("filed")))
                }
            })
            .build();
        let store = Arc::new(MemoryTaskStore::new());
        let router = McpRouter::new()
            .server_info("tickets", "1.0.0")
            .task_store(store.clone())
            .tool(tool)
            .with_tasks();
        let client = connect(router).await;

        let task_id = client
            .call_tool_as_task("live_ticket", json!({ "priority": 0 }), None)
            .await
            .expect("task created")
            .task
            .task_id;
        await_completed(&store, &task_id).await;

        let (_, result, _) = store.get_task_result(&task_id).await.unwrap().unwrap();
        let result = result.expect("a completed task has a result");
        assert!(result.is_error);
        assert!(result.all_text().contains("/priority"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
