//! Application-authenticated Task ownership (#1397).
//!
//! These tests use the public router seam rather than OAuth claims. They keep
//! the authorization contract strict: only the exact resolved owner can
//! observe or mutate a Task, and a broken resolver never degrades to an
//! anonymous principal.

#![cfg(feature = "stateless")]

use std::collections::HashMap;
use std::sync::Arc;

use tower::ServiceExt;
use tower_mcp::async_task::{MemoryTaskStore, TaskStore};
use tower_mcp::stateless::StatelessRequestMeta;
use tower_mcp::{
    CallToolParams, CallToolResult, CancelTaskParams, ClientCapabilities, Extensions,
    GetTaskInfoParams, McpRequest, McpResponse, McpRouter, RequestId, RouterRequest,
    SubscriptionFilter, SubscriptionsListenParams, TASKS_EXTENSION_ID, TaskSupportMode,
    ToolBuilder, UpdateTaskParams,
};

#[derive(Clone)]
struct Principal {
    issuer: &'static str,
    subject: &'static str,
}

#[derive(Clone)]
struct UnrelatedExtension(&'static str);

fn request_extensions(subject: Option<&'static str>) -> Extensions {
    let mut extensions = Extensions::new();
    extensions.insert(StatelessRequestMeta {
        protocol_version: Some(tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28.to_string()),
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
    if let Some(subject) = subject {
        extensions.insert(Principal {
            issuer: "https://identity.example",
            subject,
        });
    }
    extensions
}

fn base_task_router() -> McpRouter {
    let slow = ToolBuilder::new("slow")
        .task_support(TaskSupportMode::Optional)
        .handler(|_: serde_json::Value| async move {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            Ok(CallToolResult::text("done"))
        })
        .build();

    McpRouter::new().tool(slow).with_tasks()
}

fn task_router() -> McpRouter {
    base_task_router().task_owner_from_extension::<Principal>(|principal| {
        format!("{}#{}", principal.issuer, principal.subject)
    })
}

async fn dispatch(
    router: &McpRouter,
    id: i64,
    request: McpRequest,
    extensions: Extensions,
) -> Result<McpResponse, tower_mcp::JsonRpcError> {
    router
        .clone()
        .oneshot(RouterRequest {
            id: RequestId::Number(id),
            inner: request,
            extensions,
        })
        .await
        .unwrap()
        .inner
}

fn get(task_id: &str) -> McpRequest {
    McpRequest::GetTaskInfo(GetTaskInfoParams {
        task_id: task_id.to_string(),
        meta: None,
    })
}

fn update(task_id: &str) -> McpRequest {
    McpRequest::UpdateTask(UpdateTaskParams {
        task_id: task_id.to_string(),
        input_responses: HashMap::new(),
        meta: None,
    })
}

fn cancel(task_id: &str) -> McpRequest {
    McpRequest::CancelTask(CancelTaskParams {
        task_id: task_id.to_string(),
        reason: Some("test complete".to_string()),
        meta: None,
    })
}

fn listen(task_id: &str) -> McpRequest {
    McpRequest::SubscriptionsListen(SubscriptionsListenParams {
        notifications: Some(SubscriptionFilter {
            task_ids: Some(vec![task_id.to_string()]),
            ..Default::default()
        }),
        meta: None,
    })
}

#[tokio::test]
async fn application_principal_owns_every_task_operation() {
    let router = task_router();
    let mut alice = request_extensions(Some("alice"));
    alice.insert(UnrelatedExtension("trace-1"));
    assert_eq!(alice.get::<UnrelatedExtension>().unwrap().0, "trace-1");

    let created = dispatch(
        &router,
        1,
        McpRequest::CallTool(CallToolParams {
            name: "slow".to_string(),
            arguments: serde_json::json!({}),
            input_responses: None,
            request_state: None,
            meta: None,
            task: None,
        }),
        alice.clone(),
    )
    .await
    .unwrap();
    let McpResponse::FinalCreateTask(created) = created else {
        panic!("expected a Task creation response")
    };
    let task_id = created.task.metadata.task_id;

    assert!(
        dispatch(&router, 2, get(&task_id), alice.clone())
            .await
            .is_ok()
    );
    assert!(
        dispatch(&router, 3, update(&task_id), alice.clone())
            .await
            .is_ok()
    );

    // Changing an extension the resolver does not read cannot change the
    // owner. Removing the actual principal does.
    let mut alice_with_different_trace = request_extensions(Some("alice"));
    alice_with_different_trace.insert(UnrelatedExtension("trace-2"));
    assert_eq!(
        alice_with_different_trace
            .get::<UnrelatedExtension>()
            .unwrap()
            .0,
        "trace-2"
    );
    assert!(
        dispatch(&router, 4, get(&task_id), alice_with_different_trace)
            .await
            .is_ok()
    );

    for (label, extensions) in [
        ("Bob", request_extensions(Some("bob"))),
        ("anonymous", request_extensions(None)),
    ] {
        for (offset, request) in [get(&task_id), update(&task_id), cancel(&task_id)]
            .into_iter()
            .enumerate()
        {
            let denied = dispatch(&router, 10 + offset as i64, request, extensions.clone())
                .await
                .expect_err(label);
            assert_eq!(denied.code, -32602);
            assert!(
                denied.message.contains("not found"),
                "{label} learned that the Task exists: {}",
                denied.message
            );
        }
    }

    assert!(dispatch(&router, 20, cancel(&task_id), alice).await.is_ok());
}

#[tokio::test]
async fn invalid_or_panicking_resolvers_fail_closed() {
    async fn rejected_creation(
        resolver: impl Fn(&Extensions) -> Option<String> + Send + Sync + 'static,
    ) {
        let store = Arc::new(MemoryTaskStore::new());
        let router = base_task_router()
            .task_store(store.clone())
            .task_owner_resolver(resolver);
        let error = dispatch(
            &router,
            1,
            McpRequest::CallTool(CallToolParams {
                name: "slow".to_string(),
                arguments: serde_json::json!({}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
            request_extensions(Some("alice")),
        )
        .await
        .expect_err("a broken resolver must reject creation");
        assert_eq!(error.code, -32603);
        assert_eq!(error.message, "Task owner resolver failed");
        assert!(
            store.list_tasks(None).await.unwrap().is_empty(),
            "a failed resolver must not persist an anonymous Task"
        );
    }

    rejected_creation(|_| Some("  ".to_string())).await;
    rejected_creation(|_| panic!("resolver panic")).await;

    // Invalid resolution is not anonymous: it cannot read a deliberately
    // unowned Task already present in a shared store.
    let store: Arc<dyn TaskStore> = Arc::new(MemoryTaskStore::new());
    let (task_id, _) = store
        .create_task("slow", serde_json::Value::Null, None, None)
        .await
        .unwrap();
    let router = task_router()
        .task_store(store)
        .task_owner_resolver(|_| Some(String::new()));
    for (offset, request) in [
        get(&task_id),
        update(&task_id),
        cancel(&task_id),
        listen(&task_id),
    ]
    .into_iter()
    .enumerate()
    {
        let denied = dispatch(
            &router,
            2 + offset as i64,
            request,
            request_extensions(None),
        )
        .await
        .expect_err("invalid resolution must not match an unowned Task");
        assert_eq!(denied.code, -32602);
        assert!(denied.message.contains("not found"));
    }
}

#[cfg(feature = "oauth")]
#[tokio::test]
async fn oauth_default_persists_the_subject_without_rewriting_it() {
    use tower_mcp::oauth::TokenClaims;

    let store = Arc::new(MemoryTaskStore::new());
    let router = base_task_router().task_store(store.clone());
    let subject = "issuer.example/tenant-a::subject:alice";
    let mut extensions = request_extensions(None);
    extensions.insert(TokenClaims {
        sub: Some(subject.to_string()),
        iss: None,
        aud: None,
        exp: None,
        scope: None,
        client_id: None,
        extra: HashMap::new(),
    });

    let created = dispatch(
        &router,
        1,
        McpRequest::CallTool(CallToolParams {
            name: "slow".to_string(),
            arguments: serde_json::json!({}),
            input_responses: None,
            request_state: None,
            meta: None,
            task: None,
        }),
        extensions,
    )
    .await
    .unwrap();
    let McpResponse::FinalCreateTask(created) = created else {
        panic!("expected a Task creation response")
    };

    assert_eq!(
        store
            .task_owner(&created.task.metadata.task_id)
            .await
            .unwrap(),
        Some(Some(subject.to_string()))
    );
}
