//! An external `TaskStore`, written the way a downstream crate would.
//!
//! #1293: `TaskStore::create_task` must return an `async_task::CancellationToken`,
//! and that type had a private field with no constructor, so no crate outside
//! `tower-mcp` could implement the trait at all. A struct literal fails with
//! `E0451` and there was nothing else to hand back, which made the whole trait
//! closed despite existing for exactly this purpose.
//!
//! This file deliberately uses only the public API. If any part of the trait
//! becomes unimplementable from outside again, this stops compiling, which is
//! the real assertion. Asserting a constructor exists would not catch the trait
//! being unimplementable for some other reason.

#![cfg(feature = "stateless")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use tower_mcp::async_task::{
    CancellationToken, Result as TaskResult, TaskOwner, TaskSnapshot, TaskStore, TaskStoreError,
};
use tower_mcp::client::{ChannelTransport, McpClient};
use tower_mcp::protocol::{CallToolResult, InputRequests, InputResponses, TaskObject, TaskStatus};
use tower_mcp::{JsonRpcError, McpRouter, ToolBuilder};

/// Stands in for a SQLite or Postgres store: its own state, its own ids, no
/// delegation to `MemoryTaskStore`.
#[derive(Default)]
struct ExternalStore {
    tasks: Mutex<HashMap<String, Record>>,
}

struct Record {
    object: TaskObject,
    owner: TaskOwner,
    result: Option<CallToolResult>,
    error: Option<JsonRpcError>,
    token: CancellationToken,
}

impl ExternalStore {
    fn with<T>(&self, id: &str, f: impl FnOnce(&mut Record) -> T) -> Option<T> {
        self.tasks.lock().ok()?.get_mut(id).map(f)
    }
}

#[async_trait]
impl TaskStore for ExternalStore {
    async fn create_task(
        &self,
        _tool_name: &str,
        _arguments: serde_json::Value,
        ttl: Option<u64>,
        owner: TaskOwner,
    ) -> TaskResult<(String, CancellationToken)> {
        // The line this whole test exists for.
        let token = CancellationToken::new();
        let id = format!("external-{}", self.tasks.lock().unwrap().len());
        let object = TaskObject {
            task_id: id.clone(),
            status: TaskStatus::Working,
            status_message: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            last_updated_at: "2026-01-01T00:00:00Z".to_string(),
            ttl,
            poll_interval: None,
            meta: None,
            result: None,
            error: None,
        };
        self.tasks.lock().unwrap().insert(
            id.clone(),
            Record {
                object,
                owner,
                result: None,
                error: None,
                token: token.clone(),
            },
        );
        Ok((id, token))
    }

    async fn get_task(&self, task_id: &str) -> TaskResult<Option<TaskObject>> {
        Ok(self.with(task_id, |r| r.object.clone()))
    }

    async fn get_task_result(&self, task_id: &str) -> TaskResult<Option<TaskSnapshot>> {
        Ok(self.with(task_id, |r| {
            (r.object.clone(), r.result.clone(), r.error.clone())
        }))
    }

    async fn task_owner(&self, task_id: &str) -> TaskResult<Option<TaskOwner>> {
        Ok(self.with(task_id, |r| r.owner.clone()))
    }

    async fn list_tasks(&self, status: Option<TaskStatus>) -> TaskResult<Vec<TaskObject>> {
        let tasks = self.tasks.lock().map_err(|e| TaskStoreError::Backend(e.to_string()))?;
        Ok(tasks
            .values()
            .map(|r| r.object.clone())
            .filter(|t| status.is_none_or(|want| t.status == want))
            .collect())
    }

    async fn require_input(
        &self,
        _task_id: &str,
        _requests: InputRequests,
        _message: Option<&str>,
    ) -> TaskResult<bool> {
        Ok(false)
    }

    async fn outstanding_input_requests(
        &self,
        _task_id: &str,
    ) -> TaskResult<Option<InputRequests>> {
        Ok(Some(Default::default()))
    }

    async fn apply_input_responses(
        &self,
        _task_id: &str,
        _responses: InputResponses,
    ) -> TaskResult<Option<tower_mcp::async_task::AppliedInputResponses>> {
        Ok(None)
    }

    async fn set_ttl(&self, task_id: &str, ttl_ms: u64) -> TaskResult<bool> {
        Ok(self
            .with(task_id, |r| r.object.ttl = Some(ttl_ms))
            .is_some())
    }

    async fn complete_task(&self, task_id: &str, result: CallToolResult) -> TaskResult<bool> {
        Ok(self
            .with(task_id, |r| {
                r.object.status = TaskStatus::Completed;
                r.result = Some(result);
            })
            .is_some())
    }

    async fn fail_task(&self, task_id: &str, error: JsonRpcError) -> TaskResult<bool> {
        Ok(self
            .with(task_id, |r| {
                r.object.status = TaskStatus::Failed;
                r.error = Some(error);
            })
            .is_some())
    }

    async fn cancel_task(
        &self,
        task_id: &str,
        reason: Option<&str>,
    ) -> TaskResult<Option<TaskObject>> {
        Ok(self.with(task_id, |r| {
            r.token.cancel();
            if !r.object.status.is_terminal() {
                r.object.status = TaskStatus::Cancelled;
                r.object.status_message = reason.map(str::to_string);
            }
            r.object.clone()
        }))
    }

    async fn wait_for_completion(&self, task_id: &str) -> TaskResult<Option<TaskSnapshot>> {
        // A polling store is a legitimate implementation; the router only
        // needs an eventual answer.
        for _ in 0..200 {
            if let Some(snapshot) = self.get_task_result(task_id).await? {
                if snapshot.0.status.is_terminal() {
                    return Ok(Some(snapshot));
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        self.get_task_result(task_id).await
    }

    async fn set_task_meta(&self, task_id: &str, meta: serde_json::Value) -> TaskResult<bool> {
        Ok(self
            .with(task_id, |r| r.object.meta = Some(meta))
            .is_some())
    }

    async fn discard_task(&self, task_id: &str) -> TaskResult<bool> {
        Ok(self
            .tasks
            .lock()
            .map_err(|e| TaskStoreError::Backend(e.to_string()))?
            .remove(task_id)
            .is_some())
    }
}

/// The store drives a real task to completion through the router.
#[tokio::test]
async fn an_external_store_can_back_a_task() {
    let tool = ToolBuilder::new("work")
        .description("Completes")
        .task_support(tower_mcp::protocol::TaskSupportMode::Optional)
        .handler(|_input: serde_json::Value| async move { Ok(CallToolResult::text("done")) })
        .build();

    let store = Arc::new(ExternalStore::default());
    let router = McpRouter::new()
        .server_info("external", "1.0.0")
        .task_store(store.clone())
        .tool(tool)
        .with_tasks();

    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .expect("connect");
    client.initialize("t", "1.0.0").await.expect("init");

    let task_id = client
        .call_tool_as_task("work", json!({}), None)
        .await
        .expect("task created")
        .task
        .task_id;
    assert!(
        task_id.starts_with("external-"),
        "the external store minted the id: {task_id}"
    );

    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if store.get_task(&task_id).await.unwrap().unwrap().status == TaskStatus::Completed {
            let (_, result, _) = store.get_task_result(&task_id).await.unwrap().unwrap();
            assert_eq!(result.unwrap().all_text(), "done");
            return;
        }
    }
    panic!("task never completed through the external store");
}

/// The token an external store constructs is the one the router raises.
#[tokio::test]
async fn a_constructed_token_carries_cancellation() {
    let token = CancellationToken::new();
    let clone = token.clone();
    assert!(!token.is_cancelled());
    clone.cancel();
    assert!(
        token.is_cancelled(),
        "every clone observes the same flag, which is what the store relies on"
    );
    assert!(CancellationToken::default().is_cancelled().eq(&false));
}
