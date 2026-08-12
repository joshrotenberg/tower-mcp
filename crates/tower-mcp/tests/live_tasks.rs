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
    InputRequest, InputRequests, InputRequiredResult, InputResponse, RequestOutcome, TaskStatus,
};
use tower_mcp::schemars::JsonSchema;
use tower_mcp::{
    CallToolResult, McpRouter, PanicPolicy, ProtocolSupport, RequestContext, TaskContext,
    TaskOutcome, Tool, ToolBuilder,
};

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

// ============================================================================
// Request context in live handlers (#1301)
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
struct Principal(String);

/// #1301: the internal trait carried the `RequestContext` from #1298, but the
/// public closure adapter discarded it, so a live handler could not read an
/// authenticated principal, a trace id, or anything task preparation added
/// without keeping its own registry keyed by task id.
#[tokio::test]
async fn a_contextual_live_handler_sees_a_preparation_extension() {
    let tool = ToolBuilder::new("whoami")
        .description("Reports the prepared principal")
        .live_task_handler_with_context(
            |ctx: RequestContext, _task: TaskContext, _input: NoArgs| async move {
                let who = ctx
                    .extension::<Principal>()
                    .map(|p| p.0.clone())
                    .unwrap_or_else(|| "absent".to_string());
                Ok(TaskOutcome::Completed(CallToolResult::text(who)))
            },
        )
        .build()
        .with_task_preparation(|_task: TaskContext, _args: serde_json::Value| async move {
            Ok(tower_mcp::TaskPreparation::new()
                .with_extension(Principal("prepared-caller".to_string())))
        });

    let store = Arc::new(MemoryTaskStore::new());
    let router = McpRouter::new()
        .server_info("ctx", "1.0.0")
        .task_store(store.clone())
        .tool(tool)
        .with_tasks();

    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .expect("connect");
    client.initialize("t", "1.0.0").await.expect("init");

    let task_id = client
        .call_tool_as_task("whoami", json!({}), None)
        .await
        .expect("task created")
        .task
        .task_id;
    await_status(&store, &task_id, TaskStatus::Completed).await;

    let (_, result, _) = store.get_task_result(&task_id).await.unwrap().unwrap();
    assert_eq!(
        result.unwrap().all_text(),
        "prepared-caller",
        "the preparation extension must reach the live handler"
    );
}

/// #1295 was exactly this bug class for the simple form, so the contextual
/// form is checked on the same transformation paths rather than assumed to
/// inherit the fix.
#[tokio::test]
async fn a_contextual_live_handler_survives_nest_and_guard() {
    let tool = ToolBuilder::new("run")
        .description("Contextual")
        .live_task_handler_with_context(
            |_ctx: RequestContext, _task: TaskContext, _input: NoArgs| async move {
                Ok(TaskOutcome::Completed(CallToolResult::text("ran")))
            },
        )
        .build()
        // A guard that admits everything still replaces the handler, so this
        // exercises the guarded wrapper surviving the prefix as well.
        .with_guard(|_req: &tower_mcp::ToolRequest| Ok(()));

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

    let task_id = client
        .call_tool_as_task("provider.run", json!({}), None)
        .await
        .expect("task created")
        .task
        .task_id;
    await_status(&store, &task_id, TaskStatus::Completed).await;

    let (_, result, _) = store.get_task_result(&task_id).await.unwrap().unwrap();
    assert_eq!(result.unwrap().all_text(), "ran");
}

/// The simple two-argument form stays source-compatible, which the other
/// tests in this file already exercise; this pins it explicitly.
#[tokio::test]
async fn the_simple_live_form_still_compiles_and_runs() {
    let tool = ToolBuilder::new("simple")
        .description("Two-argument form")
        .live_task_handler(|_task: TaskContext, _input: NoArgs| async move {
            Ok(TaskOutcome::Completed(CallToolResult::text("simple")))
        })
        .build();

    let store = Arc::new(MemoryTaskStore::new());
    let router = McpRouter::new()
        .server_info("simple", "1.0.0")
        .task_store(store.clone())
        .tool(tool)
        .with_tasks();

    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .expect("connect");
    client.initialize("t", "1.0.0").await.expect("init");
    let task_id = client
        .call_tool_as_task("simple", json!({}), None)
        .await
        .expect("task created")
        .task
        .task_id;
    await_status(&store, &task_id, TaskStatus::Completed).await;
}

// ============================================================================
// Two-phase parking (#1246)
// ============================================================================
//
// `park_input` commits the `input_required` transition and returns without
// suspending, so an execution owner can release admission permits between the
// commit and the wait. That opens a window in which arbitrary code runs, and
// these two tests are the reason the window is safe to open: a response and a
// cancellation are each delivered entirely inside it, and neither is lost.
//
// The handshake is deterministic rather than timed. `Notify::notify_one`
// stores a permit when nobody is waiting, so neither side can miss the other
// no matter which arrives first.

/// An answer that lands after `park_input` and before `wait` must still be
/// delivered. Nothing is waiting on the notification when it fires, so a
/// design that relied on the wakeup alone would hang here forever.
#[tokio::test]
async fn an_answer_that_lands_before_wait_is_not_lost() {
    let parked = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let handler_parked = parked.clone();
    let handler_release = release.clone();

    let tool = ToolBuilder::new("live")
        .description("Parks, stands, then waits")
        .live_task_handler(move |task: TaskContext, _input: NoArgs| {
            let parked = handler_parked.clone();
            let release = handler_release.clone();
            async move {
                let pending = task.park_input(ask("one")).await?;
                // Durably parked, and this handler has not suspended yet.
                // An admission-controlled owner releases its permits here.
                parked.notify_one();
                release.notified().await;

                let answers = pending.wait().await?;
                let keys: Vec<String> = answers.into_keys().collect();
                Ok(TaskOutcome::Completed(CallToolResult::text(keys.join(","))))
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

    // The park has committed, so the question is already visible to a client.
    parked.notified().await;
    await_status(&store, &task_id, TaskStatus::InputRequired).await;

    // Answer while the handler is standing between the two calls.
    answer(&client, &task_id, "one").await;

    release.notify_one();
    await_status(&store, &task_id, TaskStatus::Completed).await;

    let (_, result, _) = store.get_task_result(&task_id).await.unwrap().unwrap();
    let text = serde_json::to_value(result.unwrap()).unwrap()["content"][0]["text"].clone();
    assert_eq!(
        text, "one",
        "the answer arrived while nothing was waiting for it, and must still be delivered"
    );
}

/// The same window, for cancellation. `wait` checks the flag before it awaits
/// anything, so a cancellation raised in the gap ends the task rather than
/// leaving it parked on a question nobody will answer.
#[tokio::test]
async fn a_cancellation_that_lands_before_wait_is_observed() {
    let parked = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let handler_parked = parked.clone();
    let handler_release = release.clone();

    let tool = ToolBuilder::new("live")
        .description("Parks, stands, then waits")
        .live_task_handler(move |task: TaskContext, _input: NoArgs| {
            let parked = handler_parked.clone();
            let release = handler_release.clone();
            async move {
                let pending = task.park_input(ask("never")).await?;
                parked.notify_one();
                release.notified().await;

                // Propagates as TaskCancelled, which the router turns into a
                // cancelled outcome without the handler writing a select!.
                let _answers = pending.wait().await?;
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

    parked.notified().await;
    await_status(&store, &task_id, TaskStatus::InputRequired).await;

    client
        .task_cancel(&task_id, Some("changed my mind".to_string()))
        .await
        .expect("cancel accepted");

    release.notify_one();
    await_status(&store, &task_id, TaskStatus::Cancelled).await;
}

/// `require_input` is the two calls back to back, so it must keep behaving
/// exactly as it did. This is the same round trip driven through the
/// compatibility wrapper rather than the split.
#[tokio::test]
async fn require_input_still_parks_and_waits_in_one_call() {
    let tool = ToolBuilder::new("live")
        .description("Uses the combined call")
        .live_task_handler(move |task: TaskContext, _input: NoArgs| async move {
            let answers = task.require_input(ask("one")).await?;
            let keys: Vec<String> = answers.into_keys().collect();
            Ok(TaskOutcome::Completed(CallToolResult::text(keys.join(","))))
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
    answer(&client, &task_id, "one").await;
    await_status(&store, &task_id, TaskStatus::Completed).await;

    let (_, result, _) = store.get_task_result(&task_id).await.unwrap().unwrap();
    let text = serde_json::to_value(result.unwrap()).unwrap()["content"][0]["text"].clone();
    assert_eq!(text, "one");
}

// ============================================================================
// Live plus fallback (#1246)
// ============================================================================
//
// A `Tool` can now carry a live handler and a synchronous/MRTR fallback at
// the same time: the live handler runs when a call is task-backed, the
// fallback runs when it is not. One tool, one schema, one name -- the router
// already picked between the two by how the call arrived (see `router.rs`);
// what was missing was the ability to build a `Tool` that carries both.

#[derive(Debug, Deserialize, JsonSchema)]
struct MultiInput {
    value: String,
    #[serde(default)]
    ask: bool,
}

/// Bounds an awaited client call so a routing regression that panics a
/// connection task -- rather than returning a clean error -- fails this test
/// instead of hanging the run. Found empirically while mutation-testing
/// `Tool::clone`: dropping the fallback there does not surface as a client
/// error, because the panic happens inside `ChannelTransport`'s background
/// task and the client is left waiting on a response that will never come.
async fn timed<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(std::time::Duration::from_secs(5), fut)
        .await
        .expect("call timed out; the connection likely panicked without responding")
}

/// One tool, a live handler and an MRTR fallback, both counted so a test can
/// assert which side actually ran rather than only checking the result text.
fn multi_tool(live_calls: Arc<AtomicUsize>, fallback_calls: Arc<AtomicUsize>) -> Tool {
    ToolBuilder::new("multi")
        .description("Live when Tasks are negotiated, MRTR/synchronous otherwise")
        .live_task_handler(move |_task: TaskContext, input: MultiInput| {
            let live_calls = live_calls.clone();
            async move {
                live_calls.fetch_add(1, Ordering::SeqCst);
                Ok(TaskOutcome::Completed(CallToolResult::text(format!(
                    "live:{}",
                    input.value
                ))))
            }
        })
        .fallback_mrtr_handler(move |ctx: RequestContext, input: MultiInput| {
            let fallback_calls = fallback_calls.clone();
            async move {
                fallback_calls.fetch_add(1, Ordering::SeqCst);
                // A non-empty request_state means this is the client's retry
                // after answering the question below.
                if ctx.request_state().is_some() {
                    return Ok(RequestOutcome::Complete(CallToolResult::text(format!(
                        "resumed:{}",
                        input.value
                    ))));
                }
                if input.ask {
                    return Ok(RequestOutcome::input_required(
                        InputRequiredResult::new().with_request_state("confirm"),
                    ));
                }
                Ok(RequestOutcome::Complete(CallToolResult::text(format!(
                    "sync:{}",
                    input.value
                ))))
            }
        })
        .build()
}

/// A plain, non-task-backed `tools/call` (legacy protocol, no `task` param)
/// must reach the fallback. The live handler has nothing to run for a call
/// that never became a task, so it must not run at all.
#[tokio::test]
async fn a_live_plus_fallback_tool_serves_a_plain_call_through_the_fallback() {
    let live_calls = Arc::new(AtomicUsize::new(0));
    let fallback_calls = Arc::new(AtomicUsize::new(0));
    let tool = multi_tool(live_calls.clone(), fallback_calls.clone());

    let store = Arc::new(MemoryTaskStore::new());
    let router = McpRouter::new()
        .server_info("multi", "1.0.0")
        .task_store(store.clone())
        .tool(tool)
        .with_tasks();

    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .expect("connect");
    client.initialize("t", "1.0.0").await.expect("init");

    let result = timed(client.call_tool("multi", json!({"value": "a"})))
        .await
        .expect("call");
    assert!(!result.is_error);
    assert_eq!(result.all_text(), "sync:a");
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        live_calls.load(Ordering::SeqCst),
        0,
        "the live handler must not run for a plain call"
    );
}

/// The same tool, task-requested, must reach the live handler and never the
/// fallback -- distinguishing this from "any handler ran" is the point.
#[tokio::test]
async fn a_live_plus_fallback_tool_serves_a_task_backed_call_through_the_live_handler() {
    let live_calls = Arc::new(AtomicUsize::new(0));
    let fallback_calls = Arc::new(AtomicUsize::new(0));
    let tool = multi_tool(live_calls.clone(), fallback_calls.clone());

    let store = Arc::new(MemoryTaskStore::new());
    let router = McpRouter::new()
        .server_info("multi", "1.0.0")
        .task_store(store.clone())
        .tool(tool)
        .with_tasks();

    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .expect("connect");
    client.initialize("t", "1.0.0").await.expect("init");

    let task_id = timed(client.call_tool_as_task("multi", json!({"value": "b"}), None))
        .await
        .expect("task created")
        .task
        .task_id;

    await_status(&store, &task_id, TaskStatus::Completed).await;
    let (_, result, _) = store.get_task_result(&task_id).await.unwrap().unwrap();
    assert_eq!(result.unwrap().all_text(), "live:b");
    assert_eq!(live_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fallback_calls.load(Ordering::SeqCst),
        0,
        "the fallback must not run for a task-backed call"
    );
}

/// The same tool again, this time driving the fallback through an SEP-2322
/// MRTR round trip. `InputRequiredResult` from a synchronous `tools/call` is
/// only accepted by the router from a 2026-07-28 caller, so this is the one
/// case in the file that needs the final protocol rather than legacy.
#[tokio::test]
async fn a_live_plus_fallback_tool_serves_an_mrtr_round_trip_through_the_fallback() {
    let live_calls = Arc::new(AtomicUsize::new(0));
    let fallback_calls = Arc::new(AtomicUsize::new(0));
    let tool = multi_tool(live_calls.clone(), fallback_calls.clone());

    let router = McpRouter::new()
        .server_info("multi", "1.0.0")
        .task_store(Arc::new(MemoryTaskStore::new()))
        .tool(tool)
        .with_tasks();

    // Final protocol, but this client never declares Tasks, so the router
    // never elects a task for it: every call reaches the fallback.
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .connect_simple(ChannelTransport::new(router))
        .await
        .expect("connect");
    client.discover("t", "1.0.0").await.expect("discover");

    let outcome =
        timed(client.call_tool_once("multi", json!({"value": "c", "ask": true}), None, None))
            .await
            .expect("tools/call");
    let required = outcome
        .as_input_required()
        .expect("the fallback asked for input");
    assert_eq!(required.request_state.as_deref(), Some("confirm"));
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);

    let outcome = timed(client.call_tool_once(
        "multi",
        json!({"value": "c", "ask": true}),
        None,
        required.request_state.clone(),
    ))
    .await
    .expect("tools/call retry");
    let result = outcome.as_complete().expect("the retry completes");
    assert_eq!(result.all_text(), "resumed:c");
    assert_eq!(
        fallback_calls.load(Ordering::SeqCst),
        2,
        "both rounds go through the fallback"
    );
    assert_eq!(
        live_calls.load(Ordering::SeqCst),
        0,
        "the live handler must not run for a non-task call"
    );
}

/// Regression: a live-only tool (no fallback registered) keeps its existing
/// `TaskSupportMode::Required` contract. A plain call must still be rejected
/// with a clear protocol error before any handler runs, not panic reaching
/// for a synchronous or MRTR path that was never registered.
#[tokio::test]
async fn a_live_only_tool_still_rejects_a_plain_call_without_panicking() {
    let tool = ToolBuilder::new("live_only")
        .description("No fallback")
        .live_task_handler(|_task: TaskContext, _input: NoArgs| async move {
            Ok(TaskOutcome::Completed(CallToolResult::text(
                "should not run",
            )))
        })
        .build();

    let store = Arc::new(MemoryTaskStore::new());
    let router = McpRouter::new()
        .server_info("live-only", "1.0.0")
        .task_store(store.clone())
        .tool(tool)
        .with_tasks();

    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .expect("connect");
    client.initialize("t", "1.0.0").await.expect("init");

    let error = timed(client.call_tool("live_only", json!({})))
        .await
        .expect_err("a live-only tool has nothing to run for a plain call");
    assert!(
        error.to_string().contains("requires async task execution"),
        "got: {error}"
    );
}

/// `Tool::clone` and `Tool::with_name_prefix` must carry a live handler and a
/// fallback together, not just whichever one #1295 already covered alone.
/// Asserted by actually running both paths post-clone/prefix, the same
/// discipline the #1295 tests above use: checking that a field is non-`None`
/// would pass even if the clone were otherwise broken.
#[tokio::test]
async fn clone_and_prefix_carry_both_the_live_handler_and_the_fallback() {
    let tool = ToolBuilder::new("multi")
        .description("Live plus fallback")
        .live_task_handler(|_task: TaskContext, input: MultiInput| async move {
            Ok(TaskOutcome::Completed(CallToolResult::text(format!(
                "live:{}",
                input.value
            ))))
        })
        .fallback_handler(|input: MultiInput| async move {
            Ok(CallToolResult::text(format!("sync:{}", input.value)))
        })
        .build();

    let cloned = tool.clone();
    let prefixed = tool.with_name_prefix("ns");

    let store = Arc::new(MemoryTaskStore::new());
    let router = McpRouter::new()
        .server_info("multi", "1.0.0")
        .task_store(store.clone())
        .tool(cloned)
        .tool(prefixed)
        .with_tasks();

    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .expect("connect");
    client.initialize("t", "1.0.0").await.expect("init");

    for name in ["multi", "ns.multi"] {
        let result = timed(client.call_tool(name, json!({"value": "x"})))
            .await
            .unwrap_or_else(|e| panic!("plain call to {name} failed: {e}"));
        assert_eq!(
            result.all_text(),
            "sync:x",
            "fallback must survive for {name}"
        );

        let task_id = timed(client.call_tool_as_task(name, json!({"value": "x"}), None))
            .await
            .unwrap_or_else(|e| panic!("task call to {name} failed: {e}"))
            .task
            .task_id;
        await_status(&store, &task_id, TaskStatus::Completed).await;
        let (_, result, _) = store.get_task_result(&task_id).await.unwrap().unwrap();
        assert_eq!(
            result.unwrap().all_text(),
            "live:x",
            "live handler must survive for {name}"
        );
    }
}

/// `Tool::with_guard` on a live-plus-fallback tool must reject both paths.
/// Before this PR's fix, `with_guard` returned after guarding whichever of
/// `live_handler`/`service`/`mrtr_handler` it found first, which was
/// harmless while those fields were mutually exclusive but silently left a
/// coexisting fallback unguarded once they no longer were.
#[tokio::test]
async fn a_guard_on_a_live_plus_fallback_tool_rejects_both_paths() {
    let tool = ToolBuilder::new("multi")
        .description("Live plus fallback")
        .live_task_handler(|_task: TaskContext, _input: NoArgs| async move {
            Ok(TaskOutcome::Completed(CallToolResult::text(
                "should not run (live)",
            )))
        })
        .fallback_handler(|_input: NoArgs| async move {
            Ok(CallToolResult::text("should not run (fallback)"))
        })
        .build()
        .with_guard(|_req: &tower_mcp::ToolRequest| Err("nope".to_string()));

    let store = Arc::new(MemoryTaskStore::new());
    let router = McpRouter::new()
        .server_info("multi", "1.0.0")
        .task_store(store.clone())
        .tool(tool)
        .with_tasks();

    let client = McpClient::connect(ChannelTransport::new(router))
        .await
        .expect("connect");
    client.initialize("t", "1.0.0").await.expect("init");

    // The fallback path is rejected.
    let result = timed(client.call_tool("multi", json!({})))
        .await
        .expect("call");
    assert!(
        result.is_error,
        "the guard must reject the fallback path too"
    );
    assert!(result.all_text().contains("nope"));

    // The live path is rejected too.
    let task_id = timed(client.call_tool_as_task("multi", json!({}), None))
        .await
        .expect("task created")
        .task
        .task_id;
    await_status(&store, &task_id, TaskStatus::Completed).await;
    let (_, result, _) = store.get_task_result(&task_id).await.unwrap().unwrap();
    let result = result.expect("a rejected call still produces a result");
    assert!(result.is_error);
    assert!(result.all_text().contains("nope"));
}
