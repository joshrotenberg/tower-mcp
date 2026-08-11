//! Live task execution (#1246).
//!
//! The distinguishing property is that the handler future stays alive across
//! an input round trip. A replayed handler is invoked again from the top,
//! which for a handler owning a subprocess or an open stream starts a second
//! operation instead of continuing the first. These tests assert the handler
//! runs exactly once no matter how many rounds of input it asks for.

#![cfg(feature = "stateless")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::Deserialize;
use serde_json::json;
use tower_mcp::async_task::{MemoryTaskStore, TaskStore};
use tower_mcp::client::{ChannelTransport, McpClient};
use tower_mcp::protocol::{
    ElicitAction, ElicitFormParams, ElicitFormSchema, ElicitRequestParams, ElicitResult,
    InputRequest, InputRequests, InputResponse, TaskStatus,
};
use tower_mcp::schemars::JsonSchema;
use tower_mcp::{CallToolResult, McpRouter, PanicPolicy, TaskContext, TaskOutcome, ToolBuilder};

#[derive(Debug, Deserialize, JsonSchema)]
struct NoArgs {}

fn ask(key: &str) -> InputRequests {
    let mut requests: InputRequests = Default::default();
    requests.insert(
        key.to_string(),
        InputRequest::Elicit(ElicitRequestParams::Form(ElicitFormParams {
            mode: None,
            message: format!("answer {key}?"),
            requested_schema: ElicitFormSchema::new(),
            meta: None,
        })),
    );
    requests
}

/// Wait for a task to reach a status, or panic with what it actually reached.
async fn await_status(store: &MemoryTaskStore, id: &str, want: TaskStatus) {
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let task = store.get_task(id).await.unwrap().unwrap();
        if task.status == want {
            return;
        }
    }
    let (task, result, error) = store.get_task_result(id).await.unwrap().unwrap();
    panic!(
        "wanted {want:?}, task sat at {:?} msg={:?} result={:?} error={:?}",
        task.status, task.status_message, result, error
    );
}

async fn answer(client: &McpClient, task_id: &str, key: &str) {
    client
        .task_update(
            task_id,
            [(
                key.to_string(),
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
        .expect("update accepted");
}

/// The acceptance criterion from #1246: one invocation across several rounds.
#[tokio::test]
async fn a_live_handler_runs_once_across_multiple_input_rounds() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let counter = invocations.clone();

    let tool = ToolBuilder::new("live")
        .description("Asks twice, runs once")
        .live_task_handler(move |task: TaskContext, _input: NoArgs| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                // State local to this future. A replayed handler would lose it.
                let mut seen = Vec::new();
                let first = task.require_input(ask("one")).await?;
                seen.extend(first.into_keys());
                let second = task.require_input(ask("two")).await?;
                seen.extend(second.into_keys());
                seen.sort();
                Ok(TaskOutcome::Completed(CallToolResult::text(seen.join(","))))
            }
        })
        .build();

    let store = Arc::new(MemoryTaskStore::new());
    let router = McpRouter::new()
        .server_info("live", "1.0.0")
        .task_store(store.clone())
        .tool(tool)
        .with_tasks();

    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .expect("connect");
    client.initialize("t", "1.0.0").await.expect("init");

    let task_id = client
        .call_tool_as_task("live", json!({}), None)
        .await
        .expect("task created")
        .task
        .task_id;

    await_status(&store, &task_id, TaskStatus::InputRequired).await;
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "the handler starts once"
    );
    answer(&client, &task_id, "one").await;

    await_status(&store, &task_id, TaskStatus::InputRequired).await;
    answer(&client, &task_id, "two").await;

    await_status(&store, &task_id, TaskStatus::Completed).await;
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "and is never invoked again, however many rounds it asks for"
    );

    let (_, result, _) = store.get_task_result(&task_id).await.unwrap().unwrap();
    assert_eq!(
        result.unwrap().all_text(),
        "one,two",
        "state accumulated across both rounds survived, which replay could not do"
    );
}

/// A live task records no invocation arguments, which is how a server keeps
/// prompts or credentials out of durable storage.
#[tokio::test]
async fn a_live_task_persists_no_arguments() {
    let tool = ToolBuilder::new("live")
        .description("Completes immediately")
        .live_task_handler(|_task: TaskContext, _input: serde_json::Value| async move {
            Ok(TaskOutcome::Completed(CallToolResult::text("ok")))
        })
        .build();

    let store = Arc::new(MemoryTaskStore::new());
    let router = McpRouter::new()
        .server_info("live", "1.0.0")
        .task_store(store.clone())
        .tool(tool)
        .with_tasks();

    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .expect("connect");
    client.initialize("t", "1.0.0").await.expect("init");

    let task_id = client
        .call_tool_as_task("live", json!({"secret": "hunter2"}), None)
        .await
        .expect("task created")
        .task
        .task_id;
    await_status(&store, &task_id, TaskStatus::Completed).await;

    let resume = store.resume_context(&task_id).await.unwrap().unwrap();
    assert_eq!(
        resume.arguments,
        serde_json::Value::Null,
        "the secret must never have been written to the store"
    );
}

/// Cancellation is signalled, not imposed. The task stays non-terminal until
/// the handler says it finished unwinding.
#[tokio::test]
async fn cancellation_is_confirmed_by_the_handler_rather_than_imposed() {
    let tore_down = Arc::new(AtomicUsize::new(0));
    let counter = tore_down.clone();

    let tool = ToolBuilder::new("live")
        .description("Waits, then unwinds on cancel")
        .live_task_handler(move |task: TaskContext, _input: NoArgs| {
            let counter = counter.clone();
            async move {
                // Propagating with `?` is the ordinary way to unwind.
                let result = task.require_input(ask("never")).await;
                counter.fetch_add(1, Ordering::SeqCst);
                result?;
                Ok(TaskOutcome::Completed(CallToolResult::text("unreachable")))
            }
        })
        .build();

    let store = Arc::new(MemoryTaskStore::new());
    let router = McpRouter::new()
        .server_info("live", "1.0.0")
        .task_store(store.clone())
        .tool(tool)
        .with_tasks();

    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .expect("connect");
    client.initialize("t", "1.0.0").await.expect("init");

    let task_id = client
        .call_tool_as_task("live", json!({}), None)
        .await
        .expect("task created")
        .task
        .task_id;
    await_status(&store, &task_id, TaskStatus::InputRequired).await;

    client
        .task_cancel(&task_id, None)
        .await
        .expect("cancel accepted");

    await_status(&store, &task_id, TaskStatus::Cancelled).await;
    assert_eq!(
        tore_down.load(Ordering::SeqCst),
        1,
        "the handler ran its teardown rather than being abandoned"
    );
}

// ============================================================================
// Clone, prefix, and nest (#1295)
// ============================================================================

/// #1295: `Tool::clone` and `Tool::with_name_prefix` set `live_handler: None`,
/// so a cloned or prefixed live tool still advertised
/// `TaskSupportMode::Required` while having neither a live handler nor an
/// ordinary service. Ordinary router lookup clones `Arc<Tool>` and stayed
/// correct, which is why nothing caught it.
///
/// This asserts the nested tool actually runs a task to completion. Checking
/// that the field survived would pass even if the clone were otherwise broken.
#[tokio::test]
async fn a_nested_live_tool_still_runs() {
    let tool = ToolBuilder::new("run")
        .description("Completes immediately")
        .live_task_handler(|_task: TaskContext, _input: NoArgs| async move {
            Ok(TaskOutcome::Completed(CallToolResult::text("ran")))
        })
        .build();

    let inner = McpRouter::new().server_info("inner", "1.0.0").tool(tool);

    let store = Arc::new(MemoryTaskStore::new());
    let outer = McpRouter::new()
        .server_info("outer", "1.0.0")
        .task_store(store.clone())
        .nest("provider", inner)
        .with_tasks();

    let client = McpClient::connect(ChannelTransport::new(outer))
        .await
        .expect("connect");
    client.initialize("t", "1.0.0").await.expect("init");

    let names: Vec<String> = client
        .list_tools()
        .await
        .expect("list")
        .tools
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(
        names.iter().any(|n| n == "provider.run"),
        "nested tool must be listed: {names:?}"
    );

    let task_id = client
        .call_tool_as_task("provider.run", json!({}), None)
        .await
        .expect("task created")
        .task
        .task_id;
    await_status(&store, &task_id, TaskStatus::Completed).await;

    let (_, result, _) = store.get_task_result(&task_id).await.unwrap().unwrap();
    assert_eq!(
        result.unwrap().all_text(),
        "ran",
        "the nested live handler must actually execute"
    );
}

/// A guard on a live tool must reject through the ordinary domain-error path
/// rather than panicking on the absent service.
#[tokio::test]
async fn a_guard_on_a_live_tool_rejects_without_panicking() {
    let tool = ToolBuilder::new("guarded")
        .description("Always rejected")
        .live_task_handler(|_task: TaskContext, _input: NoArgs| async move {
            Ok(TaskOutcome::Completed(CallToolResult::text(
                "should not run",
            )))
        })
        .build()
        .with_guard(|_req: &tower_mcp::ToolRequest| Err("nope".to_string()));

    let store = Arc::new(MemoryTaskStore::new());
    let router = McpRouter::new()
        .server_info("guarded", "1.0.0")
        .task_store(store.clone())
        .tool(tool)
        .with_tasks();

    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .expect("connect");
    client.initialize("t", "1.0.0").await.expect("init");

    let task_id = client
        .call_tool_as_task("guarded", json!({}), None)
        .await
        .expect("task created")
        .task
        .task_id;
    await_status(&store, &task_id, TaskStatus::Completed).await;

    let (_, result, _) = store.get_task_result(&task_id).await.unwrap().unwrap();
    let result = result.expect("a rejected call still produces a result");
    assert!(result.is_error, "the guard rejection is a domain error");
    assert!(result.all_text().contains("nope"));
}

// ============================================================================
// Cancellation ordering (#1294)
// ============================================================================

/// #1294, startup window: the spawned future inspected the store token before
/// registering its live handle, so a `tasks/cancel` in between found no handle,
/// took the store path, terminalized, and acknowledged, after which the handler
/// ran on against an already-cancelled task.
///
/// This asserts the invariant, not the race. The two statements have no `await`
/// between them, so the window only exists across threads and this test passes
/// against the old ordering as well; it was checked. What it does guard is that
/// a cancel arriving before the handler starts is still observed at all, which
/// is the property the reordering must not break.
#[tokio::test]
async fn cancelling_before_the_handler_starts_is_still_observed() {
    let started = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let (s, f) = (started.clone(), finished.clone());

    let tool = ToolBuilder::new("live")
        .description("Waits forever unless cancelled")
        .live_task_handler(move |task: TaskContext, _input: NoArgs| {
            let (s, f) = (s.clone(), f.clone());
            async move {
                s.fetch_add(1, Ordering::SeqCst);
                // Never answered, so only cancellation ends this.
                let result = task.require_input(ask("never")).await;
                f.fetch_add(1, Ordering::SeqCst);
                result?;
                Ok(TaskOutcome::Completed(CallToolResult::text("unreachable")))
            }
        })
        .build();

    let store = Arc::new(MemoryTaskStore::new());
    let router = McpRouter::new()
        .server_info("live", "1.0.0")
        .task_store(store.clone())
        .tool(tool)
        .with_tasks();

    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .expect("connect");
    client.initialize("t", "1.0.0").await.expect("init");

    let task_id = client
        .call_tool_as_task("live", json!({}), None)
        .await
        .expect("task created")
        .task
        .task_id;

    // No sleep: cancel as soon as the handle exists.
    client.task_cancel(&task_id, None).await.expect("cancel");

    await_status(&store, &task_id, TaskStatus::Cancelled).await;
    assert_eq!(
        started.load(Ordering::SeqCst),
        finished.load(Ordering::SeqCst),
        "a handler that started must also have unwound; the previous ordering \
         left it running against a cancelled task"
    );
}

/// #1294, completion window: the handle was unregistered before the outcome was
/// persisted, so a cancel in that interval took the store path and could write
/// `cancelled` over a task whose handler had already produced a result.
///
/// A completion is allowed to win the race, which is the documented semantic.
#[tokio::test]
async fn a_completion_is_not_overwritten_by_a_late_cancellation() {
    let tool = ToolBuilder::new("live")
        .description("Completes immediately")
        .live_task_handler(|_task: TaskContext, _input: NoArgs| async move {
            Ok(TaskOutcome::Completed(CallToolResult::text("done")))
        })
        .build();

    let store = Arc::new(MemoryTaskStore::new());
    let router = McpRouter::new()
        .server_info("live", "1.0.0")
        .task_store(store.clone())
        .tool(tool)
        .with_tasks();

    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .expect("connect");
    client.initialize("t", "1.0.0").await.expect("init");

    // Repeated because the interesting interleaving is timing-dependent. This
    // does not reliably reproduce the old bug either; it asserts that whichever
    // writer wins, the recorded status and the recorded result agree.
    for round in 0..25 {
        let task_id = client
            .call_tool_as_task("live", json!({}), None)
            .await
            .expect("task created")
            .task
            .task_id;
        let _ = client.task_cancel(&task_id, None).await;

        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            let task = store.get_task(&task_id).await.unwrap().unwrap();
            if task.status.is_terminal() {
                let (_, result, _) = store.get_task_result(&task_id).await.unwrap().unwrap();
                // Whichever wins, the two must agree: a task recorded as
                // completed must carry the handler's result, and a cancelled
                // one must not claim a result it never produced.
                match task.status {
                    TaskStatus::Completed => assert_eq!(
                        result.expect("completed carries its result").all_text(),
                        "done",
                        "round {round}"
                    ),
                    TaskStatus::Cancelled => {}
                    other => panic!("round {round}: unexpected terminal {other:?}"),
                }
                break;
            }
        }
    }
}

// ============================================================================
// Panic isolation (#1305)
// ============================================================================

fn panicking_live_router(catch: bool) -> (Arc<MemoryTaskStore>, McpRouter) {
    let boom = ToolBuilder::new("boom")
        .description("Panics")
        .live_task_handler(|_task: TaskContext, _input: NoArgs| async move {
            panic!("live handler exploded");
            #[allow(unreachable_code)]
            Ok(TaskOutcome::Completed(CallToolResult::text("unreachable")))
        })
        .build();
    let fine = ToolBuilder::new("fine")
        .description("Works")
        .live_task_handler(|_task: TaskContext, _input: NoArgs| async move {
            Ok(TaskOutcome::Completed(CallToolResult::text("ok")))
        })
        .build();

    let store = Arc::new(MemoryTaskStore::new());
    let router = McpRouter::new()
        .server_info("panic-live", "1.0.0")
        .task_store(store.clone())
        .tool(boom)
        .tool(fine)
        .with_tasks();
    let router = if catch { router.catch_panics() } else { router };
    (store, router)
}

/// #1305: the live branch had no panic boundary, so a panicking handler
/// unwound before any terminal state was written. The task sat at `working`
/// forever: not failed, not cancelled, and nothing running.
#[tokio::test]
async fn a_panicking_live_handler_reaches_a_terminal_state() {
    let (store, router) = panicking_live_router(true);
    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .expect("connect");
    client.initialize("t", "1.0.0").await.expect("init");

    let task_id = client
        .call_tool_as_task("boom", json!({}), None)
        .await
        .expect("task created")
        .task
        .task_id;

    await_status(&store, &task_id, TaskStatus::Failed).await;
    let (_, _, error) = store.get_task_result(&task_id).await.unwrap().unwrap();
    let error = error.expect("a failed task carries a structured error");
    assert!(
        error.message.contains("panicked"),
        "the failure must say what happened: {}",
        error.message
    );

    // The decisive part: the server still serves.
    let next = client
        .call_tool_as_task("fine", json!({}), None)
        .await
        .expect("the server must still be serving")
        .task
        .task_id;
    await_status(&store, &next, TaskStatus::Completed).await;
}

/// #1306: live execution uses the same root-router disclosure policy as
/// ordinary and replayed handlers while retaining its `failed` Task outcome.
#[tokio::test]
async fn a_panicking_live_handler_uses_the_redacted_policy() {
    const TOOL_NAME: &str = "private.provider.live";
    const PAYLOAD: &str = "secret live provider payload";
    const SAFE_MESSAGE: &str = "internal tool failure";

    let boom = ToolBuilder::new(TOOL_NAME)
        .description("Panics")
        .live_task_handler(|_task: TaskContext, _input: NoArgs| async move {
            panic!("{PAYLOAD}");
            #[allow(unreachable_code)]
            Ok(TaskOutcome::Completed(CallToolResult::text("unreachable")))
        })
        .build();
    let fine = ToolBuilder::new("fine")
        .description("Works")
        .live_task_handler(|_task: TaskContext, _input: NoArgs| async move {
            Ok(TaskOutcome::Completed(CallToolResult::text("ok")))
        })
        .build();
    let store = Arc::new(MemoryTaskStore::new());
    let router = McpRouter::new()
        .server_info("panic-live", "1.0.0")
        .task_store(store.clone())
        .tool(boom)
        .tool(fine)
        .with_tasks()
        .catch_panics_with(PanicPolicy::redacted(SAFE_MESSAGE));
    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .expect("connect");
    client.initialize("t", "1.0.0").await.expect("init");

    let task_id = client
        .call_tool_as_task(TOOL_NAME, json!({}), None)
        .await
        .expect("task created")
        .task
        .task_id;
    await_status(&store, &task_id, TaskStatus::Failed).await;

    let (_, _, error) = store.get_task_result(&task_id).await.unwrap().unwrap();
    let error = error.expect("a failed task carries a structured error");
    assert_eq!(error.message, SAFE_MESSAGE);
    assert!(!error.message.contains(TOOL_NAME));
    assert!(!error.message.contains(PAYLOAD));

    let next = client
        .call_tool_as_task("fine", json!({}), None)
        .await
        .expect("the server must still be serving")
        .task
        .task_id;
    await_status(&store, &next, TaskStatus::Completed).await;
}

/// The registry entry must be released however the handler leaves, so a later
/// cancellation does not target a dead handle. This holds without
/// `catch_panics` too: opting out means the bug stays visible, not that
/// registry state leaks.
#[tokio::test]
async fn a_panicking_live_handler_releases_its_registry_entry() {
    for catch in [true, false] {
        let (store, router) = panicking_live_router(catch);
        let client = McpClient::connect(ChannelTransport::new(router))
            .await
            .expect("connect");
        client.initialize("t", "1.0.0").await.expect("init");

        let task_id = client
            .call_tool_as_task("boom", json!({}), None)
            .await
            .expect("task created")
            .task
            .task_id;

        // Let the handler panic and unwind.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // With a stale entry the router takes the live path, signals a handle
        // nobody reads, and leaves the task non-terminal. With the entry gone
        // it takes the store path and terminalizes.
        let cancellation = client.task_cancel(&task_id, None).await;
        if catch {
            assert!(
                cancellation.is_err(),
                "a caught panic is already Failed; a successful legacy cancel would mean a stale live registration handled it"
            );
        } else {
            cancellation.expect("an uncaught panic leaves a working task for store cancellation");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let task = store.get_task(&task_id).await.unwrap().unwrap();
        assert!(
            task.status.is_terminal(),
            "catch_panics={catch}: cancelling after a panic must terminalize, \
             which a stale registry entry would prevent (status {:?})",
            task.status
        );
    }
}
