#![cfg(feature = "stateless")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, json};
use tokio::sync::mpsc;
use tower_mcp::client::{ChannelTransport, McpClient, NotificationHandler};
use tower_mcp::extract::{Extension, Json};
use tower_mcp::protocol::TaskStatus;
use tower_mcp::schemars::JsonSchema;
use tower_mcp::{
    CallToolResult, McpRouter, ProtocolSupport, TaskContext, TaskPreparation, TaskStore,
    TaskSupportMode, ToolBuilder,
};

const PREPARED_META: &str = "dev.tower-mcp/prepared";

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    value: u64,
}

#[derive(Debug, Clone)]
struct PreparedState {
    task_id: String,
    doubled: u64,
}

#[tokio::test]
async fn preparation_metadata_and_state_follow_the_entire_task_lifecycle() {
    let preparation_count = Arc::new(AtomicUsize::new(0));
    let handler_count = Arc::new(AtomicUsize::new(0));
    let prepare_calls = preparation_count.clone();
    let handler_calls = handler_count.clone();

    let tool = ToolBuilder::new("prepared")
        .task_support(TaskSupportMode::Required)
        .extractor_handler(
            (),
            move |Extension(prepared): Extension<PreparedState>,
                  Extension(task): Extension<TaskContext>,
                  Json(input): Json<Input>| {
                let handler_calls = handler_calls.clone();
                async move {
                    handler_calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(task.task_id(), prepared.task_id);
                    assert_eq!(prepared.doubled, input.value * 2);
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    Ok(CallToolResult::json(json!({
                        "task_id": prepared.task_id,
                        "doubled": prepared.doubled
                    })))
                }
            },
        )
        .build()
        .with_task_preparation(move |task, arguments| {
            let prepare_calls = prepare_calls.clone();
            async move {
                prepare_calls.fetch_add(1, Ordering::SeqCst);
                let input: Input = serde_json::from_value(arguments)
                    .map_err(|error| tower_mcp::Error::invalid_params(error.to_string()))?;
                let state = PreparedState {
                    task_id: task.task_id().to_string(),
                    doubled: input.value * 2,
                };
                let mut meta = Map::new();
                meta.insert(
                    PREPARED_META.to_string(),
                    json!({"taskId": task.task_id(), "doubled": state.doubled}),
                );
                Ok(TaskPreparation::new().with_meta(meta).with_extension(state))
            }
        });
    let router = McpRouter::new()
        .server_info("task-preparation-test", "1.0.0")
        .tool(tool)
        .with_tasks();

    let (notification_tx, mut notification_rx) = mpsc::unbounded_channel();
    let handler = NotificationHandler::new().on_final_task_status_changed(move |notification| {
        let _ = notification_tx.send(notification);
    });
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .with_tasks()
        .connect(ChannelTransport::new(router), handler)
        .await
        .expect("connect");
    client
        .discover("task-preparation-client", "1.0.0")
        .await
        .expect("discover");

    let created = client
        .call_tool_as_task("prepared", json!({"value": 21}), None)
        .await
        .expect("task creation");
    let initial_meta = created.meta.as_ref().expect("initial task metadata");
    assert_eq!(initial_meta[PREPARED_META]["doubled"], 42);
    assert_eq!(initial_meta[PREPARED_META]["taskId"], created.task.task_id);

    let polled = client
        .task_get(&created.task.task_id)
        .await
        .expect("tasks/get");
    assert_eq!(
        polled.meta.as_ref().unwrap()[PREPARED_META],
        initial_meta[PREPARED_META]
    );

    let done = tokio::time::timeout(
        Duration::from_secs(5),
        client.task_wait(&created.task.task_id),
    )
    .await
    .expect("task wait timeout")
    .expect("task wait");
    assert_eq!(done.status, TaskStatus::Completed);
    assert_eq!(
        done.meta.as_ref().unwrap()[PREPARED_META],
        initial_meta[PREPARED_META]
    );
    assert_eq!(
        done.result
            .as_ref()
            .unwrap()
            .structured_content
            .as_ref()
            .unwrap()["doubled"],
        42
    );

    let notification = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let notification = notification_rx.recv().await.expect("notification stream");
            if notification.task.task_id() == created.task.task_id {
                return notification;
            }
        }
    })
    .await
    .expect("task notification timeout");
    assert_eq!(
        notification.meta.as_ref().unwrap()[PREPARED_META],
        initial_meta[PREPARED_META]
    );
    assert_eq!(preparation_count.load(Ordering::SeqCst), 1);
    assert_eq!(handler_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn preparation_error_discards_the_task_and_skips_background_execution() {
    let preparation_count = Arc::new(AtomicUsize::new(0));
    let handler_count = Arc::new(AtomicUsize::new(0));
    let prepare_calls = preparation_count.clone();
    let handler_calls = handler_count.clone();
    let store = Arc::new(tower_mcp::MemoryTaskStore::new());

    let tool = ToolBuilder::new("rejected")
        .task_support(TaskSupportMode::Required)
        .handler(move |_input: Input| {
            let handler_calls = handler_calls.clone();
            async move {
                handler_calls.fetch_add(1, Ordering::SeqCst);
                Ok(CallToolResult::text("should not run"))
            }
        })
        .task_preparation(move |_task, _input| {
            let prepare_calls = prepare_calls.clone();
            async move {
                prepare_calls.fetch_add(1, Ordering::SeqCst);
                Err::<TaskPreparation, _>(tower_mcp::Error::Internal(
                    "preparation rejected".to_string(),
                ))
            }
        })
        .build();
    let router = McpRouter::new()
        .server_info("task-preparation-test", "1.0.0")
        .tool(tool)
        .task_store(store.clone())
        .with_tasks();
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .with_tasks()
        .connect_simple(ChannelTransport::new(router))
        .await
        .expect("connect");
    client
        .discover("task-preparation-client", "1.0.0")
        .await
        .expect("discover");

    let error = client
        .call_tool_as_task("rejected", json!({"value": 1}), None)
        .await
        .expect_err("preparation must reject the call");
    assert!(error.to_string().contains("preparation rejected"));
    assert_eq!(preparation_count.load(Ordering::SeqCst), 1);
    assert_eq!(handler_count.load(Ordering::SeqCst), 0);
    assert!(store.list_tasks(None).await.unwrap().is_empty());
}
