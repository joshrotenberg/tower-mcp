//! Unit tests for [`McpRouter`](super::McpRouter) and the router-level helpers.

use super::*;
use crate::extract::{Context, Json};
use crate::jsonrpc::JsonRpcService;
use crate::tool::ToolBuilder;
use schemars::JsonSchema;
use serde::Deserialize;
use tower::ServiceExt;

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

#[cfg(feature = "stateless")]
fn final_extensions(client_capabilities: ClientCapabilities) -> Extensions {
    let mut extensions = Extensions::new();
    extensions.insert(crate::stateless::StatelessRequestMeta {
        protocol_version: Some(PROTOCOL_VERSION_2026_07_28.to_string()),
        client_capabilities: Some(client_capabilities),
        ..Default::default()
    });
    extensions
}

#[cfg(feature = "stateless")]
fn tasks_client_extensions() -> Extensions {
    final_extensions(ClientCapabilities {
        extensions: Some(
            [(TASKS_EXTENSION_ID.to_string(), serde_json::json!({}))]
                .into_iter()
                .collect(),
        ),
        ..Default::default()
    })
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn final_tasks_require_server_opt_in_and_client_declaration() {
    let tool = || {
        ToolBuilder::new("optional_task")
            .task_support(TaskSupportMode::Optional)
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build()
    };
    let task_params = |task| CallToolParams {
        name: "optional_task".to_string(),
        arguments: serde_json::json!({"a": 1, "b": 2}),
        input_responses: None,
        request_state: None,
        meta: None,
        task,
    };

    // Registering a task-capable tool is not an opt-in: a server that
    // never called `with_tasks` advertises nothing on the final path and
    // still refuses the augmentation even to a declaring client.
    let implicit = McpRouter::new().tool(tool());
    let McpResponse::Discover(result) = implicit
        .handle(
            RequestId::Number(1),
            McpRequest::Discover(DiscoverParams::default()),
            Extensions::new(),
        )
        .await
        .unwrap()
    else {
        panic!("Expected Discover response");
    };
    assert!(
        result
            .capabilities
            .extensions
            .as_ref()
            .is_none_or(|extensions| !extensions.contains_key(TASKS_EXTENSION_ID))
    );
    let error = implicit
        .handle(
            RequestId::Number(2),
            McpRequest::CallTool(task_params(Some(TaskRequestParams { ttl: None }))),
            tasks_client_extensions(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::JsonRpc(e) if e.code == -32602));

    // Opting in advertises the extension.
    let router = McpRouter::new().tool(tool()).with_tasks();
    let McpResponse::Discover(result) = router
        .handle(
            RequestId::Number(3),
            McpRequest::Discover(DiscoverParams::default()),
            Extensions::new(),
        )
        .await
        .unwrap()
    else {
        panic!("Expected Discover response");
    };
    assert!(
        result
            .capabilities
            .extensions
            .as_ref()
            .is_some_and(|extensions| extensions.contains_key(TASKS_EXTENSION_ID)),
        "with_tasks() must advertise the extension on the final path"
    );
    assert!(
        result.capabilities.tasks.is_none(),
        "the legacy capability shape is never advertised on the final path"
    );

    // A client that did not declare the extension gets the synchronous
    // form of an optional tool.
    let response = router
        .handle(
            RequestId::Number(4),
            McpRequest::CallTool(task_params(None)),
            final_extensions(ClientCapabilities::default()),
        )
        .await
        .unwrap();
    assert!(matches!(response, McpResponse::CallTool(_)));

    // Both sides declared: the server elects a task from an ordinary
    // tools/call request.
    let response = router
        .handle(
            RequestId::Number(5),
            McpRequest::CallTool(task_params(None)),
            tasks_client_extensions(),
        )
        .await
        .unwrap();
    assert!(
        matches!(response, McpResponse::FinalCreateTask(_)),
        "a negotiated request must receive a task, got {response:?}"
    );

    // The removed legacy request flag is invalid even when the extension
    // was negotiated.
    let error = router
        .handle(
            RequestId::Number(6),
            McpRequest::CallTool(task_params(Some(TaskRequestParams { ttl: None }))),
            tasks_client_extensions(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::JsonRpc(e) if e.code == -32602));
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn final_task_methods_serve_the_negotiated_wire_shapes() {
    let router = McpRouter::new()
        .tool(
            ToolBuilder::new("optional_task")
                .task_support(TaskSupportMode::Optional)
                .handler(|input: AddInput| async move {
                    Ok(CallToolResult::text(format!("{}", input.a + input.b)))
                })
                .task_preparation(|task, _input| async move {
                    let mut meta = serde_json::Map::new();
                    meta.insert(
                        "dev.tower-mcp/owner-test".to_string(),
                        serde_json::json!({"taskId": task.task_id()}),
                    );
                    Ok(crate::TaskPreparation::new().with_meta(meta))
                })
                .build(),
        )
        .with_tasks();

    let McpResponse::FinalCreateTask(created) = router
        .handle(
            RequestId::Number(1),
            McpRequest::CallTool(CallToolParams {
                name: "optional_task".to_string(),
                arguments: serde_json::json!({"a": 1, "b": 2}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
            tasks_client_extensions(),
        )
        .await
        .unwrap()
    else {
        panic!("Expected a final create-task response");
    };

    // Flat, with no legacy nested mirror.
    let wire = serde_json::to_value(&created).unwrap();
    assert_eq!(wire["resultType"], "task");
    assert!(wire.get("task").is_none(), "final results are flat: {wire}");
    assert!(wire["ttlMs"].is_number() || wire["ttlMs"].is_null());
    assert!(wire.get("ttl").is_none(), "legacy field name leaked");
    let task_id = created.task.metadata.task_id.clone();
    assert_eq!(
        created.meta.as_ref().unwrap()["dev.tower-mcp/owner-test"]["taskId"],
        task_id
    );

    // tasks/get returns a status-discriminated DetailedTask.
    let McpResponse::FinalGetTask(fetched) = router
        .handle(
            RequestId::Number(2),
            McpRequest::GetTaskInfo(GetTaskInfoParams {
                task_id: task_id.clone(),
                meta: None,
            }),
            tasks_client_extensions(),
        )
        .await
        .unwrap()
    else {
        panic!("Expected a final get-task response");
    };
    let wire = serde_json::to_value(&fetched).unwrap();
    assert_eq!(wire["resultType"], "complete");
    assert_eq!(wire["taskId"], serde_json::json!(task_id));
    assert!(wire["status"].is_string());

    // Both ack methods produce the complete acknowledgement.
    for (id, request) in [
        (
            3,
            McpRequest::UpdateTask(UpdateTaskParams {
                task_id: task_id.clone(),
                input_responses: HashMap::new(),
                meta: None,
            }),
        ),
        (
            4,
            McpRequest::CancelTask(CancelTaskParams {
                task_id: task_id.clone(),
                reason: None,
                meta: None,
            }),
        ),
    ] {
        let response = router
            .handle(RequestId::Number(id), request, tasks_client_extensions())
            .await
            .unwrap();
        let McpResponse::FinalTaskAck(ack) = response else {
            panic!("Expected a final ack for request {id}");
        };
        assert_eq!(
            serde_json::to_value(&ack).unwrap(),
            serde_json::json!({"resultType": "complete"})
        );
    }

    // An unknown task is invalid params, not a method error.
    let error = router
        .handle(
            RequestId::Number(5),
            McpRequest::GetTaskInfo(GetTaskInfoParams {
                task_id: "does-not-exist".to_string(),
                meta: None,
            }),
            tasks_client_extensions(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::JsonRpc(e) if e.code == -32602));

    // A server that advertises Tasks names the capability a client omitted.
    let error = router
        .handle(
            RequestId::Number(6),
            McpRequest::GetTaskInfo(GetTaskInfoParams {
                task_id: task_id.clone(),
                meta: None,
            }),
            final_extensions(ClientCapabilities::default()),
        )
        .await
        .unwrap_err();
    let Error::JsonRpc(error) = error else {
        panic!("expected a JSON-RPC error");
    };
    assert_eq!(error.code, -32021);
    assert_eq!(
        error.data.as_ref().unwrap()["requiredCapabilities"]["extensions"]["io.modelcontextprotocol/tasks"],
        serde_json::json!({})
    );
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn final_required_task_tools_follow_per_request_capabilities() {
    let router = McpRouter::new()
        .tool(
            ToolBuilder::new("required_task")
                .task_support(TaskSupportMode::Required)
                .handler(|input: AddInput| async move {
                    Ok(CallToolResult::text(format!("{}", input.a + input.b)))
                })
                .build(),
        )
        .with_tasks();
    let params = || CallToolParams {
        name: "required_task".to_string(),
        arguments: serde_json::json!({"a": 1, "b": 2}),
        input_responses: None,
        request_state: None,
        meta: None,
        task: None,
    };

    let McpResponse::ListTools(without_tasks) = router
        .handle(
            RequestId::Number(1),
            McpRequest::ListTools(ListToolsParams::default()),
            final_extensions(ClientCapabilities::default()),
        )
        .await
        .unwrap()
    else {
        panic!("expected tools/list")
    };
    assert!(without_tasks.tools.is_empty());

    let McpResponse::ListTools(with_tasks) = router
        .handle(
            RequestId::Number(2),
            McpRequest::ListTools(ListToolsParams::default()),
            tasks_client_extensions(),
        )
        .await
        .unwrap()
    else {
        panic!("expected tools/list")
    };
    assert_eq!(with_tasks.tools.len(), 1);
    assert!(with_tasks.tools[0].execution.is_none());

    let error = router
        .handle(
            RequestId::Number(3),
            McpRequest::CallTool(params()),
            final_extensions(ClientCapabilities::default()),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::JsonRpc(error) if error.code == -32021));

    let response = router
        .handle(
            RequestId::Number(4),
            McpRequest::CallTool(params()),
            tasks_client_extensions(),
        )
        .await
        .unwrap();
    assert!(matches!(response, McpResponse::FinalCreateTask(_)));
}

#[cfg(all(feature = "oauth", feature = "stateless"))]
#[tokio::test]
async fn task_operations_are_bound_to_the_creating_principal() {
    fn as_principal(subject: &str) -> Extensions {
        let mut extensions = tasks_client_extensions();
        extensions.insert(crate::oauth::token::TokenClaims {
            sub: Some(subject.to_string()),
            iss: None,
            aud: None,
            exp: None,
            scope: None,
            client_id: None,
            extra: HashMap::new(),
        });
        extensions
    }

    let router = McpRouter::new()
        .tool(
            ToolBuilder::new("optional_task")
                .task_support(TaskSupportMode::Optional)
                .handler(|input: AddInput| async move {
                    Ok(CallToolResult::text(format!("{}", input.a + input.b)))
                })
                .build(),
        )
        .with_tasks();

    let McpResponse::FinalCreateTask(created) = router
        .handle(
            RequestId::Number(1),
            McpRequest::CallTool(CallToolParams {
                name: "optional_task".to_string(),
                arguments: serde_json::json!({"a": 1, "b": 2}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
            as_principal("alice"),
        )
        .await
        .unwrap()
    else {
        panic!("Expected a final create-task response");
    };
    let task_id = created.task.metadata.task_id.clone();

    // The owner is served normally.
    assert!(
        router
            .handle(
                RequestId::Number(2),
                McpRequest::GetTaskInfo(GetTaskInfoParams {
                    task_id: task_id.clone(),
                    meta: None,
                }),
                as_principal("alice"),
            )
            .await
            .is_ok()
    );

    // Knowing the ID is not authority. Every operation is refused for a
    // different principal, and for one that dropped its token.
    for (id, label, context) in [
        (3, "another principal", as_principal("bob")),
        (4, "no principal", tasks_client_extensions()),
    ] {
        for (offset, request) in [
            McpRequest::GetTaskInfo(GetTaskInfoParams {
                task_id: task_id.clone(),
                meta: None,
            }),
            McpRequest::UpdateTask(UpdateTaskParams {
                task_id: task_id.clone(),
                input_responses: HashMap::new(),
                meta: None,
            }),
            McpRequest::CancelTask(CancelTaskParams {
                task_id: task_id.clone(),
                reason: None,
                meta: None,
            }),
        ]
        .into_iter()
        .enumerate()
        {
            let error = router
                .handle(
                    RequestId::Number(id * 10 + offset as i64),
                    request,
                    context.clone(),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(error, Error::JsonRpc(ref e) if e.code == -32602),
                "{label} was served: {error:?}"
            );
            // The refusal must be indistinguishable from an unknown task,
            // or it confirms the ID is real.
            let Error::JsonRpc(error) = error else {
                unreachable!()
            };
            assert!(
                error.message.contains("not found"),
                "refusal leaked that the task exists: {}",
                error.message
            );
        }
    }

    // The task survived every refused operation.
    assert!(
        router
            .handle(
                RequestId::Number(9),
                McpRequest::GetTaskInfo(GetTaskInfoParams {
                    task_id: task_id.clone(),
                    meta: None,
                }),
                as_principal("alice"),
            )
            .await
            .is_ok(),
        "a refused cancel must not have cancelled the task"
    );
}

#[cfg(all(feature = "oauth", feature = "stateless"))]
#[tokio::test]
async fn final_tasks_work_across_independent_routers_with_a_shared_store() {
    fn as_principal(subject: &str) -> Extensions {
        let mut extensions = tasks_client_extensions();
        extensions.insert(crate::oauth::token::TokenClaims {
            sub: Some(subject.to_string()),
            iss: None,
            aud: None,
            exp: None,
            scope: None,
            client_id: None,
            extra: HashMap::new(),
        });
        extensions
    }

    fn router_with_store(store: Arc<dyn TaskStore>) -> McpRouter {
        McpRouter::new()
            .tool(
                ToolBuilder::new("shared_task")
                    .task_support(TaskSupportMode::Optional)
                    .handler(|_input: serde_json::Value| async move {
                        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                        Ok(CallToolResult::text("done"))
                    })
                    .build(),
            )
            .task_store(store)
            .with_tasks()
    }

    let store: Arc<dyn TaskStore> = Arc::new(MemoryTaskStore::new());
    let router_a = router_with_store(store.clone());
    let router_b = router_with_store(store);

    let McpResponse::FinalCreateTask(created) = router_a
        .handle(
            RequestId::Number(1),
            McpRequest::CallTool(CallToolParams {
                name: "shared_task".to_string(),
                arguments: serde_json::json!({}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
            as_principal("alice"),
        )
        .await
        .unwrap()
    else {
        panic!("router A did not create a final task")
    };
    let task_id = created.task.metadata.task_id;

    // A separate router instance can read the shared task for its owner.
    assert!(
        router_b
            .handle(
                RequestId::Number(2),
                McpRequest::GetTaskInfo(GetTaskInfoParams {
                    task_id: task_id.clone(),
                    meta: None,
                }),
                as_principal("alice"),
            )
            .await
            .is_ok()
    );

    // Another principal sees the same response as an unknown ID.
    let denied = router_b
        .handle(
            RequestId::Number(3),
            McpRequest::GetTaskInfo(GetTaskInfoParams {
                task_id: task_id.clone(),
                meta: None,
            }),
            as_principal("bob"),
        )
        .await
        .unwrap_err();
    let unknown = router_b
        .handle(
            RequestId::Number(4),
            McpRequest::GetTaskInfo(GetTaskInfoParams {
                task_id: "unknown-task".to_string(),
                meta: None,
            }),
            as_principal("bob"),
        )
        .await
        .unwrap_err();
    let (Error::JsonRpc(denied), Error::JsonRpc(unknown)) = (denied, unknown) else {
        panic!("expected JSON-RPC task denials")
    };
    assert_eq!(denied.code, unknown.code);
    assert_eq!(
        denied.message.replace(&task_id, "<task-id>"),
        unknown.message.replace("unknown-task", "<task-id>")
    );
    assert_eq!(denied.data, unknown.data);

    // Router B mutates the shared task, and router A immediately observes
    // the terminal state through the same backend.
    assert!(matches!(
        router_b
            .handle(
                RequestId::Number(5),
                McpRequest::CancelTask(CancelTaskParams {
                    task_id: task_id.clone(),
                    reason: None,
                    meta: None,
                }),
                as_principal("alice"),
            )
            .await
            .unwrap(),
        McpResponse::FinalTaskAck(_)
    ));
    let McpResponse::FinalGetTask(fetched) = router_a
        .handle(
            RequestId::Number(6),
            McpRequest::GetTaskInfo(GetTaskInfoParams {
                task_id,
                meta: None,
            }),
            as_principal("alice"),
        )
        .await
        .unwrap()
    else {
        panic!("router A did not read the shared task")
    };
    assert_eq!(fetched.task.status(), TaskStatus::Cancelled);
}

#[test]
fn router_advertises_only_locally_declared_protocol_extensions() {
    let router = McpRouter::new().with_protocol_extension(
        crate::ExtensionDeclaration::new(
            "com.example/rendering",
            serde_json::json!({"formats": ["html"]}),
        )
        .unwrap(),
    );

    let stable = router.capabilities();
    let final_capabilities =
        router.capabilities_for_protocol(Some(crate::protocol::PROTOCOL_VERSION_2026_07_28));
    for capabilities in [stable, final_capabilities] {
        let extensions = capabilities.extensions.unwrap();
        assert_eq!(extensions.len(), 1);
        assert_eq!(extensions["com.example/rendering"]["formats"][0], "html");
        assert!(!extensions.contains_key("com.example/client-only"));
    }
}

#[tokio::test]
async fn initialize_persists_negotiated_extensions_for_legacy_contexts() {
    let router = McpRouter::new().with_protocol_extension(
        crate::ExtensionDeclaration::new("com.example/shared", serde_json::json!({"server": true}))
            .unwrap(),
    );
    let client_capabilities = ClientCapabilities {
        extensions: Some(HashMap::from([
            (
                "com.example/shared".to_string(),
                serde_json::json!({"client": true}),
            ),
            ("com.example/client-only".to_string(), serde_json::json!({})),
        ])),
        ..ClientCapabilities::default()
    };

    router
        .handle(
            RequestId::Number(1),
            McpRequest::Initialize(InitializeParams {
                protocol_version: crate::protocol::LATEST_PROTOCOL_VERSION.to_string(),
                capabilities: client_capabilities,
                client_info: Implementation {
                    name: "extension-test".to_string(),
                    version: "1.0.0".to_string(),
                    title: None,
                    description: None,
                    icons: None,
                    website_url: None,
                    meta: None,
                },
                meta: None,
            }),
            Extensions::new(),
        )
        .await
        .unwrap();

    let context = router.create_context(RequestId::Number(2), None);
    let negotiated = context.negotiated_extensions().unwrap();
    assert!(negotiated.contains("com.example/shared"));
    assert!(!negotiated.contains("com.example/client-only"));
}

#[cfg(feature = "stateless")]
#[test]
fn final_request_context_exposes_only_negotiated_extensions() {
    let router = McpRouter::new().with_protocol_extension(
        crate::ExtensionDeclaration::new("com.example/shared", serde_json::json!({"server": true}))
            .unwrap(),
    );
    let per_request = final_extensions(ClientCapabilities {
        extensions: Some(HashMap::from([
            (
                "com.example/shared".to_string(),
                serde_json::json!({"client": true}),
            ),
            ("com.example/client-only".to_string(), serde_json::json!({})),
        ])),
        ..ClientCapabilities::default()
    });

    let context = router.create_context_with_extensions(RequestId::Number(1), None, &per_request);
    let negotiated = context.negotiated_extensions().unwrap();

    assert_eq!(negotiated.len(), 1);
    assert_eq!(
        negotiated
            .get("com.example/shared")
            .unwrap()
            .client_settings()["client"],
        true
    );
    assert!(!negotiated.contains("com.example/client-only"));
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn final_protocol_withholds_incomplete_tasks_advertisement() {
    let optional = ToolBuilder::new("optional_task")
        .task_support(TaskSupportMode::Optional)
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();
    let required = ToolBuilder::new("required_task")
        .task_support(TaskSupportMode::Required)
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();
    let mut router = McpRouter::new().tool(optional).tool(required);

    // Stable clients retain the existing capability surface.
    let stable_capabilities = router.capabilities();
    assert!(stable_capabilities.tasks.is_some());
    assert!(
        stable_capabilities
            .extensions
            .as_ref()
            .is_some_and(|extensions| extensions.contains_key(TASKS_EXTENSION_ID))
    );

    // Final discovery must not claim support for the incomplete extension.
    let response = router
        .handle(
            RequestId::Number(1),
            McpRequest::Discover(DiscoverParams::default()),
            Extensions::new(),
        )
        .await
        .unwrap();
    let McpResponse::Discover(result) = response else {
        panic!("Expected Discover response");
    };
    assert!(result.capabilities.tasks.is_none());
    assert!(
        result
            .capabilities
            .extensions
            .as_ref()
            .is_none_or(|extensions| !extensions.contains_key(TASKS_EXTENSION_ID))
    );

    init_router(&mut router).await;

    // Stable discovery keeps both tools and their execution metadata.
    let response = router
        .handle(
            RequestId::Number(2),
            McpRequest::ListTools(ListToolsParams::default()),
            Extensions::new(),
        )
        .await
        .unwrap();
    let McpResponse::ListTools(result) = response else {
        panic!("Expected ListTools response");
    };
    assert_eq!(result.tools.len(), 2);
    assert!(result.tools.iter().all(|tool| tool.execution.is_some()));

    // Final discovery keeps the synchronously callable optional tool, but
    // strips Tasks metadata and hides the required-task-only tool.
    let response = router
        .handle(
            RequestId::Number(3),
            McpRequest::ListTools(ListToolsParams::default()),
            final_extensions(ClientCapabilities::default()),
        )
        .await
        .unwrap();
    let McpResponse::ListTools(result) = response else {
        panic!("Expected ListTools response");
    };
    assert_eq!(result.tools.len(), 1);
    assert_eq!(result.tools[0].name, "optional_task");
    assert!(result.tools[0].execution.is_none());
}

#[cfg(feature = "stateless")]
#[tokio::test]
async fn final_protocol_enforces_tasks_negotiation() {
    let optional = ToolBuilder::new("optional_task")
        .task_support(TaskSupportMode::Optional)
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();
    let required = ToolBuilder::new("required_task")
        .task_support(TaskSupportMode::Required)
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();
    let mut router = McpRouter::new().tool(optional).tool(required).with_tasks();
    init_router(&mut router).await;

    // The optional tool remains synchronously callable on the final path.
    let response = router
        .handle(
            RequestId::Number(1),
            McpRequest::CallTool(CallToolParams {
                name: "optional_task".to_string(),
                arguments: serde_json::json!({"a": 1, "b": 2}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
            final_extensions(ClientCapabilities::default()),
        )
        .await
        .unwrap();
    assert!(matches!(response, McpResponse::CallTool(_)));

    // The removed legacy task augmentation is invalid on the final wire.
    let error = router
        .handle(
            RequestId::Number(2),
            McpRequest::CallTool(CallToolParams {
                name: "optional_task".to_string(),
                arguments: serde_json::json!({"a": 1, "b": 2}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: Some(TaskRequestParams { ttl: None }),
            }),
            final_extensions(ClientCapabilities::default()),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::JsonRpc(error) if error.code == -32602));

    // A required-task tool cannot run without a task, so the server names
    // the capability the client is missing rather than pretending the tool
    // does not exist.
    let error = router
        .handle(
            RequestId::Number(3),
            McpRequest::CallTool(CallToolParams {
                name: "required_task".to_string(),
                arguments: serde_json::json!({"a": 1, "b": 2}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
            final_extensions(ClientCapabilities::default()),
        )
        .await
        .unwrap_err();
    let Error::JsonRpc(error) = error else {
        panic!("expected a JSON-RPC error");
    };
    assert_eq!(error.code, -32021);
    assert_eq!(
        error.data.as_ref().unwrap()["requiredCapabilities"]["extensions"]["io.modelcontextprotocol/tasks"],
        serde_json::json!({}),
        "the error must name the extension the client needs to declare"
    );

    let task_requests = [
        McpRequest::GetTaskInfo(GetTaskInfoParams {
            task_id: "task-unknown".to_string(),
            meta: None,
        }),
        McpRequest::UpdateTask(UpdateTaskParams {
            task_id: "task-unknown".to_string(),
            input_responses: HashMap::new(),
            meta: None,
        }),
        McpRequest::CancelTask(CancelTaskParams {
            task_id: "task-unknown".to_string(),
            reason: None,
            meta: None,
        }),
    ];
    for (index, request) in task_requests.into_iter().enumerate() {
        let error = router
            .handle(
                RequestId::Number(4 + index as i64),
                request,
                final_extensions(ClientCapabilities::default()),
            )
            .await
            .unwrap_err();
        let Error::JsonRpc(error) = error else {
            panic!("expected a JSON-RPC error");
        };
        assert_eq!(error.code, -32021);
        assert_eq!(
            error.data.as_ref().unwrap()["requiredCapabilities"]["extensions"]["io.modelcontextprotocol/tasks"],
            serde_json::json!({})
        );
    }

    // If the server itself did not advertise the extension, the method is
    // unavailable regardless of what the client declared.
    let router_without_tasks = McpRouter::new();
    let error = router_without_tasks
        .handle(
            RequestId::Number(7),
            McpRequest::GetTaskInfo(GetTaskInfoParams {
                task_id: "task-unknown".to_string(),
                meta: None,
            }),
            final_extensions(tasks_client_capabilities()),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::JsonRpc(error) if error.code == -32601));
}

#[cfg(feature = "stateless")]
#[test]
fn input_required_capability_validation_uses_capability_semantics() {
    let roots = InputRequiredResult::with_requests(
        [(
            "roots".to_string(),
            InputRequest::ListRoots(ListRootsParams::default()),
        )]
        .into_iter()
        .collect(),
    );
    let extensions = final_extensions(ClientCapabilities {
        roots: Some(RootsCapability {
            list_changed: true,
            deprecated: None,
        }),
        ..Default::default()
    });
    validate_input_required_result(&extensions, &roots).unwrap();
    assert!(client_capabilities_satisfy(
        extensions
            .get::<crate::stateless::StatelessRequestMeta>()
            .and_then(|meta| meta.client_capabilities.as_ref())
            .unwrap(),
        &ClientCapabilities {
            roots: Some(RootsCapability::default()),
            ..Default::default()
        }
    ));

    let sampling_with_tools = InputRequiredResult::with_requests(
        [(
            "sample".to_string(),
            InputRequest::CreateMessage(CreateMessageParams {
                tools: Some(Vec::new()),
                ..CreateMessageParams::new(vec![SamplingMessage::user("hello")], 10)
            }),
        )]
        .into_iter()
        .collect(),
    );
    let extensions = final_extensions(ClientCapabilities {
        sampling: Some(SamplingCapability::default()),
        ..Default::default()
    });
    assert!(validate_input_required_result(&extensions, &sampling_with_tools).is_err());

    let form = InputRequiredResult::with_requests(
        [(
            "form".to_string(),
            InputRequest::Elicit(ElicitRequestParams::Form(ElicitFormParams {
                mode: Some(ElicitMode::Form),
                message: "name".into(),
                requested_schema: ElicitFormSchema::new(),
                meta: None,
            })),
        )]
        .into_iter()
        .collect(),
    );
    let extensions = final_extensions(ClientCapabilities {
        elicitation: Some(ElicitationCapability::default()),
        ..Default::default()
    });
    validate_input_required_result(&extensions, &form).unwrap();
}

/// Helper to initialize a router for testing
async fn init_router(router: &mut McpRouter) {
    // Send initialize request
    let init_req = RouterRequest {
        id: RequestId::Number(0),
        inner: McpRequest::Initialize(InitializeParams {
            protocol_version: "2025-11-25".to_string(),
            capabilities: ClientCapabilities {
                roots: None,
                sampling: None,
                elicitation: None,
                tasks: None,
                experimental: None,
                extensions: None,
            },
            client_info: Implementation {
                name: "test".to_string(),
                version: "1.0".to_string(),
                ..Default::default()
            },
            meta: None,
        }),
        extensions: Extensions::new(),
    };
    let _ = router.ready().await.unwrap().call(init_req).await.unwrap();
    // Send initialized notification
    router.handle_notification(McpNotification::Initialized);
}

#[tokio::test]
async fn test_router_list_tools() {
    let add_tool = ToolBuilder::new("add")
        .description("Add two numbers")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    let mut router = McpRouter::new().tool(add_tool);

    // Initialize session first
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ListTools(ListToolsParams::default()),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::ListTools(result)) => {
            assert_eq!(result.tools.len(), 1);
            assert_eq!(result.tools[0].name, "add");
        }
        _ => panic!("Expected ListTools response"),
    }
}

#[tokio::test]
async fn test_router_call_tool() {
    let add_tool = ToolBuilder::new("add")
        .description("Add two numbers")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    let mut router = McpRouter::new().tool(add_tool);

    // Initialize session first
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::CallTool(CallToolParams {
            input_responses: None,
            request_state: None,
            name: "add".to_string(),
            arguments: serde_json::json!({"a": 2, "b": 3}),
            meta: None,
            task: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::CallTool(result)) => {
            assert!(!result.is_error);
            // Check the text content
            match &result.content[0] {
                Content::Text { text, .. } => assert_eq!(text, "5"),
                _ => panic!("Expected text content"),
            }
        }
        _ => panic!("Expected CallTool response"),
    }
}

/// Helper to initialize a JsonRpcService for testing
async fn init_jsonrpc_service(service: &mut JsonRpcService<McpRouter>, router: &McpRouter) {
    let init_req = JsonRpcRequest::new(0, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-11-25",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    let _ = service.call_single(init_req).await.unwrap();
    router.handle_notification(McpNotification::Initialized);
}

#[tokio::test]
async fn test_jsonrpc_service() {
    let add_tool = ToolBuilder::new("add")
        .description("Add two numbers")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    let router = McpRouter::new().tool(add_tool);
    let mut service = JsonRpcService::new(router.clone());

    // Initialize session first
    init_jsonrpc_service(&mut service, &router).await;

    let req = JsonRpcRequest::new(1, "tools/list");

    let resp = service.call_single(req).await.unwrap();

    match resp {
        JsonRpcResponse::Result(r) => {
            assert_eq!(r.id, RequestId::Number(1));
            let tools = r.result.get("tools").unwrap().as_array().unwrap();
            assert_eq!(tools.len(), 1);
        }
        JsonRpcResponse::Error(_) => panic!("Expected success response"),
        _ => panic!("unexpected response variant"),
    }
}

#[tokio::test]
async fn test_batch_request() {
    let add_tool = ToolBuilder::new("add")
        .description("Add two numbers")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    let router = McpRouter::new().tool(add_tool);
    let mut service = JsonRpcService::new(router.clone())
        .protocol_versions(["2025-03-26"])
        .unwrap();

    // Initialize session first
    init_jsonrpc_service(&mut service, &router).await;

    // Create a batch of requests
    let requests = vec![
        JsonRpcRequest::new(1, "tools/list"),
        JsonRpcRequest::new(2, "tools/call").with_params(serde_json::json!({
            "name": "add",
            "arguments": {"a": 10, "b": 20}
        })),
        JsonRpcRequest::new(3, "ping"),
    ];

    let responses = service.call_batch(requests).await.unwrap();

    assert_eq!(responses.len(), 3);

    // Check first response (tools/list)
    match &responses[0] {
        JsonRpcResponse::Result(r) => {
            assert_eq!(r.id, RequestId::Number(1));
            let tools = r.result.get("tools").unwrap().as_array().unwrap();
            assert_eq!(tools.len(), 1);
        }
        JsonRpcResponse::Error(_) => panic!("Expected success for tools/list"),
        _ => panic!("unexpected response variant"),
    }

    // Check second response (tools/call)
    match &responses[1] {
        JsonRpcResponse::Result(r) => {
            assert_eq!(r.id, RequestId::Number(2));
            let content = r.result.get("content").unwrap().as_array().unwrap();
            let text = content[0].get("text").unwrap().as_str().unwrap();
            assert_eq!(text, "30");
        }
        JsonRpcResponse::Error(_) => panic!("Expected success for tools/call"),
        _ => panic!("unexpected response variant"),
    }

    // Check third response (ping)
    match &responses[2] {
        JsonRpcResponse::Result(r) => {
            assert_eq!(r.id, RequestId::Number(3));
        }
        JsonRpcResponse::Error(_) => panic!("Expected success for ping"),
        _ => panic!("unexpected response variant"),
    }
}

#[tokio::test]
async fn test_empty_batch_error() {
    let router = McpRouter::new();
    let mut service = JsonRpcService::new(router);

    let result = service.call_batch(vec![]).await;
    assert!(result.is_err());
}

// =========================================================================
// Progress Token Tests
// =========================================================================

#[tokio::test]
async fn test_progress_token_extraction() {
    use crate::context::{ServerNotification, notification_channel};
    use crate::protocol::ProgressToken;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    // Track whether progress was reported
    let progress_reported = Arc::new(AtomicBool::new(false));
    let progress_ref = progress_reported.clone();

    // Create a tool that reports progress
    let tool = ToolBuilder::new("progress_tool")
        .description("Tool that reports progress")
        .extractor_handler((), move |ctx: Context, Json(_input): Json<AddInput>| {
            let reported = progress_ref.clone();
            async move {
                // Report progress - this should work if token was extracted
                ctx.report_progress(50.0, Some(100.0), Some("Halfway"))
                    .await;
                reported.store(true, Ordering::SeqCst);
                Ok(CallToolResult::text("done"))
            }
        })
        .build();

    // Set up notification channel
    let (tx, mut rx) = notification_channel(10);
    let router = McpRouter::new().with_notification_sender(tx).tool(tool);
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    init_jsonrpc_service(&mut service, &router).await;

    // Call tool WITH progress token in _meta
    let req = JsonRpcRequest::new(1, "tools/call").with_params(serde_json::json!({
        "name": "progress_tool",
        "arguments": {"a": 1, "b": 2},
        "_meta": {
            "progressToken": "test-token-123"
        }
    }));

    let resp = service.call_single(req).await.unwrap();

    // Verify the tool was called successfully
    match resp {
        JsonRpcResponse::Result(_) => {}
        JsonRpcResponse::Error(e) => panic!("Expected success, got error: {:?}", e),
        _ => panic!("unexpected response variant"),
    }

    // Verify progress was reported by handler
    assert!(progress_reported.load(Ordering::SeqCst));

    // Verify progress notification was sent through channel
    let notification = rx.try_recv().expect("Expected progress notification");
    match notification {
        ServerNotification::Progress(params) => {
            assert_eq!(
                params.progress_token,
                ProgressToken::String("test-token-123".to_string())
            );
            assert_eq!(params.progress, 50.0);
            assert_eq!(params.total, Some(100.0));
            assert_eq!(params.message.as_deref(), Some("Halfway"));
        }
        _ => panic!("Expected Progress notification"),
    }
}

#[tokio::test]
async fn test_tool_call_without_progress_token() {
    use crate::context::notification_channel;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let progress_attempted = Arc::new(AtomicBool::new(false));
    let progress_ref = progress_attempted.clone();

    let tool = ToolBuilder::new("no_token_tool")
        .description("Tool that tries to report progress without token")
        .extractor_handler((), move |ctx: Context, Json(_input): Json<AddInput>| {
            let attempted = progress_ref.clone();
            async move {
                // Try to report progress - should be a no-op without token
                ctx.report_progress(50.0, Some(100.0), None).await;
                attempted.store(true, Ordering::SeqCst);
                Ok(CallToolResult::text("done"))
            }
        })
        .build();

    let (tx, mut rx) = notification_channel(10);
    let router = McpRouter::new().with_notification_sender(tx).tool(tool);
    let mut service = JsonRpcService::new(router.clone());

    init_jsonrpc_service(&mut service, &router).await;

    // Call tool WITHOUT progress token
    let req = JsonRpcRequest::new(1, "tools/call").with_params(serde_json::json!({
        "name": "no_token_tool",
        "arguments": {"a": 1, "b": 2}
    }));

    let resp = service.call_single(req).await.unwrap();
    assert!(matches!(resp, JsonRpcResponse::Result(_)));

    // Handler was called
    assert!(progress_attempted.load(Ordering::SeqCst));

    // But no notification was sent (no progress token)
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn test_batch_errors_returned_not_dropped() {
    let add_tool = ToolBuilder::new("add")
        .description("Add two numbers")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    let router = McpRouter::new().tool(add_tool);
    let mut service = JsonRpcService::new(router.clone())
        .protocol_versions(["2025-03-26"])
        .unwrap();

    init_jsonrpc_service(&mut service, &router).await;

    // Create a batch with one valid and one invalid request
    let requests = vec![
        // Valid request
        JsonRpcRequest::new(1, "tools/call").with_params(serde_json::json!({
            "name": "add",
            "arguments": {"a": 10, "b": 20}
        })),
        // Invalid request - tool doesn't exist
        JsonRpcRequest::new(2, "tools/call").with_params(serde_json::json!({
            "name": "nonexistent_tool",
            "arguments": {}
        })),
        // Another valid request
        JsonRpcRequest::new(3, "ping"),
    ];

    let responses = service.call_batch(requests).await.unwrap();

    // All three requests should have responses (errors are not dropped)
    assert_eq!(responses.len(), 3);

    // First should be success
    match &responses[0] {
        JsonRpcResponse::Result(r) => {
            assert_eq!(r.id, RequestId::Number(1));
        }
        JsonRpcResponse::Error(_) => panic!("Expected success for first request"),
        _ => panic!("unexpected response variant"),
    }

    // Second should be an error (tool not found)
    match &responses[1] {
        JsonRpcResponse::Error(e) => {
            assert_eq!(e.id, Some(RequestId::Number(2)));
            // Error should indicate method not found
            assert!(e.error.message.contains("not found") || e.error.code == -32601);
        }
        JsonRpcResponse::Result(_) => panic!("Expected error for second request"),
        _ => panic!("unexpected response variant"),
    }

    // Third should be success
    match &responses[2] {
        JsonRpcResponse::Result(r) => {
            assert_eq!(r.id, RequestId::Number(3));
        }
        JsonRpcResponse::Error(_) => panic!("Expected success for third request"),
        _ => panic!("unexpected response variant"),
    }
}

// =========================================================================
// Resource Template Tests
// =========================================================================

#[tokio::test]
async fn test_list_resource_templates() {
    use crate::resource::ResourceTemplateBuilder;
    use std::collections::HashMap;

    let template = ResourceTemplateBuilder::new("file:///{path}")
        .name("Project Files")
        .description("Access project files")
        .handler(|uri: String, _vars: HashMap<String, String>| async move {
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri,
                    mime_type: None,
                    text: None,
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        });

    let mut router = McpRouter::new().resource_template(template);

    // Initialize session
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ListResourceTemplates(ListResourceTemplatesParams::default()),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::ListResourceTemplates(result)) => {
            assert_eq!(result.resource_templates.len(), 1);
            assert_eq!(result.resource_templates[0].uri_template, "file:///{path}");
            assert_eq!(result.resource_templates[0].name, "Project Files");
        }
        _ => panic!("Expected ListResourceTemplates response"),
    }
}

#[tokio::test]
async fn test_read_resource_via_template() {
    use crate::resource::ResourceTemplateBuilder;
    use std::collections::HashMap;

    let template = ResourceTemplateBuilder::new("db://users/{id}")
        .name("User Records")
        .handler(|uri: String, vars: HashMap<String, String>| async move {
            let id = vars.get("id").unwrap().clone();
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri,
                    mime_type: Some("application/json".to_string()),
                    text: Some(format!(r#"{{"id": "{}"}}"#, id)),
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        });

    let mut router = McpRouter::new().resource_template(template);

    // Initialize session
    init_router(&mut router).await;

    // Read a resource that matches the template
    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ReadResource(ReadResourceParams {
            input_responses: None,
            request_state: None,
            uri: "db://users/123".to_string(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::ReadResource(result)) => {
            assert_eq!(result.contents.len(), 1);
            assert_eq!(result.contents[0].uri, "db://users/123");
            assert!(result.contents[0].text.as_ref().unwrap().contains("123"));
        }
        _ => panic!("Expected ReadResource response"),
    }
}

#[tokio::test]
async fn test_static_resource_takes_precedence_over_template() {
    use crate::resource::{ResourceBuilder, ResourceTemplateBuilder};
    use std::collections::HashMap;

    // Template that would match the same URI
    let template = ResourceTemplateBuilder::new("file:///{path}")
        .name("Files Template")
        .handler(|uri: String, _vars: HashMap<String, String>| async move {
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri,
                    mime_type: None,
                    text: Some("from template".to_string()),
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        });

    // Static resource with exact URI
    let static_resource = ResourceBuilder::new("file:///README.md")
        .name("README")
        .text("from static resource");

    let mut router = McpRouter::new()
        .resource_template(template)
        .resource(static_resource);

    // Initialize session
    init_router(&mut router).await;

    // Read the static resource - should NOT go through template
    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ReadResource(ReadResourceParams {
            input_responses: None,
            request_state: None,
            uri: "file:///README.md".to_string(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::ReadResource(result)) => {
            // Should get static resource, not template
            assert_eq!(
                result.contents[0].text.as_deref(),
                Some("from static resource")
            );
        }
        _ => panic!("Expected ReadResource response"),
    }
}

#[tokio::test]
async fn test_resource_not_found_when_no_match() {
    use crate::resource::ResourceTemplateBuilder;
    use std::collections::HashMap;

    let template = ResourceTemplateBuilder::new("db://users/{id}")
        .name("Users")
        .handler(|uri: String, _vars: HashMap<String, String>| async move {
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri,
                    mime_type: None,
                    text: None,
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        });

    let mut router = McpRouter::new().resource_template(template);

    // Initialize session
    init_router(&mut router).await;

    // Try to read a URI that doesn't match any resource or template
    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ReadResource(ReadResourceParams {
            input_responses: None,
            request_state: None,
            uri: "db://posts/123".to_string(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Err(err) => {
            assert!(err.message.contains("not found"));
        }
        Ok(_) => panic!("Expected error for non-matching URI"),
    }
}

#[tokio::test]
async fn test_capabilities_include_resources_with_only_templates() {
    use crate::resource::ResourceTemplateBuilder;
    use std::collections::HashMap;

    let template = ResourceTemplateBuilder::new("file:///{path}")
        .name("Files")
        .handler(|uri: String, _vars: HashMap<String, String>| async move {
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri,
                    mime_type: None,
                    text: None,
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        });

    let mut router = McpRouter::new().resource_template(template);

    // Send initialize request and check capabilities
    let init_req = RouterRequest {
        id: RequestId::Number(0),
        inner: McpRequest::Initialize(InitializeParams {
            protocol_version: "2025-11-25".to_string(),
            capabilities: ClientCapabilities {
                roots: None,
                sampling: None,
                elicitation: None,
                tasks: None,
                experimental: None,
                extensions: None,
            },
            client_info: Implementation {
                name: "test".to_string(),
                version: "1.0".to_string(),
                ..Default::default()
            },
            meta: None,
        }),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(init_req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::Initialize(result)) => {
            // Should have resources capability even though only templates registered
            assert!(result.capabilities.resources.is_some());
        }
        _ => panic!("Expected Initialize response"),
    }
}

// =========================================================================
// Logging Notification Tests
// =========================================================================

#[tokio::test]
async fn test_log_sends_notification() {
    use crate::context::notification_channel;

    let (tx, mut rx) = notification_channel(10);
    let router = McpRouter::new().with_notification_sender(tx);

    // Send an info log
    let sent = router.log_info("Test message");
    assert!(sent);

    // Should receive the notification
    let notification = rx.try_recv().unwrap();
    match notification {
        ServerNotification::LogMessage(params) => {
            assert_eq!(params.level, LogLevel::Info);
            let data = params.data;
            assert_eq!(
                data.get("message").unwrap().as_str().unwrap(),
                "Test message"
            );
        }
        _ => panic!("Expected LogMessage notification"),
    }
}

#[tokio::test]
async fn test_log_with_custom_params() {
    use crate::context::notification_channel;

    let (tx, mut rx) = notification_channel(10);
    let router = McpRouter::new().with_notification_sender(tx);

    // Send a custom log message
    let params = LoggingMessageParams::new(
        LogLevel::Error,
        serde_json::json!({
            "error": "Connection failed",
            "host": "localhost"
        }),
    )
    .with_logger("database");

    let sent = router.log(params);
    assert!(sent);

    let notification = rx.try_recv().unwrap();
    match notification {
        ServerNotification::LogMessage(params) => {
            assert_eq!(params.level, LogLevel::Error);
            assert_eq!(params.logger.as_deref(), Some("database"));
            let data = params.data;
            assert_eq!(
                data.get("error").unwrap().as_str().unwrap(),
                "Connection failed"
            );
        }
        _ => panic!("Expected LogMessage notification"),
    }
}

#[tokio::test]
async fn test_log_without_channel_returns_false() {
    // Router without notification channel
    let router = McpRouter::new();

    // Should return false when no channel configured
    assert!(!router.log_info("Test"));
    assert!(!router.log_warning("Test"));
    assert!(!router.log_error("Test"));
    assert!(!router.log_debug("Test"));
}

#[tokio::test]
async fn test_logging_capability_with_channel() {
    use crate::context::notification_channel;

    let (tx, _rx) = notification_channel(10);
    let mut router = McpRouter::new().with_notification_sender(tx);

    // Initialize and check capabilities
    let init_req = RouterRequest {
        id: RequestId::Number(0),
        inner: McpRequest::Initialize(InitializeParams {
            protocol_version: "2025-11-25".to_string(),
            capabilities: ClientCapabilities {
                roots: None,
                sampling: None,
                elicitation: None,
                tasks: None,
                experimental: None,
                extensions: None,
            },
            client_info: Implementation {
                name: "test".to_string(),
                version: "1.0".to_string(),
                ..Default::default()
            },
            meta: None,
        }),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(init_req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::Initialize(result)) => {
            // Should have logging capability when notification channel is set
            assert!(result.capabilities.logging.is_some());
        }
        _ => panic!("Expected Initialize response"),
    }
}

#[tokio::test]
async fn test_no_logging_capability_without_channel() {
    let mut router = McpRouter::new();

    // Initialize and check capabilities
    let init_req = RouterRequest {
        id: RequestId::Number(0),
        inner: McpRequest::Initialize(InitializeParams {
            protocol_version: "2025-11-25".to_string(),
            capabilities: ClientCapabilities {
                roots: None,
                sampling: None,
                elicitation: None,
                tasks: None,
                experimental: None,
                extensions: None,
            },
            client_info: Implementation {
                name: "test".to_string(),
                version: "1.0".to_string(),
                ..Default::default()
            },
            meta: None,
        }),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(init_req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::Initialize(result)) => {
            // Should NOT have logging capability without notification channel
            assert!(result.capabilities.logging.is_none());
        }
        _ => panic!("Expected Initialize response"),
    }
}

// =========================================================================
// Task Lifecycle Tests
// =========================================================================

/// #1230: a panic payload is `&'static str` for a literal and `String`
/// for a formatted message, and both have to survive the trip through
/// `Box<dyn Any>` or the error result says nothing useful.
#[test]
fn panic_message_recovers_both_payload_shapes() {
    let literal = std::panic::catch_unwind(|| panic!("boom literal")).unwrap_err();
    assert_eq!(panic_message(&*literal), "boom literal");

    let n = 7;
    let formatted = std::panic::catch_unwind(|| panic!("boom {n}")).unwrap_err();
    assert_eq!(panic_message(&*formatted), "boom 7");

    let odd = std::panic::catch_unwind(|| std::panic::panic_any(42u8)).unwrap_err();
    assert_eq!(panic_message(&*odd), "panicked with a non-string payload");
}

#[test]
fn panic_policy_debug_does_not_disclose_its_fixed_client_message() {
    let debug = format!(
        "{:?}",
        PanicPolicy::redacted("private incident text")
            .include_tool_name_in_client_message(true)
            .include_tool_name_in_logs(true)
            .include_payload_in_logs(true)
    );
    assert!(debug.contains("client_message: \"fixed\""), "{debug}");
    assert!(debug.contains("client_tool_name: \"original\""), "{debug}");
    assert!(debug.contains("log_tool_name: \"original\""), "{debug}");
    assert!(!debug.contains("private incident text"), "{debug}");
}

/// #1208: the full task input round trip. The handler asks, the task
/// parks in `input_required` carrying the requests, the client answers
/// with `tasks/update`, the router re-invokes the handler with the
/// answers, and the task completes.
#[cfg(feature = "stateless")]
#[tokio::test]
async fn a_task_resumes_after_its_input_is_answered() {
    use crate::async_task::{MemoryTaskStore, TaskStore};
    use crate::protocol::{
        ElicitFormParams, ElicitFormSchema, ElicitRequestParams, InputRequest, InputRequests,
        InputRequiredResult, RequestOutcome,
    };

    let asks = ToolBuilder::new("asks")
        .description("Needs a decision")
        .task_support(TaskSupportMode::Optional)
        .mrtr_handler::<serde_json::Value, _, _>(|ctx, _input| async move {
            // Resume leg: the answers are readable exactly as a non-task
            // MRTR handler sees them on the client's retry.
            if let Some(responses) = ctx.input_responses()
                && responses.contains_key("decision")
            {
                return Ok(RequestOutcome::Complete(CallToolResult::text("approved")));
            }
            let mut requests: InputRequests = Default::default();
            requests.insert(
                "decision".to_string(),
                InputRequest::Elicit(ElicitRequestParams::Form(ElicitFormParams {
                    mode: None,
                    message: "approve?".to_string(),
                    requested_schema: ElicitFormSchema::new(),
                    meta: None,
                })),
            );
            Ok(RequestOutcome::input_required(
                InputRequiredResult::with_requests(requests),
            ))
        })
        .build();

    let store = std::sync::Arc::new(MemoryTaskStore::new());
    let mut router = McpRouter::new()
        .task_store(store.clone())
        .tool(asks)
        .with_tasks();
    init_router(&mut router).await;

    let resp = router
        .ready()
        .await
        .unwrap()
        .call(RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                input_responses: None,
                request_state: None,
                name: "asks".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
            extensions: tasks_client_extensions(),
        })
        .await
        .unwrap();
    let task_id = match resp.inner {
        Ok(McpResponse::FinalCreateTask(result)) => result.task.metadata.task_id,
        other => panic!("expected a created task, got {other:?}"),
    };

    // The task parks rather than failing.
    let mut parked = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let task = store.get_task(&task_id).await.unwrap().unwrap();
        if task.status == TaskStatus::InputRequired {
            parked = true;
            break;
        }
        assert_ne!(task.status, TaskStatus::Failed, "must park, not fail");
    }
    assert!(parked, "the task must reach input_required");

    // The outstanding request is visible to the client.
    let outstanding = store
        .outstanding_input_requests(&task_id)
        .await
        .unwrap()
        .unwrap();
    assert!(outstanding.contains_key("decision"));

    // The client answers.
    let update = router
        .ready()
        .await
        .unwrap()
        .call(RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::UpdateTask(UpdateTaskParams {
                task_id: task_id.clone(),
                input_responses: [(
                    "decision".to_string(),
                    serde_json::json!({"action": "accept"}),
                )]
                .into_iter()
                .collect(),
                meta: None,
            }),
            extensions: tasks_client_extensions(),
        })
        .await
        .unwrap();
    assert!(update.inner.is_ok(), "update must be acknowledged");

    // The handler runs again and the task completes with its answer.
    let mut completed = None;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let (task, result, _) = store.get_task_result(&task_id).await.unwrap().unwrap();
        if task.status == TaskStatus::Completed {
            completed = result;
            break;
        }
        assert_ne!(
            task.status,
            TaskStatus::Failed,
            "the resumed handler must not fail"
        );
    }
    let completed = completed.expect("the task must complete after resuming");
    assert_eq!(completed.all_text(), "approved");
}

/// #1306: replay uses the root router's selected panic disclosure policy,
/// just like an ordinary call. The existing replay contract still records a
/// caught panic as a completed tool error rather than changing task semantics.
#[cfg(feature = "stateless")]
#[tokio::test]
async fn a_replayed_task_panic_uses_the_redacted_policy() {
    use crate::async_task::{MemoryTaskStore, TaskStore};
    use crate::protocol::{
        ElicitFormParams, ElicitFormSchema, ElicitRequestParams, InputRequest, InputRequests,
        InputRequiredResult, RequestOutcome,
    };

    const PAYLOAD: &str = "secret replay payload";
    const SAFE_MESSAGE: &str = "internal tool failure";

    let asks = ToolBuilder::new("private.replay")
        .description("Panics after input")
        .task_support(TaskSupportMode::Optional)
        .mrtr_handler::<serde_json::Value, _, _>(|ctx, _input| async move {
            if ctx.input_responses().is_some() {
                panic!("{PAYLOAD}");
            }
            let mut requests: InputRequests = Default::default();
            requests.insert(
                "decision".to_string(),
                InputRequest::Elicit(ElicitRequestParams::Form(ElicitFormParams {
                    mode: None,
                    message: "approve?".to_string(),
                    requested_schema: ElicitFormSchema::new(),
                    meta: None,
                })),
            );
            Ok(RequestOutcome::input_required(
                InputRequiredResult::with_requests(requests),
            ))
        })
        .build();

    let store = Arc::new(MemoryTaskStore::new());
    let mut router = McpRouter::new()
        .task_store(store.clone())
        .tool(asks)
        .with_tasks()
        .catch_panics_with(PanicPolicy::redacted(SAFE_MESSAGE));
    init_router(&mut router).await;

    let created = router
        .ready()
        .await
        .unwrap()
        .call(RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                input_responses: None,
                request_state: None,
                name: "private.replay".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
            extensions: tasks_client_extensions(),
        })
        .await
        .unwrap();
    let task_id = match created.inner {
        Ok(McpResponse::FinalCreateTask(result)) => result.task.metadata.task_id,
        other => panic!("expected a created task, got {other:?}"),
    };

    for _ in 0..50 {
        let task = store.get_task(&task_id).await.unwrap().unwrap();
        if task.status == TaskStatus::InputRequired {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        store.get_task(&task_id).await.unwrap().unwrap().status,
        TaskStatus::InputRequired
    );

    let update = router
        .ready()
        .await
        .unwrap()
        .call(RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::UpdateTask(UpdateTaskParams {
                task_id: task_id.clone(),
                input_responses: [(
                    "decision".to_string(),
                    serde_json::json!({"action": "accept"}),
                )]
                .into_iter()
                .collect(),
                meta: None,
            }),
            extensions: tasks_client_extensions(),
        })
        .await
        .unwrap();
    assert!(update.inner.is_ok(), "update must be acknowledged");

    let mut completed = None;
    for _ in 0..50 {
        let (task, result, _) = store.get_task_result(&task_id).await.unwrap().unwrap();
        if task.status == TaskStatus::Completed {
            completed = result;
            break;
        }
        assert_ne!(task.status, TaskStatus::Failed);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let completed = completed.expect("caught replay panic must complete as a tool error");
    assert!(completed.is_error);
    assert_eq!(completed.all_text(), SAFE_MESSAGE);
    assert!(!completed.all_text().contains(PAYLOAD));
    assert!(!completed.all_text().contains("private.replay"));
}

/// A store predating resumption cannot supply what a re-invocation needs,
/// so it must fail the task loudly rather than strand it in `working`.
///
/// `CountingTaskStore` delegates everything to `MemoryTaskStore` but does
/// not override `resume_context`, which is exactly the shape of an
/// external store written before resumption existed.
#[cfg(feature = "stateless")]
#[tokio::test]
async fn a_store_without_resume_support_fails_the_task() {
    let store = Arc::new(CountingTaskStore::new());
    assert!(
        store.resume_context("anything").await.unwrap().is_none(),
        "the trait default must report no resume support"
    );

    let (task_id, _cancel) = store
        .create_task("t", serde_json::json!({}), None, None)
        .await
        .unwrap();
    let requests: crate::protocol::InputRequests = [(
        "k".to_string(),
        crate::protocol::InputRequest::ListRoots(crate::protocol::ListRootsParams { meta: None }),
    )]
    .into_iter()
    .collect();
    store.require_input(&task_id, requests, None).await.unwrap();

    let mut router = McpRouter::new()
        .task_store(store.clone() as Arc<dyn TaskStore>)
        .with_tasks();
    init_router(&mut router).await;
    router
        .ready()
        .await
        .unwrap()
        .call(RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::UpdateTask(UpdateTaskParams {
                task_id: task_id.clone(),
                input_responses: [("k".to_string(), serde_json::json!({"roots": []}))]
                    .into_iter()
                    .collect(),
                meta: None,
            }),
            extensions: tasks_client_extensions(),
        })
        .await
        .unwrap();

    let (task, _, error) = store.get_task_result(&task_id).await.unwrap().unwrap();
    assert_eq!(
        task.status,
        TaskStatus::Failed,
        "a store that cannot resume must fail the task, not strand it"
    );
    assert!(
        error.unwrap().message.contains("resume_context"),
        "the failure must name what to implement"
    );
}
/// #1246 point 5: a handler that reuses a spent key is asking for
/// something SEP-2663 forbids the server to send. The park is refused,
/// and the task must say so rather than sit in `working` with nothing
/// outstanding, which no `tasks/update` could ever move.
#[cfg(feature = "stateless")]
#[tokio::test]
async fn reusing_a_spent_input_key_fails_the_task_instead_of_stranding_it() {
    use crate::async_task::{MemoryTaskStore, TaskStore};
    use crate::protocol::{
        ElicitFormParams, ElicitFormSchema, ElicitRequestParams, InputRequest, InputRequests,
        InputRequiredResult, RequestOutcome,
    };

    fn ask(key: &str) -> InputRequests {
        let mut requests: InputRequests = Default::default();
        requests.insert(
            key.to_string(),
            InputRequest::Elicit(ElicitRequestParams::Form(ElicitFormParams {
                mode: None,
                message: "approve?".to_string(),
                requested_schema: ElicitFormSchema::new(),
                meta: None,
            })),
        );
        requests
    }

    // Always asks for "decision", even after it has been answered.
    let repeats = ToolBuilder::new("repeats")
        .description("Reuses a spent key")
        .task_support(TaskSupportMode::Optional)
        .mrtr_handler::<serde_json::Value, _, _>(|_ctx, _input| async move {
            Ok(RequestOutcome::input_required(
                InputRequiredResult::with_requests(ask("decision")),
            ))
        })
        .build();

    let store = std::sync::Arc::new(MemoryTaskStore::new());
    let mut router = McpRouter::new()
        .task_store(store.clone())
        .tool(repeats)
        .with_tasks();
    init_router(&mut router).await;

    let resp = router
        .ready()
        .await
        .unwrap()
        .call(RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                input_responses: None,
                request_state: None,
                name: "repeats".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
            extensions: tasks_client_extensions(),
        })
        .await
        .unwrap();
    let task_id = match resp.inner {
        Ok(McpResponse::FinalCreateTask(result)) => result.task.metadata.task_id,
        other => panic!("expected a created task, got {other:?}"),
    };

    // First park is legitimate.
    let mut parked = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if store.get_task(&task_id).await.unwrap().unwrap().status == TaskStatus::InputRequired {
            parked = true;
            break;
        }
    }
    assert!(parked, "the task must reach input_required");

    // Answering resumes the handler, which asks for the same key again.
    router
        .ready()
        .await
        .unwrap()
        .call(RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::UpdateTask(UpdateTaskParams {
                task_id: task_id.clone(),
                input_responses: [(
                    "decision".to_string(),
                    serde_json::json!({"action": "accept"}),
                )]
                .into_iter()
                .collect(),
                meta: None,
            }),
            extensions: tasks_client_extensions(),
        })
        .await
        .unwrap();

    let mut failed = None;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let (task, _, error) = store.get_task_result(&task_id).await.unwrap().unwrap();
        if task.status == TaskStatus::Failed {
            failed = error;
            break;
        }
    }
    let error = failed.expect("the task must fail rather than strand");
    assert_eq!(error.message, "Task store operation failed");
    assert!(
        !error.message.contains("decision"),
        "store transition details must be redacted: {}",
        error.message
    );
}

/// #1246 point 1: `tasks/update` resumes whenever nothing is
/// outstanding, without checking that an `input_required -> working`
/// transition actually happened. A stray update while the first handler
/// is still running therefore starts a second one.
#[cfg(feature = "stateless")]
#[tokio::test]
async fn a_stray_update_while_working_must_not_reinvoke_the_handler() {
    use crate::async_task::{MemoryTaskStore, TaskStore};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let invocations = std::sync::Arc::new(AtomicUsize::new(0));
    let counter = invocations.clone();

    let slow = ToolBuilder::new("slow")
        .description("Runs for a while")
        .task_support(TaskSupportMode::Optional)
        .mrtr_handler::<serde_json::Value, _, _>(move |_ctx, _input| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                Ok(crate::protocol::RequestOutcome::Complete(
                    CallToolResult::text("done"),
                ))
            }
        })
        .build();

    let store = std::sync::Arc::new(MemoryTaskStore::new());
    let mut router = McpRouter::new()
        .task_store(store.clone())
        .tool(slow)
        .with_tasks();
    init_router(&mut router).await;

    let resp = router
        .ready()
        .await
        .unwrap()
        .call(RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                input_responses: None,
                request_state: None,
                name: "slow".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
            extensions: tasks_client_extensions(),
        })
        .await
        .unwrap();
    let task_id = match resp.inner {
        Ok(McpResponse::FinalCreateTask(result)) => result.task.metadata.task_id,
        other => panic!("expected a created task, got {other:?}"),
    };

    // The handler is running and nothing is outstanding.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "handler started once"
    );
    let task = store.get_task(&task_id).await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Working);

    // A stray update answering nothing.
    let update = router
        .ready()
        .await
        .unwrap()
        .call(RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::UpdateTask(UpdateTaskParams {
                task_id: task_id.clone(),
                input_responses: Default::default(),
                meta: None,
            }),
            extensions: tasks_client_extensions(),
        })
        .await
        .unwrap();
    assert!(update.inner.is_ok());

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "a stray update must not start a second handler"
    );
}

/// A handler that asks for input without naming any requests would strand
/// its task: no `tasks/update` can complete an empty request set, so the
/// task would sit in `input_required` forever. It fails instead.
///
/// This started life as #1207's assertion that the combination was
/// unsupported; #1208 made it work, and the remaining failure case is
/// this one.
#[cfg(feature = "stateless")]
#[tokio::test]
async fn asking_for_input_without_requests_fails_rather_than_stranding() {
    use crate::async_task::{MemoryTaskStore, TaskStore};
    use crate::protocol::{InputRequiredResult, RequestOutcome};

    let asks = ToolBuilder::new("asks")
        .description("Wants input")
        .task_support(TaskSupportMode::Optional)
        .mrtr_handler::<serde_json::Value, _, _>(|_ctx, _input| async move {
            Ok(RequestOutcome::input_required(
                InputRequiredResult::new().with_request_state("state"),
            ))
        })
        .build();

    let store = std::sync::Arc::new(MemoryTaskStore::new());
    let mut router = McpRouter::new()
        .task_store(store.clone())
        .tool(asks)
        .with_tasks();
    init_router(&mut router).await;

    let resp = router
        .ready()
        .await
        .unwrap()
        .call(RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                input_responses: None,
                request_state: None,
                name: "asks".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
            extensions: tasks_client_extensions(),
        })
        .await
        .unwrap();

    let task_id = match resp.inner {
        Ok(McpResponse::FinalCreateTask(result)) => result.task.metadata.task_id,
        other => panic!("expected a created task, got {other:?}"),
    };

    // The spawned handler runs concurrently; wait for a terminal state.
    let mut error = None;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let (task, _, err) = store.get_task_result(&task_id).await.unwrap().unwrap();
        if task.status == TaskStatus::Failed {
            error = err;
            break;
        }
    }

    let error = error.expect("the task must reach a terminal failure");
    assert!(
        error.message.contains("nothing to wait for"),
        "must explain why the task cannot park: {}",
        error.message
    );
    assert!(
        !error.message.contains("call_outcome_with_context"),
        "must not name an internal Rust API: {}",
        error.message
    );
}

/// #1188: on the stable lifecycle, `tasks/update` acknowledged the client
/// but never routed `inputResponses` to the store, so a task that reached
/// `input_required` stayed there forever while the same flow worked on
/// 2026-07-28.
#[tokio::test]
async fn stable_task_update_applies_input_responses() {
    use crate::async_task::{MemoryTaskStore, TaskStore};
    use crate::protocol::{InputRequest, ListRootsParams};

    let store = std::sync::Arc::new(MemoryTaskStore::new());
    let mut router = McpRouter::new().task_store(store.clone());
    init_router(&mut router).await;

    let (task_id, _cancel) = store
        .create_task("permission_gate", serde_json::json!({}), None, None)
        .await
        .expect("create task");
    let requests: crate::protocol::InputRequests = [(
        "permission".to_string(),
        InputRequest::ListRoots(ListRootsParams { meta: None }),
    )]
    .into_iter()
    .collect();
    store
        .require_input(&task_id, requests, Some("need a decision"))
        .await
        .expect("require input");
    assert_eq!(
        store.get_task(&task_id).await.unwrap().unwrap().status,
        TaskStatus::InputRequired
    );

    // A stable-lifecycle update: no final-protocol extensions present.
    let resp = router
        .ready()
        .await
        .unwrap()
        .call(RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::UpdateTask(UpdateTaskParams {
                task_id: task_id.clone(),
                input_responses: [("permission".to_string(), serde_json::json!({"roots": []}))]
                    .into_iter()
                    .collect(),
                meta: None,
            }),
            extensions: Extensions::new(),
        })
        .await
        .unwrap();
    assert!(
        matches!(resp.inner, Ok(McpResponse::UpdateTask(_))),
        "the empty-result acknowledgment shape is unchanged: {:?}",
        resp.inner
    );

    // The response was consumed and the task resumed, rather than the ack
    // being a black hole.
    assert!(
        store
            .outstanding_input_requests(&task_id)
            .await
            .unwrap()
            .unwrap()
            .is_empty(),
        "the outstanding request must be consumed"
    );
    assert_eq!(
        store.get_task(&task_id).await.unwrap().unwrap().status,
        TaskStatus::Working,
        "answering the last outstanding request resumes the task"
    );
}

/// Unknown keys stay ignorable, which is the part of the spec allowance
/// that does apply, and an unknown task still reports -32602.
#[tokio::test]
async fn stable_task_update_ignores_unmatched_keys() {
    use crate::async_task::{MemoryTaskStore, TaskStore};

    let store = std::sync::Arc::new(MemoryTaskStore::new());
    let mut router = McpRouter::new().task_store(store.clone());
    init_router(&mut router).await;

    let (task_id, _cancel) = store
        .create_task("noop", serde_json::json!({}), None, None)
        .await
        .expect("create task");

    let resp = router
        .ready()
        .await
        .unwrap()
        .call(RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::UpdateTask(UpdateTaskParams {
                task_id: task_id.clone(),
                input_responses: [("never-issued".to_string(), serde_json::json!({"roots": []}))]
                    .into_iter()
                    .collect(),
                meta: None,
            }),
            extensions: Extensions::new(),
        })
        .await
        .unwrap();
    assert!(
        matches!(resp.inner, Ok(McpResponse::UpdateTask(_))),
        "an unmatched key is ignored, not rejected: {:?}",
        resp.inner
    );

    let unknown = router
        .ready()
        .await
        .unwrap()
        .call(RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::UpdateTask(UpdateTaskParams {
                task_id: "no-such-task".to_string(),
                input_responses: HashMap::new(),
                meta: None,
            }),
            extensions: Extensions::new(),
        })
        .await
        .unwrap();
    match unknown.inner {
        Err(error) => assert_eq!(error.code, -32602),
        other => panic!("expected -32602 for an unknown task, got {other:?}"),
    }
}

/// Unknown keys are idempotently ignored only when their values are valid
/// protocol responses. A malformed value is Invalid Params even when the key
/// does not match an outstanding request.
#[tokio::test]
async fn task_update_rejects_malformed_response_values() {
    use crate::async_task::{MemoryTaskStore, TaskStore};

    let store = std::sync::Arc::new(MemoryTaskStore::new());
    let mut router = McpRouter::new().task_store(store.clone());
    init_router(&mut router).await;

    let (task_id, _cancel) = store
        .create_task("noop", serde_json::json!({}), None, None)
        .await
        .expect("create task");
    let response = router
        .ready()
        .await
        .unwrap()
        .call(RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::UpdateTask(UpdateTaskParams {
                task_id,
                input_responses: [("never-issued".to_string(), serde_json::json!(42))]
                    .into_iter()
                    .collect(),
                meta: None,
            }),
            extensions: Extensions::new(),
        })
        .await
        .unwrap();

    match response.inner {
        Err(error) => assert_eq!(error.code, -32602),
        other => panic!("expected -32602 for malformed inputResponses, got {other:?}"),
    }
}

#[tokio::test]
async fn test_create_task_via_call_tool() {
    let add_tool = ToolBuilder::new("add")
        .description("Add two numbers")
        .task_support(TaskSupportMode::Optional)
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    let mut router = McpRouter::new().tool(add_tool);
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::CallTool(CallToolParams {
            input_responses: None,
            request_state: None,
            name: "add".to_string(),
            arguments: serde_json::json!({"a": 5, "b": 10}),
            meta: None,
            task: Some(TaskRequestParams { ttl: None }),
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::CreateTask(result)) => {
            assert!(!result.task.task_id.is_empty());
            assert_eq!(result.task.status, TaskStatus::Working);
        }
        _ => panic!("Expected CreateTask response"),
    }
}

/// [`TaskStore`] wrapper that counts calls, for proving dispatch goes
/// through an injected store.
struct CountingTaskStore {
    inner: MemoryTaskStore,
    creates: std::sync::atomic::AtomicUsize,
    gets: std::sync::atomic::AtomicUsize,
    completes: std::sync::atomic::AtomicUsize,
}

impl CountingTaskStore {
    fn new() -> Self {
        Self {
            inner: MemoryTaskStore::new(),
            creates: std::sync::atomic::AtomicUsize::new(0),
            gets: std::sync::atomic::AtomicUsize::new(0),
            completes: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl TaskStore for CountingTaskStore {
    async fn create_task(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        ttl: Option<u64>,
        owner: crate::async_task::TaskOwner,
    ) -> crate::async_task::Result<(String, crate::async_task::CancellationToken)> {
        self.creates
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner
            .create_task(tool_name, arguments, ttl, owner)
            .await
    }

    async fn get_task(&self, task_id: &str) -> crate::async_task::Result<Option<TaskObject>> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.get_task(task_id).await
    }

    async fn task_owner(
        &self,
        task_id: &str,
    ) -> crate::async_task::Result<Option<crate::async_task::TaskOwner>> {
        self.inner.task_owner(task_id).await
    }

    async fn get_task_result(
        &self,
        task_id: &str,
    ) -> crate::async_task::Result<Option<crate::async_task::TaskSnapshot>> {
        // Counted as a read: `tasks/get` dispatch fetches the snapshot so
        // it can inline the SEP-2663 DetailedTask terminal payload.
        self.gets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.get_task_result(task_id).await
    }

    async fn wait_for_completion(
        &self,
        task_id: &str,
    ) -> crate::async_task::Result<Option<crate::async_task::TaskSnapshot>> {
        self.inner.wait_for_completion(task_id).await
    }

    async fn list_tasks(
        &self,
        status_filter: Option<TaskStatus>,
    ) -> crate::async_task::Result<Vec<TaskObject>> {
        self.inner.list_tasks(status_filter).await
    }

    async fn require_input(
        &self,
        task_id: &str,
        requests: crate::protocol::InputRequests,
        message: Option<&str>,
    ) -> crate::async_task::Result<bool> {
        self.inner.require_input(task_id, requests, message).await
    }

    async fn outstanding_input_requests(
        &self,
        task_id: &str,
    ) -> crate::async_task::Result<Option<crate::protocol::InputRequests>> {
        self.inner.outstanding_input_requests(task_id).await
    }

    async fn apply_input_responses(
        &self,
        task_id: &str,
        responses: crate::protocol::InputResponses,
    ) -> crate::async_task::Result<Option<crate::async_task::AppliedInputResponses>> {
        self.inner.apply_input_responses(task_id, responses).await
    }

    async fn set_ttl(&self, task_id: &str, ttl_ms: u64) -> crate::async_task::Result<bool> {
        self.inner.set_ttl(task_id, ttl_ms).await
    }

    async fn complete_task(
        &self,
        task_id: &str,
        result: CallToolResult,
    ) -> crate::async_task::Result<bool> {
        self.completes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.complete_task(task_id, result).await
    }

    async fn fail_task(
        &self,
        task_id: &str,
        error: JsonRpcError,
    ) -> crate::async_task::Result<bool> {
        self.inner.fail_task(task_id, error).await
    }

    async fn cancel_task(
        &self,
        task_id: &str,
        reason: Option<&str>,
    ) -> crate::async_task::Result<Option<TaskObject>> {
        self.inner.cancel_task(task_id, reason).await
    }
}

#[tokio::test]
async fn test_injected_task_store_used_by_dispatch() {
    let store = Arc::new(CountingTaskStore::new());

    let add_tool = ToolBuilder::new("add")
        .description("Add two numbers")
        .task_support(TaskSupportMode::Optional)
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    let mut router = McpRouter::new()
        .tool(add_tool)
        .task_store(store.clone() as Arc<dyn TaskStore>);
    init_router(&mut router).await;

    // Task-augmented tools/call must create the task in the injected store.
    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::CallTool(CallToolParams {
            input_responses: None,
            request_state: None,
            name: "add".to_string(),
            arguments: serde_json::json!({"a": 2, "b": 3}),
            meta: None,
            task: Some(TaskRequestParams { ttl: None }),
        }),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    let task_id = match resp.inner {
        Ok(McpResponse::CreateTask(result)) => result.task.task_id,
        other => panic!("Expected CreateTask response, got {other:?}"),
    };

    assert_eq!(
        store.creates.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "create_task must go through the injected store"
    );

    // Wait for the background execution to record completion.
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(
        store.completes.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "complete_task must go through the injected store"
    );

    // tasks/get must read from the injected store.
    let gets_before = store.gets.load(std::sync::atomic::Ordering::Relaxed);
    let req = RouterRequest {
        id: RequestId::Number(2),
        inner: McpRequest::GetTaskInfo(GetTaskInfoParams {
            task_id: task_id.clone(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    match resp.inner {
        Ok(McpResponse::GetTaskInfo(info)) => {
            assert_eq!(info.task_id, task_id);
            assert_eq!(info.status, TaskStatus::Completed);
        }
        other => panic!("Expected GetTaskInfo response, got {other:?}"),
    }
    assert!(
        store.gets.load(std::sync::atomic::Ordering::Relaxed) > gets_before,
        "tasks/get must go through the injected store"
    );
}

#[tokio::test]
async fn test_removed_tasks_methods_get_method_not_found() {
    // Final SEP-2663 removes tasks/list and tasks/result. They no longer
    // parse into typed requests, so the router sees Unknown and must
    // answer MethodNotFound (-32601).
    let mut router = McpRouter::new();
    init_router(&mut router).await;

    for method in ["tasks/list", "tasks/result"] {
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::Unknown {
                method: method.to_string(),
                params: None,
            },
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();

        match resp.inner {
            Err(err) => {
                assert_eq!(err.code, -32601, "{method} must be MethodNotFound");
            }
            other => panic!("Expected MethodNotFound error for {method}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn test_task_lifecycle_complete() {
    let add_tool = ToolBuilder::new("add")
        .description("Add two numbers")
        .task_support(TaskSupportMode::Optional)
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    let mut router = McpRouter::new().tool(add_tool);
    init_router(&mut router).await;

    // Create task via tools/call with task params
    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::CallTool(CallToolParams {
            input_responses: None,
            request_state: None,
            name: "add".to_string(),
            arguments: serde_json::json!({"a": 7, "b": 8}),
            meta: None,
            task: Some(TaskRequestParams { ttl: None }),
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    let task_id = match resp.inner {
        Ok(McpResponse::CreateTask(result)) => result.task.task_id,
        _ => panic!("Expected CreateTask response"),
    };

    // Wait for task to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Poll task state via tasks/get (final SEP-2663 removed the blocking
    // tasks/result; the terminal result payload on tasks/get is the
    // phase 4 DetailedTask work, #951).
    let req = RouterRequest {
        id: RequestId::Number(2),
        inner: McpRequest::GetTaskInfo(GetTaskInfoParams {
            task_id: task_id.clone(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::GetTaskInfo(info)) => {
            assert_eq!(info.task_id, task_id);
            assert_eq!(info.status, TaskStatus::Completed);
        }
        _ => panic!("Expected GetTaskInfo response"),
    }
}

/// Build a router whose one tool parks for longer than any test will wait.
async fn parked_router() -> McpRouter {
    let parked = ToolBuilder::new("park")
        .description("Never finishes on its own")
        .handler(|_input: serde_json::Value| async move {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            Ok(CallToolResult::text("unreachable"))
        })
        .build();
    let mut router = McpRouter::new().tool(parked);
    init_router(&mut router).await;
    router
}

fn park_request(id: i64) -> RouterRequest {
    RouterRequest {
        id: RequestId::Number(id),
        inner: McpRequest::CallTool(CallToolParams {
            name: "park".to_string(),
            arguments: serde_json::json!({}),
            input_responses: None,
            request_state: None,
            meta: None,
            task: None,
        }),
        extensions: Extensions::new(),
    }
}

fn tracked_dispatches(router: &McpRouter, id: &RequestId) -> usize {
    router
        .inner
        .in_flight
        .read()
        .unwrap()
        .get(id)
        .map_or(0, Vec::len)
}

/// #1270: tracking was removed on the success path in `Service::call`, so a
/// future dropped before reaching it left its entry behind for the process
/// lifetime. A timeout layer firing or an HTTP client disconnecting does
/// exactly that.
#[tokio::test]
async fn dropping_a_request_future_untracks_it() {
    let router = parked_router().await;
    let id = RequestId::Number(1);

    let mut service = router.clone();
    let mut future = Box::pin(service.call(park_request(1)));

    // Poll it far enough to reach the handler, which parks.
    let polled = tokio::time::timeout(std::time::Duration::from_millis(100), &mut future).await;
    assert!(polled.is_err(), "the parked handler must not have finished");
    assert_eq!(
        tracked_dispatches(&router, &id),
        1,
        "an in-flight request must be tracked, or this test proves nothing"
    );

    drop(future);

    assert_eq!(
        tracked_dispatches(&router, &id),
        0,
        "dropping the future must untrack the request"
    );
    assert!(router.inner.in_flight.read().unwrap().is_empty());
}

/// The counterpart to the two duplicate-id tests in `adversarial_input.rs`,
/// asserted against the registry rather than through the wire: twins under one
/// id coexist, and each drop removes only its own dispatch.
#[tokio::test]
async fn twin_dispatches_under_one_id_are_tracked_and_removed_independently() {
    let router = parked_router().await;
    let id = RequestId::Number(7);

    let mut first_service = router.clone();
    let mut first = Box::pin(first_service.call(park_request(7)));
    let _ = tokio::time::timeout(std::time::Duration::from_millis(100), &mut first).await;

    let mut second_service = router.clone();
    let mut second = Box::pin(second_service.call(park_request(7)));
    let _ = tokio::time::timeout(std::time::Duration::from_millis(100), &mut second).await;

    assert_eq!(
        tracked_dispatches(&router, &id),
        2,
        "the second registration must not evict the first"
    );

    drop(first);
    assert_eq!(
        tracked_dispatches(&router, &id),
        1,
        "one request ending must leave its twin tracked"
    );

    drop(second);
    assert_eq!(tracked_dispatches(&router, &id), 0);
}

#[tokio::test]
async fn test_task_cancellation() {
    // Use a slow tool to test cancellation
    let slow_tool = ToolBuilder::new("slow")
        .description("Slow tool")
        .task_support(TaskSupportMode::Optional)
        .handler(|_input: serde_json::Value| async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            Ok(CallToolResult::text("done"))
        })
        .build();

    let mut router = McpRouter::new().tool(slow_tool);
    init_router(&mut router).await;

    // Create task
    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::CallTool(CallToolParams {
            input_responses: None,
            request_state: None,
            name: "slow".to_string(),
            arguments: serde_json::json!({}),
            meta: None,
            task: Some(TaskRequestParams { ttl: None }),
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    let task_id = match resp.inner {
        Ok(McpResponse::CreateTask(result)) => result.task.task_id,
        _ => panic!("Expected CreateTask response"),
    };

    // Cancel the task
    let req = RouterRequest {
        id: RequestId::Number(2),
        inner: McpRequest::CancelTask(CancelTaskParams {
            task_id: task_id.clone(),
            reason: Some("Test cancellation".to_string()),
            meta: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    // SEP-2663 (final): cancel acknowledges with an empty result.
    match resp.inner {
        Ok(McpResponse::CancelTask(EmptyResult {})) => {}
        other => panic!("Expected empty CancelTask ack, got {other:?}"),
    }

    // Observable status is polled via tasks/get.
    let req = RouterRequest {
        id: RequestId::Number(3),
        inner: McpRequest::GetTaskInfo(GetTaskInfoParams {
            task_id: task_id.clone(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    match resp.inner {
        Ok(McpResponse::GetTaskInfo(info)) => {
            assert_eq!(info.status, TaskStatus::Cancelled);
        }
        _ => panic!("Expected GetTaskInfo response"),
    }
}

#[tokio::test]
async fn test_get_task_info() {
    let add_tool = ToolBuilder::new("add")
        .description("Add two numbers")
        .task_support(TaskSupportMode::Optional)
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    let mut router = McpRouter::new().tool(add_tool);
    init_router(&mut router).await;

    // Create task with TTL
    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::CallTool(CallToolParams {
            input_responses: None,
            request_state: None,
            name: "add".to_string(),
            arguments: serde_json::json!({"a": 1, "b": 2}),
            meta: None,
            task: Some(TaskRequestParams { ttl: Some(600_000) }),
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    let task_id = match resp.inner {
        Ok(McpResponse::CreateTask(result)) => result.task.task_id,
        _ => panic!("Expected CreateTask response"),
    };

    // Get task info
    let req = RouterRequest {
        id: RequestId::Number(2),
        inner: McpRequest::GetTaskInfo(GetTaskInfoParams {
            task_id: task_id.clone(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::GetTaskInfo(info)) => {
            assert_eq!(info.task_id, task_id);
            assert!(info.created_at.contains('T')); // ISO 8601
            assert_eq!(info.ttl, Some(600_000));
        }
        _ => panic!("Expected GetTaskInfo response"),
    }
}

#[tokio::test]
async fn test_task_forbidden_tool_rejects_task_params() {
    let tool = ToolBuilder::new("sync_only")
        .description("Sync only tool")
        .handler(|_input: serde_json::Value| async move { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new().tool(tool);
    init_router(&mut router).await;

    // Try to create task on a tool with Forbidden task support
    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::CallTool(CallToolParams {
            input_responses: None,
            request_state: None,
            name: "sync_only".to_string(),
            arguments: serde_json::json!({}),
            meta: None,
            task: Some(TaskRequestParams { ttl: None }),
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Err(e) => {
            assert!(e.message.contains("does not support async tasks"));
        }
        _ => panic!("Expected error response"),
    }
}

#[tokio::test]
async fn test_get_nonexistent_task() {
    let mut router = McpRouter::new();
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::GetTaskInfo(GetTaskInfoParams {
            task_id: "task-999".to_string(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Err(e) => {
            assert!(e.message.contains("not found"));
        }
        _ => panic!("Expected error response"),
    }
}

// =========================================================================
// Resource Subscription Tests
// =========================================================================

#[tokio::test]
async fn test_subscribe_to_resource() {
    use crate::resource::ResourceBuilder;

    let resource = ResourceBuilder::new("file:///test.txt")
        .name("Test File")
        .text("Hello");

    let mut router = McpRouter::new().resource(resource);
    init_router(&mut router).await;

    // Subscribe to the resource
    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::SubscribeResource(SubscribeResourceParams {
            uri: "file:///test.txt".to_string(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::SubscribeResource(_)) => {
            // Should be subscribed now
            assert!(router.is_subscribed("file:///test.txt"));
        }
        _ => panic!("Expected SubscribeResource response"),
    }
}

#[tokio::test]
async fn test_unsubscribe_from_resource() {
    use crate::resource::ResourceBuilder;

    let resource = ResourceBuilder::new("file:///test.txt")
        .name("Test File")
        .text("Hello");

    let mut router = McpRouter::new().resource(resource);
    init_router(&mut router).await;

    // Subscribe first
    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::SubscribeResource(SubscribeResourceParams {
            uri: "file:///test.txt".to_string(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };
    let _ = router.ready().await.unwrap().call(req).await.unwrap();
    assert!(router.is_subscribed("file:///test.txt"));

    // Now unsubscribe
    let req = RouterRequest {
        id: RequestId::Number(2),
        inner: McpRequest::UnsubscribeResource(UnsubscribeResourceParams {
            uri: "file:///test.txt".to_string(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::UnsubscribeResource(_)) => {
            // Should no longer be subscribed
            assert!(!router.is_subscribed("file:///test.txt"));
        }
        _ => panic!("Expected UnsubscribeResource response"),
    }
}

#[tokio::test]
async fn test_subscribe_nonexistent_resource() {
    let mut router = McpRouter::new();
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::SubscribeResource(SubscribeResourceParams {
            uri: "file:///nonexistent.txt".to_string(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Err(e) => {
            assert!(e.message.contains("not found"));
        }
        _ => panic!("Expected error response"),
    }
}

#[tokio::test]
async fn test_notify_resource_updated() {
    use crate::context::notification_channel;
    use crate::resource::ResourceBuilder;

    let (tx, mut rx) = notification_channel(10);

    let resource = ResourceBuilder::new("file:///test.txt")
        .name("Test File")
        .text("Hello");

    let router = McpRouter::new()
        .resource(resource)
        .with_notification_sender(tx);

    // First, manually subscribe (simulate subscription)
    router.subscribe("file:///test.txt");

    // Now notify
    let sent = router.notify_resource_updated("file:///test.txt");
    assert!(sent);

    // Check the notification was sent
    let notification = rx.try_recv().unwrap();
    match notification {
        ServerNotification::ResourceUpdated { uri } => {
            assert_eq!(uri, "file:///test.txt");
        }
        _ => panic!("Expected ResourceUpdated notification"),
    }
}

#[tokio::test]
async fn test_notify_resource_updated_not_subscribed() {
    use crate::context::notification_channel;
    use crate::resource::ResourceBuilder;

    let (tx, mut rx) = notification_channel(10);

    let resource = ResourceBuilder::new("file:///test.txt")
        .name("Test File")
        .text("Hello");

    let router = McpRouter::new()
        .resource(resource)
        .with_notification_sender(tx);

    // Try to notify without subscribing
    let sent = router.notify_resource_updated("file:///test.txt");
    assert!(!sent); // Should not send because not subscribed

    // Channel should be empty
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn test_notify_resources_list_changed() {
    use crate::context::notification_channel;

    let (tx, mut rx) = notification_channel(10);
    let router = McpRouter::new().with_notification_sender(tx);

    let sent = router.notify_resources_list_changed();
    assert!(sent);

    let notification = rx.try_recv().unwrap();
    match notification {
        ServerNotification::ResourcesListChanged => {}
        _ => panic!("Expected ResourcesListChanged notification"),
    }
}

#[tokio::test]
async fn test_subscribed_uris() {
    use crate::resource::ResourceBuilder;

    let resource1 = ResourceBuilder::new("file:///a.txt").name("A").text("A");

    let resource2 = ResourceBuilder::new("file:///b.txt").name("B").text("B");

    let router = McpRouter::new().resource(resource1).resource(resource2);

    // Subscribe to both
    router.subscribe("file:///a.txt");
    router.subscribe("file:///b.txt");

    let uris = router.subscribed_uris();
    assert_eq!(uris.len(), 2);
    assert!(uris.contains(&"file:///a.txt".to_string()));
    assert!(uris.contains(&"file:///b.txt".to_string()));
}

#[tokio::test]
async fn test_subscription_capability_advertised() {
    use crate::resource::ResourceBuilder;

    let resource = ResourceBuilder::new("file:///test.txt")
        .name("Test")
        .text("Hello");

    let mut router = McpRouter::new().resource(resource);

    // Initialize and check capabilities
    let init_req = RouterRequest {
        id: RequestId::Number(0),
        inner: McpRequest::Initialize(InitializeParams {
            protocol_version: "2025-11-25".to_string(),
            capabilities: ClientCapabilities {
                roots: None,
                sampling: None,
                elicitation: None,
                tasks: None,
                experimental: None,
                extensions: None,
            },
            client_info: Implementation {
                name: "test".to_string(),
                version: "1.0".to_string(),
                ..Default::default()
            },
            meta: None,
        }),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(init_req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::Initialize(result)) => {
            // Subscriptions are advertised by default on the legacy
            // lifecycle, which is where the methods exist.
            let resources_cap = result.capabilities.resources.unwrap();
            assert!(resources_cap.subscribe);
        }
        _ => panic!("Expected Initialize response"),
    }
}

/// #1261: `subscribe` was hardcoded true as soon as any resource existed, so
/// a server exposing read-only resources could not say otherwise.
#[test]
fn resource_subscriptions_can_be_declined() {
    let router = McpRouter::new()
        .server_info("read-only", "1.0")
        .resource(
            crate::resource::ResourceBuilder::new("mem://one")
                .name("one")
                .text("hi"),
        )
        .resource_subscriptions(false);

    let resources = router.capabilities().resources.expect("resources exist");
    assert!(
        !resources.subscribe,
        "a server that declined subscriptions must not advertise them"
    );

    // The default is unchanged.
    let default_router = McpRouter::new().server_info("default", "1.0").resource(
        crate::resource::ResourceBuilder::new("mem://one")
            .name("one")
            .text("hi"),
    );
    assert!(default_router.capabilities().resources.unwrap().subscribe);
}

/// The 2026-07-28 revision has no `resources/subscribe`, and this crate's own
/// inspector classifies the method as unavailable there. Advertising it would
/// promise a method the same build refuses to route (#1261).
#[test]
fn the_final_protocol_never_advertises_resource_subscriptions() {
    for advertise in [true, false] {
        let router = McpRouter::new()
            .server_info("final", "1.0")
            .resource(
                crate::resource::ResourceBuilder::new("mem://one")
                    .name("one")
                    .text("hi"),
            )
            .resource_subscriptions(advertise);

        let final_caps =
            router.capabilities_for_protocol(Some(crate::protocol::PROTOCOL_VERSION_2026_07_28));
        assert!(
            !final_caps.resources.expect("resources exist").subscribe,
            "advertise={advertise} must not surface subscribe on 2026-07-28"
        );

        // The legacy lifecycle still reflects the setting.
        let legacy_caps = router.capabilities_for_protocol(Some("2025-11-25"));
        assert_eq!(
            legacy_caps.resources.expect("resources exist").subscribe,
            advertise,
            "the legacy lifecycle must honour the setting"
        );
    }
}

/// A server with no resources advertises no resources capability at all, so
/// there is nothing for the setting to affect.
#[test]
fn declining_subscriptions_does_not_invent_a_resources_capability() {
    let router = McpRouter::new()
        .server_info("no-resources", "1.0")
        .resource_subscriptions(false);
    assert!(router.capabilities().resources.is_none());
}

/// Build a router with a tool, a resource, and a prompt registered, and
/// optionally a notification channel attached. Used to pin the pre-#1338
/// capability advertisement across every configuration that exists today.
fn router_with_all_capabilities(with_notifications: bool) -> McpRouter {
    let tool = ToolBuilder::new("test")
        .description("test")
        .handler(|_input: AddInput| async { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new()
        .server_info("defaults", "1.0")
        .tool(tool)
        .resource(
            crate::resource::ResourceBuilder::new("mem://one")
                .name("one")
                .text("hi"),
        )
        .prompt(crate::prompt::PromptBuilder::new("greeting").user_message("hi"));

    if with_notifications {
        let (tx, _rx) = crate::context::notification_channel(16);
        router = router.with_notification_sender(tx);
    }

    router
}

/// Regression test for #1338: before per-capability builders existed, all of
/// `tools.listChanged`, `prompts.listChanged`, `resources.listChanged`, and
/// `logging` were derived solely from whether a notification channel was
/// attached. This pins that exact behavior, in every configuration that
/// exists today, as a guard against the new builders' defaults drifting from
/// it.
#[test]
fn capability_defaults_are_unchanged_by_per_capability_builders() {
    // No notifications, nothing registered: no capability advertised at all.
    let empty_router = McpRouter::new().server_info("empty", "1.0");
    let empty_caps = empty_router.capabilities();
    assert!(empty_caps.tools.is_none());
    assert!(empty_caps.resources.is_none());
    assert!(empty_caps.prompts.is_none());
    assert!(empty_caps.logging.is_none());

    // Notifications attached, nothing registered: only logging is advertised,
    // since tools/resources/prompts capabilities are only present when the
    // corresponding items are registered.
    let (tx, _rx) = crate::context::notification_channel(16);
    let empty_with_notifications = McpRouter::new()
        .server_info("empty-notified", "1.0")
        .with_notification_sender(tx);
    let empty_notified_caps = empty_with_notifications.capabilities();
    assert!(empty_notified_caps.tools.is_none());
    assert!(empty_notified_caps.resources.is_none());
    assert!(empty_notified_caps.prompts.is_none());
    assert!(empty_notified_caps.logging.is_some());

    // Everything registered, no notification channel: every capability is
    // advertised, but every `list_changed` flag is false, and there is no
    // logging capability.
    let no_notifications = router_with_all_capabilities(false);
    let no_notif_caps = no_notifications.capabilities();
    let tools_cap = no_notif_caps.tools.expect("tool registered");
    assert!(!tools_cap.list_changed);
    let resources_cap = no_notif_caps.resources.expect("resource registered");
    assert!(!resources_cap.list_changed);
    assert!(resources_cap.subscribe, "subscribe still defaults to true");
    let prompts_cap = no_notif_caps.prompts.expect("prompt registered");
    assert!(!prompts_cap.list_changed);
    assert!(no_notif_caps.logging.is_none());

    // Everything registered, notification channel attached: every capability
    // is advertised and every `list_changed` flag, plus logging, is true.
    let with_notifications = router_with_all_capabilities(true);
    let notif_caps = with_notifications.capabilities();
    let tools_cap = notif_caps.tools.expect("tool registered");
    assert!(tools_cap.list_changed);
    let resources_cap = notif_caps.resources.expect("resource registered");
    assert!(resources_cap.list_changed);
    assert!(resources_cap.subscribe);
    let prompts_cap = notif_caps.prompts.expect("prompt registered");
    assert!(prompts_cap.list_changed);
    assert!(notif_caps.logging.is_some());
}

/// #1338: `tools_list_changed` overrides the notification-channel default in
/// either direction, whether or not a channel is attached.
#[test]
fn tools_list_changed_overrides_the_notification_channel_default() {
    for with_channel in [false, true] {
        for advertise in [true, false] {
            let router = router_with_all_capabilities(with_channel).tools_list_changed(advertise);
            let tools_cap = router.capabilities().tools.expect("tool registered");
            assert_eq!(
                tools_cap.list_changed, advertise,
                "with_channel={with_channel} advertise={advertise}"
            );
        }
    }
}

/// #1338: `prompts_list_changed` overrides the notification-channel default
/// in either direction, whether or not a channel is attached.
#[test]
fn prompts_list_changed_overrides_the_notification_channel_default() {
    for with_channel in [false, true] {
        for advertise in [true, false] {
            let router = router_with_all_capabilities(with_channel).prompts_list_changed(advertise);
            let prompts_cap = router.capabilities().prompts.expect("prompt registered");
            assert_eq!(
                prompts_cap.list_changed, advertise,
                "with_channel={with_channel} advertise={advertise}"
            );
        }
    }
}

/// #1338: `resources_list_changed` overrides the notification-channel
/// default in either direction, whether or not a channel is attached, and is
/// independent of `resource_subscriptions`.
#[test]
fn resources_list_changed_overrides_the_notification_channel_default() {
    for with_channel in [false, true] {
        for advertise in [true, false] {
            let router =
                router_with_all_capabilities(with_channel).resources_list_changed(advertise);
            let resources_cap = router
                .capabilities()
                .resources
                .expect("resource registered");
            assert_eq!(
                resources_cap.list_changed, advertise,
                "with_channel={with_channel} advertise={advertise}"
            );
            // Independent of resources.listChanged: subscribe keeps its own
            // default regardless of this setting.
            assert!(resources_cap.subscribe);
        }
    }
}

/// #1338: `mcp_logging` overrides the notification-channel default in either
/// direction, whether or not a channel is attached.
#[test]
fn mcp_logging_overrides_the_notification_channel_default() {
    for with_channel in [false, true] {
        for advertise in [true, false] {
            let router = router_with_all_capabilities(with_channel).mcp_logging(advertise);
            assert_eq!(
                router.capabilities().logging.is_some(),
                advertise,
                "with_channel={with_channel} advertise={advertise}"
            );
        }
    }
}

/// #1338: the four per-capability builders and
/// `StdioTransport::without_server_notifications` (#1257) compose sensibly.
///
/// `without_server_notifications` never attaches a notification channel to
/// the router, so on the router side its effect is identical to simply not
/// calling `with_notification_sender`. A server built on top of it still
/// gets the same all-or-nothing default (nothing advertised), but an
/// explicit per-capability override still wins, exactly as it does with a
/// channel attached. The all-or-nothing switch is the default; the builders
/// are the refinement underneath it, not something it disables.
#[test]
fn without_server_notifications_composes_with_explicit_overrides() {
    // No notification channel attached (what `without_server_notifications`
    // produces on the router): every flag defaults to false, matching the
    // "nothing advertised" behavior of the all-or-nothing switch.
    let router = router_with_all_capabilities(false);
    let caps = router.capabilities();
    assert!(!caps.tools.expect("tool registered").list_changed);
    assert!(!caps.prompts.expect("prompt registered").list_changed);
    assert!(!caps.resources.expect("resource registered").list_changed);
    assert!(caps.logging.is_none());

    // An explicit override still wins even though no channel is attached:
    // the flag is advertised, even though nothing will ever be sent over a
    // channel that does not exist. That is the caller's responsibility once
    // they have opted in explicitly.
    let router = router_with_all_capabilities(false)
        .tools_list_changed(true)
        .prompts_list_changed(true)
        .resources_list_changed(true)
        .mcp_logging(true);
    let caps = router.capabilities();
    assert!(caps.tools.expect("tool registered").list_changed);
    assert!(caps.prompts.expect("prompt registered").list_changed);
    assert!(caps.resources.expect("resource registered").list_changed);
    assert!(caps.logging.is_some());
}

#[tokio::test]
async fn test_completion_handler() {
    let router = McpRouter::new()
        .server_info("test", "1.0")
        .completion_handler(|params: CompleteParams| async move {
            // Return suggestions based on the argument value
            let prefix = &params.argument.value;
            let suggestions: Vec<String> = vec!["alpha", "beta", "gamma"]
                .into_iter()
                .filter(|s| s.starts_with(prefix))
                .map(String::from)
                .collect();
            Ok(CompleteResult::new(suggestions))
        });

    // Initialize
    let init_req = RouterRequest {
        id: RequestId::Number(0),
        inner: McpRequest::Initialize(InitializeParams {
            protocol_version: "2025-11-25".to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: Implementation {
                name: "test".to_string(),
                version: "1.0".to_string(),
                ..Default::default()
            },
            meta: None,
        }),
        extensions: Extensions::new(),
    };
    let resp = router
        .clone()
        .ready()
        .await
        .unwrap()
        .call(init_req)
        .await
        .unwrap();

    // Check that completions capability is advertised
    match resp.inner {
        Ok(McpResponse::Initialize(result)) => {
            assert!(result.capabilities.completions.is_some());
        }
        _ => panic!("Expected Initialize response"),
    }

    // Send initialized notification
    router.handle_notification(McpNotification::Initialized);

    // Test completion request
    let complete_req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::Complete(CompleteParams {
            reference: CompletionReference::prompt("test-prompt"),
            argument: CompletionArgument::new("query", "al"),
            context: None,
            meta: None,
        }),
        extensions: Extensions::new(),
    };
    let resp = router
        .clone()
        .ready()
        .await
        .unwrap()
        .call(complete_req)
        .await
        .unwrap();

    match resp.inner {
        Ok(McpResponse::Complete(result)) => {
            assert_eq!(result.completion.values, vec!["alpha"]);
        }
        _ => panic!("Expected Complete response"),
    }
}

#[tokio::test]
async fn test_completion_without_handler_returns_empty() {
    let router = McpRouter::new().server_info("test", "1.0");

    // Initialize
    let init_req = RouterRequest {
        id: RequestId::Number(0),
        inner: McpRequest::Initialize(InitializeParams {
            protocol_version: "2025-11-25".to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: Implementation {
                name: "test".to_string(),
                version: "1.0".to_string(),
                ..Default::default()
            },
            meta: None,
        }),
        extensions: Extensions::new(),
    };
    let resp = router
        .clone()
        .ready()
        .await
        .unwrap()
        .call(init_req)
        .await
        .unwrap();

    // Check that completions capability is NOT advertised
    match resp.inner {
        Ok(McpResponse::Initialize(result)) => {
            assert!(result.capabilities.completions.is_none());
        }
        _ => panic!("Expected Initialize response"),
    }

    // Send initialized notification
    router.handle_notification(McpNotification::Initialized);

    // Test completion request still works but returns empty
    let complete_req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::Complete(CompleteParams {
            reference: CompletionReference::prompt("test-prompt"),
            argument: CompletionArgument::new("query", "al"),
            context: None,
            meta: None,
        }),
        extensions: Extensions::new(),
    };
    let resp = router
        .clone()
        .ready()
        .await
        .unwrap()
        .call(complete_req)
        .await
        .unwrap();

    match resp.inner {
        Ok(McpResponse::Complete(result)) => {
            assert!(result.completion.values.is_empty());
        }
        _ => panic!("Expected Complete response"),
    }
}

#[tokio::test]
async fn test_tool_filter_list() {
    use crate::filter::CapabilityFilter;
    use crate::tool::Tool;

    let public_tool = ToolBuilder::new("public")
        .description("Public tool")
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("public")) })
        .build();

    let admin_tool = ToolBuilder::new("admin")
        .description("Admin tool")
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("admin")) })
        .build();

    let mut router = McpRouter::new()
        .tool(public_tool)
        .tool(admin_tool)
        .tool_filter(CapabilityFilter::new(|_, tool: &Tool| tool.name != "admin"));

    // Initialize session
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ListTools(ListToolsParams::default()),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::ListTools(result)) => {
            // Only public tool should be visible
            assert_eq!(result.tools.len(), 1);
            assert_eq!(result.tools[0].name, "public");
        }
        _ => panic!("Expected ListTools response"),
    }
}

#[tokio::test]
async fn test_tool_filter_call_denied() {
    use crate::filter::CapabilityFilter;
    use crate::tool::Tool;

    let admin_tool = ToolBuilder::new("admin")
        .description("Admin tool")
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("admin")) })
        .build();

    let mut router = McpRouter::new()
        .tool(admin_tool)
        .tool_filter(CapabilityFilter::new(|_, _: &Tool| false)); // Deny all

    // Initialize session
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::CallTool(CallToolParams {
            input_responses: None,
            request_state: None,
            name: "admin".to_string(),
            arguments: serde_json::json!({"a": 1, "b": 2}),
            meta: None,
            task: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    // Should get method not found error (default denial behavior)
    match resp.inner {
        Err(e) => {
            assert_eq!(e.code, -32601); // Method not found
        }
        _ => panic!("Expected JsonRpc error"),
    }
}

#[tokio::test]
async fn test_tool_filter_call_allowed() {
    use crate::filter::CapabilityFilter;
    use crate::tool::Tool;

    let public_tool = ToolBuilder::new("public")
        .description("Public tool")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    let mut router = McpRouter::new()
        .tool(public_tool)
        .tool_filter(CapabilityFilter::new(|_, _: &Tool| true)); // Allow all

    // Initialize session
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::CallTool(CallToolParams {
            input_responses: None,
            request_state: None,
            name: "public".to_string(),
            arguments: serde_json::json!({"a": 1, "b": 2}),
            meta: None,
            task: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::CallTool(result)) => {
            assert!(!result.is_error);
        }
        _ => panic!("Expected CallTool response"),
    }
}

#[tokio::test]
async fn test_tool_filter_custom_denial() {
    use crate::filter::{CapabilityFilter, DenialBehavior};
    use crate::tool::Tool;

    let admin_tool = ToolBuilder::new("admin")
        .description("Admin tool")
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("admin")) })
        .build();

    let mut router = McpRouter::new().tool(admin_tool).tool_filter(
        CapabilityFilter::new(|_, _: &Tool| false).denial_behavior(DenialBehavior::Unauthorized),
    );

    // Initialize session
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::CallTool(CallToolParams {
            input_responses: None,
            request_state: None,
            name: "admin".to_string(),
            arguments: serde_json::json!({"a": 1, "b": 2}),
            meta: None,
            task: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    // Should get forbidden error
    match resp.inner {
        Err(e) => {
            assert_eq!(e.code, -32007); // Forbidden
            assert!(e.message.contains("Unauthorized"));
        }
        _ => panic!("Expected JsonRpc error"),
    }
}

#[tokio::test]
async fn test_resource_filter_list() {
    use crate::filter::CapabilityFilter;
    use crate::resource::{Resource, ResourceBuilder};

    let public_resource = ResourceBuilder::new("file:///public.txt")
        .name("Public File")
        .text("public content");

    let secret_resource = ResourceBuilder::new("file:///secret.txt")
        .name("Secret File")
        .text("secret content");

    let mut router = McpRouter::new()
        .resource(public_resource)
        .resource(secret_resource)
        .resource_filter(CapabilityFilter::new(|_, r: &Resource| {
            !r.name.contains("Secret")
        }));

    // Initialize session
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ListResources(ListResourcesParams::default()),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::ListResources(result)) => {
            // Should only see public resource
            assert_eq!(result.resources.len(), 1);
            assert_eq!(result.resources[0].name, "Public File");
        }
        _ => panic!("Expected ListResources response"),
    }
}

#[tokio::test]
async fn test_resource_filter_read_denied() {
    use crate::filter::CapabilityFilter;
    use crate::resource::{Resource, ResourceBuilder};

    let secret_resource = ResourceBuilder::new("file:///secret.txt")
        .name("Secret File")
        .text("secret content");

    let mut router = McpRouter::new()
        .resource(secret_resource)
        .resource_filter(CapabilityFilter::new(|_, _: &Resource| false)); // Deny all

    // Initialize session
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ReadResource(ReadResourceParams {
            input_responses: None,
            request_state: None,
            uri: "file:///secret.txt".to_string(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    // Should get method not found error (default denial behavior)
    match resp.inner {
        Err(e) => {
            assert_eq!(e.code, -32601); // Method not found
        }
        _ => panic!("Expected JsonRpc error"),
    }
}

#[tokio::test]
async fn test_resource_filter_read_allowed() {
    use crate::filter::CapabilityFilter;
    use crate::resource::{Resource, ResourceBuilder};

    let public_resource = ResourceBuilder::new("file:///public.txt")
        .name("Public File")
        .text("public content");

    let mut router = McpRouter::new()
        .resource(public_resource)
        .resource_filter(CapabilityFilter::new(|_, _: &Resource| true)); // Allow all

    // Initialize session
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ReadResource(ReadResourceParams {
            input_responses: None,
            request_state: None,
            uri: "file:///public.txt".to_string(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::ReadResource(result)) => {
            assert_eq!(result.contents.len(), 1);
            assert_eq!(result.contents[0].text.as_deref(), Some("public content"));
        }
        _ => panic!("Expected ReadResource response"),
    }
}

#[tokio::test]
async fn test_resource_filter_custom_denial() {
    use crate::filter::{CapabilityFilter, DenialBehavior};
    use crate::resource::{Resource, ResourceBuilder};

    let secret_resource = ResourceBuilder::new("file:///secret.txt")
        .name("Secret File")
        .text("secret content");

    let mut router = McpRouter::new().resource(secret_resource).resource_filter(
        CapabilityFilter::new(|_, _: &Resource| false)
            .denial_behavior(DenialBehavior::Unauthorized),
    );

    // Initialize session
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ReadResource(ReadResourceParams {
            input_responses: None,
            request_state: None,
            uri: "file:///secret.txt".to_string(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    // Should get forbidden error
    match resp.inner {
        Err(e) => {
            assert_eq!(e.code, -32007); // Forbidden
            assert!(e.message.contains("Unauthorized"));
        }
        _ => panic!("Expected JsonRpc error"),
    }
}

#[tokio::test]
async fn test_prompt_filter_list() {
    use crate::filter::CapabilityFilter;
    use crate::prompt::{Prompt, PromptBuilder};

    let public_prompt = PromptBuilder::new("greeting")
        .description("A greeting")
        .user_message("Hello!");

    let admin_prompt = PromptBuilder::new("system_debug")
        .description("Admin prompt")
        .user_message("Debug");

    let mut router = McpRouter::new()
        .prompt(public_prompt)
        .prompt(admin_prompt)
        .prompt_filter(CapabilityFilter::new(|_, p: &Prompt| {
            !p.name.contains("system")
        }));

    // Initialize session
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ListPrompts(ListPromptsParams::default()),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::ListPrompts(result)) => {
            // Should only see public prompt
            assert_eq!(result.prompts.len(), 1);
            assert_eq!(result.prompts[0].name, "greeting");
        }
        _ => panic!("Expected ListPrompts response"),
    }
}

#[tokio::test]
async fn test_prompt_filter_get_denied() {
    use crate::filter::CapabilityFilter;
    use crate::prompt::{Prompt, PromptBuilder};
    use std::collections::HashMap;

    let admin_prompt = PromptBuilder::new("system_debug")
        .description("Admin prompt")
        .user_message("Debug");

    let mut router = McpRouter::new()
        .prompt(admin_prompt)
        .prompt_filter(CapabilityFilter::new(|_, _: &Prompt| false)); // Deny all

    // Initialize session
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::GetPrompt(GetPromptParams {
            input_responses: None,
            request_state: None,
            name: "system_debug".to_string(),
            arguments: HashMap::new(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    // Should get method not found error (default denial behavior)
    match resp.inner {
        Err(e) => {
            assert_eq!(e.code, -32601); // Method not found
        }
        _ => panic!("Expected JsonRpc error"),
    }
}

#[tokio::test]
async fn test_prompt_filter_get_allowed() {
    use crate::filter::CapabilityFilter;
    use crate::prompt::{Prompt, PromptBuilder};
    use std::collections::HashMap;

    let public_prompt = PromptBuilder::new("greeting")
        .description("A greeting")
        .user_message("Hello!");

    let mut router = McpRouter::new()
        .prompt(public_prompt)
        .prompt_filter(CapabilityFilter::new(|_, _: &Prompt| true)); // Allow all

    // Initialize session
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::GetPrompt(GetPromptParams {
            input_responses: None,
            request_state: None,
            name: "greeting".to_string(),
            arguments: HashMap::new(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::GetPrompt(result)) => {
            assert_eq!(result.messages.len(), 1);
        }
        _ => panic!("Expected GetPrompt response"),
    }
}

#[tokio::test]
async fn test_prompt_filter_custom_denial() {
    use crate::filter::{CapabilityFilter, DenialBehavior};
    use crate::prompt::{Prompt, PromptBuilder};
    use std::collections::HashMap;

    let admin_prompt = PromptBuilder::new("system_debug")
        .description("Admin prompt")
        .user_message("Debug");

    let mut router = McpRouter::new().prompt(admin_prompt).prompt_filter(
        CapabilityFilter::new(|_, _: &Prompt| false).denial_behavior(DenialBehavior::Unauthorized),
    );

    // Initialize session
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::GetPrompt(GetPromptParams {
            input_responses: None,
            request_state: None,
            name: "system_debug".to_string(),
            arguments: HashMap::new(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    // Should get forbidden error
    match resp.inner {
        Err(e) => {
            assert_eq!(e.code, -32007); // Forbidden
            assert!(e.message.contains("Unauthorized"));
        }
        _ => panic!("Expected JsonRpc error"),
    }
}

// =========================================================================
// Router Composition Tests (merge/nest)
// =========================================================================

#[derive(Debug, Deserialize, JsonSchema)]
struct StringInput {
    value: String,
}

#[tokio::test]
async fn test_router_merge_tools() {
    // Create first router with a tool
    let tool_a = ToolBuilder::new("tool_a")
        .description("Tool A")
        .handler(|_: StringInput| async move { Ok(CallToolResult::text("A")) })
        .build();

    let router_a = McpRouter::new().tool(tool_a);

    // Create second router with different tools
    let tool_b = ToolBuilder::new("tool_b")
        .description("Tool B")
        .handler(|_: StringInput| async move { Ok(CallToolResult::text("B")) })
        .build();
    let tool_c = ToolBuilder::new("tool_c")
        .description("Tool C")
        .handler(|_: StringInput| async move { Ok(CallToolResult::text("C")) })
        .build();

    let router_b = McpRouter::new().tool(tool_b).tool(tool_c);

    // Merge them
    let mut merged = McpRouter::new()
        .server_info("merged", "1.0")
        .merge(router_a)
        .merge(router_b);

    init_router(&mut merged).await;

    // List tools
    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ListTools(ListToolsParams::default()),
        extensions: Extensions::new(),
    };

    let resp = merged.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::ListTools(result)) => {
            assert_eq!(result.tools.len(), 3);
            let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
            assert!(names.contains(&"tool_a"));
            assert!(names.contains(&"tool_b"));
            assert!(names.contains(&"tool_c"));
        }
        _ => panic!("Expected ListTools response"),
    }
}

#[tokio::test]
async fn test_router_merge_overwrites_duplicates() {
    // Create first router with a tool
    let tool_v1 = ToolBuilder::new("shared")
        .description("Version 1")
        .handler(|_: StringInput| async move { Ok(CallToolResult::text("v1")) })
        .build();

    let router_a = McpRouter::new().tool(tool_v1);

    // Create second router with same tool name but different description
    let tool_v2 = ToolBuilder::new("shared")
        .description("Version 2")
        .handler(|_: StringInput| async move { Ok(CallToolResult::text("v2")) })
        .build();

    let router_b = McpRouter::new().tool(tool_v2);

    // Merge - second should win
    let mut merged = McpRouter::new().merge(router_a).merge(router_b);

    init_router(&mut merged).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ListTools(ListToolsParams::default()),
        extensions: Extensions::new(),
    };

    let resp = merged.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::ListTools(result)) => {
            assert_eq!(result.tools.len(), 1);
            assert_eq!(result.tools[0].name, "shared");
            assert_eq!(result.tools[0].description.as_deref(), Some("Version 2"));
        }
        _ => panic!("Expected ListTools response"),
    }
}

#[tokio::test]
async fn test_router_merge_resources() {
    use crate::resource::ResourceBuilder;

    // Create routers with different resources
    let router_a = McpRouter::new().resource(
        ResourceBuilder::new("file:///a.txt")
            .name("File A")
            .text("content a"),
    );

    let router_b = McpRouter::new().resource(
        ResourceBuilder::new("file:///b.txt")
            .name("File B")
            .text("content b"),
    );

    let mut merged = McpRouter::new().merge(router_a).merge(router_b);

    init_router(&mut merged).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ListResources(ListResourcesParams::default()),
        extensions: Extensions::new(),
    };

    let resp = merged.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::ListResources(result)) => {
            assert_eq!(result.resources.len(), 2);
            let uris: Vec<&str> = result.resources.iter().map(|r| r.uri.as_str()).collect();
            assert!(uris.contains(&"file:///a.txt"));
            assert!(uris.contains(&"file:///b.txt"));
        }
        _ => panic!("Expected ListResources response"),
    }
}

#[tokio::test]
async fn test_router_merge_prompts() {
    use crate::prompt::PromptBuilder;

    let router_a = McpRouter::new().prompt(PromptBuilder::new("prompt_a").user_message("Hello A"));

    let router_b = McpRouter::new().prompt(PromptBuilder::new("prompt_b").user_message("Hello B"));

    let mut merged = McpRouter::new().merge(router_a).merge(router_b);

    init_router(&mut merged).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ListPrompts(ListPromptsParams::default()),
        extensions: Extensions::new(),
    };

    let resp = merged.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::ListPrompts(result)) => {
            assert_eq!(result.prompts.len(), 2);
            let names: Vec<&str> = result.prompts.iter().map(|p| p.name.as_str()).collect();
            assert!(names.contains(&"prompt_a"));
            assert!(names.contains(&"prompt_b"));
        }
        _ => panic!("Expected ListPrompts response"),
    }
}

#[tokio::test]
async fn test_router_nest_prefixes_tools() {
    // Create a router with tools
    let tool_query = ToolBuilder::new("query")
        .description("Query the database")
        .handler(|_: StringInput| async move { Ok(CallToolResult::text("query result")) })
        .build();
    let tool_insert = ToolBuilder::new("insert")
        .description("Insert into database")
        .handler(|_: StringInput| async move { Ok(CallToolResult::text("insert result")) })
        .build();

    let db_router = McpRouter::new().tool(tool_query).tool(tool_insert);

    // Nest under "db" prefix
    let mut router = McpRouter::new()
        .server_info("nested", "1.0")
        .nest("db", db_router);

    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ListTools(ListToolsParams::default()),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::ListTools(result)) => {
            assert_eq!(result.tools.len(), 2);
            let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
            assert!(names.contains(&"db.query"));
            assert!(names.contains(&"db.insert"));
        }
        _ => panic!("Expected ListTools response"),
    }
}

#[tokio::test]
async fn test_router_nest_call_prefixed_tool() {
    let tool = ToolBuilder::new("echo")
        .description("Echo input")
        .handler(|input: StringInput| async move { Ok(CallToolResult::text(&input.value)) })
        .build();

    let nested_router = McpRouter::new().tool(tool);

    let mut router = McpRouter::new().nest("api", nested_router);

    init_router(&mut router).await;

    // Call the prefixed tool
    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::CallTool(CallToolParams {
            input_responses: None,
            request_state: None,
            name: "api.echo".to_string(),
            arguments: serde_json::json!({"value": "hello world"}),
            meta: None,
            task: None,
        }),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::CallTool(result)) => {
            assert!(!result.is_error);
            match &result.content[0] {
                Content::Text { text, .. } => assert_eq!(text, "hello world"),
                _ => panic!("Expected text content"),
            }
        }
        _ => panic!("Expected CallTool response"),
    }
}

#[tokio::test]
async fn test_router_multiple_nests() {
    let db_tool = ToolBuilder::new("query")
        .description("Database query")
        .handler(|_: StringInput| async move { Ok(CallToolResult::text("db")) })
        .build();

    let api_tool = ToolBuilder::new("fetch")
        .description("API fetch")
        .handler(|_: StringInput| async move { Ok(CallToolResult::text("api")) })
        .build();

    let db_router = McpRouter::new().tool(db_tool);
    let api_router = McpRouter::new().tool(api_tool);

    let mut router = McpRouter::new()
        .nest("db", db_router)
        .nest("api", api_router);

    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ListTools(ListToolsParams::default()),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::ListTools(result)) => {
            assert_eq!(result.tools.len(), 2);
            let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
            assert!(names.contains(&"db.query"));
            assert!(names.contains(&"api.fetch"));
        }
        _ => panic!("Expected ListTools response"),
    }
}

#[tokio::test]
async fn test_router_merge_and_nest_combined() {
    // Test combining merge and nest
    let tool_a = ToolBuilder::new("local")
        .description("Local tool")
        .handler(|_: StringInput| async move { Ok(CallToolResult::text("local")) })
        .build();

    let nested_tool = ToolBuilder::new("remote")
        .description("Remote tool")
        .handler(|_: StringInput| async move { Ok(CallToolResult::text("remote")) })
        .build();

    let nested_router = McpRouter::new().tool(nested_tool);

    let mut router = McpRouter::new()
        .tool(tool_a)
        .nest("external", nested_router);

    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ListTools(ListToolsParams::default()),
        extensions: Extensions::new(),
    };

    let resp = router.ready().await.unwrap().call(req).await.unwrap();

    match resp.inner {
        Ok(McpResponse::ListTools(result)) => {
            assert_eq!(result.tools.len(), 2);
            let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
            assert!(names.contains(&"local"));
            assert!(names.contains(&"external.remote"));
        }
        _ => panic!("Expected ListTools response"),
    }
}

#[tokio::test]
async fn test_router_merge_preserves_server_info() {
    let child_router = McpRouter::new()
        .server_info("child", "2.0")
        .instructions("Child instructions");

    let mut router = McpRouter::new()
        .server_info("parent", "1.0")
        .instructions("Parent instructions")
        .merge(child_router);

    init_router(&mut router).await;

    // Initialize response should have parent's server info
    let init_req = RouterRequest {
        id: RequestId::Number(99),
        inner: McpRequest::Initialize(InitializeParams {
            protocol_version: "2025-11-25".to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: Implementation {
                name: "test".to_string(),
                version: "1.0".to_string(),
                ..Default::default()
            },
            meta: None,
        }),
        extensions: Extensions::new(),
    };

    // Create fresh router for this test since we need to call initialize
    let child_router2 = McpRouter::new().server_info("child", "2.0");
    let mut fresh_router = McpRouter::new()
        .server_info("parent", "1.0")
        .merge(child_router2);

    let resp = fresh_router
        .ready()
        .await
        .unwrap()
        .call(init_req)
        .await
        .unwrap();

    match resp.inner {
        Ok(McpResponse::Initialize(result)) => {
            assert_eq!(result.server_info.name, "parent");
            assert_eq!(result.server_info.version, "1.0");
        }
        _ => panic!("Expected Initialize response"),
    }
}

// =========================================================================
// Auto-instructions tests
// =========================================================================

#[tokio::test]
async fn test_auto_instructions_tools_only() {
    let tool_a = ToolBuilder::new("alpha")
        .description("Alpha tool")
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
        .build();
    let tool_b = ToolBuilder::new("beta")
        .description("Beta tool")
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new()
        .auto_instructions()
        .tool(tool_a)
        .tool(tool_b);

    let resp = send_initialize(&mut router).await;
    let instructions = resp.instructions.expect("should have instructions");

    assert!(instructions.contains("## Tools"));
    assert!(instructions.contains("- **alpha**: Alpha tool"));
    assert!(instructions.contains("- **beta**: Beta tool"));
    // No resources or prompts sections
    assert!(!instructions.contains("## Resources"));
    assert!(!instructions.contains("## Prompts"));
}

#[tokio::test]
async fn test_auto_instructions_with_annotations() {
    let read_only_tool = ToolBuilder::new("query")
        .description("Run a query")
        .read_only()
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
        .build();
    let destructive_tool = ToolBuilder::new("delete")
        .description("Delete a record")
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
        .build();
    let idempotent_tool = ToolBuilder::new("upsert")
        .description("Upsert a record")
        .non_destructive()
        .idempotent()
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new()
        .auto_instructions()
        .tool(read_only_tool)
        .tool(destructive_tool)
        .tool(idempotent_tool);

    let resp = send_initialize(&mut router).await;
    let instructions = resp.instructions.unwrap();

    assert!(instructions.contains("- **query**: Run a query [read-only]"));
    // delete has no annotations set via builder, so no tags
    assert!(instructions.contains("- **delete**: Delete a record\n"));
    assert!(instructions.contains("- **upsert**: Upsert a record [idempotent]"));
}

#[tokio::test]
async fn test_auto_instructions_with_resources() {
    use crate::resource::ResourceBuilder;

    let resource = ResourceBuilder::new("file:///schema.sql")
        .name("Schema")
        .description("Database schema")
        .text("CREATE TABLE ...");

    let mut router = McpRouter::new().auto_instructions().resource(resource);

    let resp = send_initialize(&mut router).await;
    let instructions = resp.instructions.unwrap();

    assert!(instructions.contains("## Resources"));
    assert!(instructions.contains("- **file:///schema.sql**: Database schema"));
    assert!(!instructions.contains("## Tools"));
}

#[tokio::test]
async fn test_auto_instructions_with_resource_templates() {
    use crate::resource::ResourceTemplateBuilder;

    let template = ResourceTemplateBuilder::new("file:///{path}")
        .name("File")
        .description("Read a file by path")
        .handler(
            |_uri: String, _vars: std::collections::HashMap<String, String>| async move {
                Ok(crate::ReadResourceResult::text("content", "text/plain"))
            },
        );

    let mut router = McpRouter::new()
        .auto_instructions()
        .resource_template(template);

    let resp = send_initialize(&mut router).await;
    let instructions = resp.instructions.unwrap();

    assert!(instructions.contains("## Resources"));
    assert!(instructions.contains("- **file:///{path}**: Read a file by path"));
}

#[tokio::test]
async fn test_auto_instructions_with_prompts() {
    use crate::prompt::PromptBuilder;

    let prompt = PromptBuilder::new("write_query")
        .description("Help write a SQL query")
        .user_message("Write a query for: {task}");

    let mut router = McpRouter::new().auto_instructions().prompt(prompt);

    let resp = send_initialize(&mut router).await;
    let instructions = resp.instructions.unwrap();

    assert!(instructions.contains("## Prompts"));
    assert!(instructions.contains("- **write_query**: Help write a SQL query"));
    assert!(!instructions.contains("## Tools"));
}

#[tokio::test]
async fn test_auto_instructions_all_sections() {
    use crate::prompt::PromptBuilder;
    use crate::resource::ResourceBuilder;

    let tool = ToolBuilder::new("query")
        .description("Execute SQL")
        .read_only()
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
        .build();
    let resource = ResourceBuilder::new("db://schema")
        .name("Schema")
        .description("Full database schema")
        .text("schema");
    let prompt = PromptBuilder::new("write_query")
        .description("Help write a SQL query")
        .user_message("Write a query");

    let mut router = McpRouter::new()
        .auto_instructions()
        .tool(tool)
        .resource(resource)
        .prompt(prompt);

    let resp = send_initialize(&mut router).await;
    let instructions = resp.instructions.unwrap();

    // All three sections present
    assert!(instructions.contains("## Tools"));
    assert!(instructions.contains("## Resources"));
    assert!(instructions.contains("## Prompts"));

    // Sections appear in order: Tools, Resources, Prompts
    let tools_pos = instructions.find("## Tools").unwrap();
    let resources_pos = instructions.find("## Resources").unwrap();
    let prompts_pos = instructions.find("## Prompts").unwrap();
    assert!(tools_pos < resources_pos);
    assert!(resources_pos < prompts_pos);
}

#[tokio::test]
async fn test_auto_instructions_with_prefix_and_suffix() {
    let tool = ToolBuilder::new("echo")
        .description("Echo input")
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new()
        .auto_instructions_with(
            Some("This server provides echo capabilities."),
            Some("Contact admin@example.com for support."),
        )
        .tool(tool);

    let resp = send_initialize(&mut router).await;
    let instructions = resp.instructions.unwrap();

    assert!(instructions.starts_with("This server provides echo capabilities."));
    assert!(instructions.ends_with("Contact admin@example.com for support."));
    assert!(instructions.contains("## Tools"));
    assert!(instructions.contains("- **echo**: Echo input"));
}

#[tokio::test]
async fn test_auto_instructions_prefix_only() {
    let tool = ToolBuilder::new("echo")
        .description("Echo input")
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new()
        .auto_instructions_with(Some("My server intro."), None::<String>)
        .tool(tool);

    let resp = send_initialize(&mut router).await;
    let instructions = resp.instructions.unwrap();

    assert!(instructions.starts_with("My server intro."));
    assert!(instructions.contains("- **echo**: Echo input"));
}

#[tokio::test]
async fn test_auto_instructions_empty_router() {
    let mut router = McpRouter::new().auto_instructions();

    let resp = send_initialize(&mut router).await;
    let instructions = resp.instructions.expect("should have instructions");

    // No sections when nothing is registered
    assert!(!instructions.contains("## Tools"));
    assert!(!instructions.contains("## Resources"));
    assert!(!instructions.contains("## Prompts"));
    assert!(instructions.is_empty());
}

#[tokio::test]
async fn test_auto_instructions_overrides_manual() {
    let tool = ToolBuilder::new("echo")
        .description("Echo input")
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new()
        .instructions("This will be overridden")
        .auto_instructions()
        .tool(tool);

    let resp = send_initialize(&mut router).await;
    let instructions = resp.instructions.unwrap();

    assert!(!instructions.contains("This will be overridden"));
    assert!(instructions.contains("- **echo**: Echo input"));
}

#[tokio::test]
async fn test_no_auto_instructions_returns_manual() {
    let tool = ToolBuilder::new("echo")
        .description("Echo input")
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new()
        .instructions("Manual instructions here")
        .tool(tool);

    let resp = send_initialize(&mut router).await;
    let instructions = resp.instructions.unwrap();

    assert_eq!(instructions, "Manual instructions here");
}

#[tokio::test]
async fn test_auto_instructions_no_description_fallback() {
    let tool = ToolBuilder::new("mystery")
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new().auto_instructions().tool(tool);

    let resp = send_initialize(&mut router).await;
    let instructions = resp.instructions.unwrap();

    assert!(instructions.contains("- **mystery**: No description"));
}

#[tokio::test]
async fn test_auto_instructions_sorted_alphabetically() {
    let tool_z = ToolBuilder::new("zebra")
        .description("Z tool")
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
        .build();
    let tool_a = ToolBuilder::new("alpha")
        .description("A tool")
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
        .build();
    let tool_m = ToolBuilder::new("middle")
        .description("M tool")
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new()
        .auto_instructions()
        .tool(tool_z)
        .tool(tool_a)
        .tool(tool_m);

    let resp = send_initialize(&mut router).await;
    let instructions = resp.instructions.unwrap();

    let alpha_pos = instructions.find("**alpha**").unwrap();
    let middle_pos = instructions.find("**middle**").unwrap();
    let zebra_pos = instructions.find("**zebra**").unwrap();
    assert!(alpha_pos < middle_pos);
    assert!(middle_pos < zebra_pos);
}

#[tokio::test]
async fn test_auto_instructions_read_only_and_idempotent_tags() {
    let tool = ToolBuilder::new("safe_update")
        .description("Safe update operation")
        .idempotent()
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new().auto_instructions().tool(tool);

    let resp = send_initialize(&mut router).await;
    let instructions = resp.instructions.unwrap();

    assert!(
        instructions.contains("[idempotent]"),
        "got: {}",
        instructions
    );
}

#[tokio::test]
async fn test_auto_instructions_lazy_generation() {
    // auto_instructions() is called BEFORE tools are registered
    // but instructions should still include tools
    let mut router = McpRouter::new().auto_instructions();

    let tool = ToolBuilder::new("late_tool")
        .description("Added after auto_instructions")
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
        .build();

    router = router.tool(tool);

    let resp = send_initialize(&mut router).await;
    let instructions = resp.instructions.unwrap();

    assert!(instructions.contains("- **late_tool**: Added after auto_instructions"));
}

#[tokio::test]
async fn test_auto_instructions_multiple_annotation_tags() {
    let tool = ToolBuilder::new("update")
        .description("Update a record")
        .annotations(ToolAnnotations {
            read_only_hint: true,
            idempotent_hint: true,
            ..Default::default()
        })
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new().auto_instructions().tool(tool);

    let resp = send_initialize(&mut router).await;
    let instructions = resp.instructions.unwrap();

    assert!(
        instructions.contains("[read-only, idempotent]"),
        "got: {}",
        instructions
    );
}

#[tokio::test]
async fn test_auto_instructions_no_annotations_no_tags() {
    // Tools without annotations should have no tags at all
    let tool = ToolBuilder::new("fetch")
        .description("Fetch data")
        .handler(|_: AddInput| async move { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new().auto_instructions().tool(tool);

    let resp = send_initialize(&mut router).await;
    let instructions = resp.instructions.unwrap();

    // No bracket tags
    assert!(
        !instructions.contains('['),
        "should have no tags, got: {}",
        instructions
    );
    assert!(instructions.contains("- **fetch**: Fetch data"));
}

/// Helper to send an Initialize request and return the result
async fn send_initialize(router: &mut McpRouter) -> InitializeResult {
    let init_req = RouterRequest {
        id: RequestId::Number(0),
        inner: McpRequest::Initialize(InitializeParams {
            protocol_version: "2025-11-25".to_string(),
            capabilities: ClientCapabilities {
                roots: None,
                sampling: None,
                elicitation: None,
                tasks: None,
                experimental: None,
                extensions: None,
            },
            client_info: Implementation {
                name: "test".to_string(),
                version: "1.0".to_string(),
                ..Default::default()
            },
            meta: None,
        }),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(init_req).await.unwrap();
    match resp.inner {
        Ok(McpResponse::Initialize(result)) => result,
        other => panic!("Expected Initialize response, got {:?}", other),
    }
}

#[tokio::test]
async fn test_notify_tools_list_changed() {
    let (tx, mut rx) = crate::context::notification_channel(16);

    let router = McpRouter::new()
        .server_info("test", "1.0")
        .with_notification_sender(tx);

    assert!(router.notify_tools_list_changed());

    let notification = rx.recv().await.unwrap();
    assert!(matches!(notification, ServerNotification::ToolsListChanged));
}

#[tokio::test]
async fn test_notify_prompts_list_changed() {
    let (tx, mut rx) = crate::context::notification_channel(16);

    let router = McpRouter::new()
        .server_info("test", "1.0")
        .with_notification_sender(tx);

    assert!(router.notify_prompts_list_changed());

    let notification = rx.recv().await.unwrap();
    assert!(matches!(
        notification,
        ServerNotification::PromptsListChanged
    ));
}

#[tokio::test]
async fn test_notify_without_sender_returns_false() {
    let router = McpRouter::new().server_info("test", "1.0");

    assert!(!router.notify_tools_list_changed());
    assert!(!router.notify_prompts_list_changed());
    assert!(!router.notify_resources_list_changed());
}

#[tokio::test]
async fn test_list_changed_capabilities_with_notification_sender() {
    let (tx, _rx) = crate::context::notification_channel(16);
    let tool = ToolBuilder::new("test")
        .description("test")
        .handler(|_input: AddInput| async { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new()
        .server_info("test", "1.0")
        .tool(tool)
        .with_notification_sender(tx);

    init_router(&mut router).await;

    let caps = router.capabilities();
    let tools_cap = caps.tools.expect("tools capability should be present");
    assert!(
        tools_cap.list_changed,
        "tools.listChanged should be true when notification sender is configured"
    );
}

#[tokio::test]
async fn test_list_changed_capabilities_without_notification_sender() {
    let tool = ToolBuilder::new("test")
        .description("test")
        .handler(|_input: AddInput| async { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new().server_info("test", "1.0").tool(tool);

    init_router(&mut router).await;

    let caps = router.capabilities();
    let tools_cap = caps.tools.expect("tools capability should be present");
    assert!(
        !tools_cap.list_changed,
        "tools.listChanged should be false without notification sender"
    );
}

#[tokio::test]
async fn test_set_logging_level_filters_messages() {
    let (tx, mut rx) = crate::context::notification_channel(16);

    let mut router = McpRouter::new()
        .server_info("test", "1.0")
        .with_notification_sender(tx);

    init_router(&mut router).await;

    // Set logging level to Warning
    let set_level_req = RouterRequest {
        id: RequestId::Number(99),
        inner: McpRequest::SetLoggingLevel(SetLogLevelParams {
            level: LogLevel::Warning,
            meta: None,
        }),
        extensions: crate::context::Extensions::new(),
    };
    let resp = router
        .ready()
        .await
        .unwrap()
        .call(set_level_req)
        .await
        .unwrap();
    assert!(matches!(resp.inner, Ok(McpResponse::SetLoggingLevel(_))));

    // Create a context from the router (simulating a handler)
    let ctx = router.create_context(RequestId::Number(100), None);

    // Error (more severe than Warning) should pass through
    ctx.send_log(LoggingMessageParams::new(
        LogLevel::Error,
        serde_json::Value::Null,
    ));
    assert!(
        rx.try_recv().is_ok(),
        "Error should pass through Warning filter"
    );

    // Info (less severe than Warning) should be filtered
    ctx.send_log(LoggingMessageParams::new(
        LogLevel::Info,
        serde_json::Value::Null,
    ));
    assert!(
        rx.try_recv().is_err(),
        "Info should be filtered at Warning level"
    );
}

#[test]
fn test_paginate_no_page_size() {
    let items = vec![1, 2, 3, 4, 5];
    let (page, cursor) = paginate(items.clone(), None, None).unwrap();
    assert_eq!(page, items);
    assert!(cursor.is_none());
}

#[test]
fn test_paginate_first_page() {
    let items = vec![1, 2, 3, 4, 5];
    let (page, cursor) = paginate(items, None, Some(2)).unwrap();
    assert_eq!(page, vec![1, 2]);
    assert!(cursor.is_some());
}

#[test]
fn test_paginate_middle_page() {
    let items = vec![1, 2, 3, 4, 5];
    let (page1, cursor1) = paginate(items.clone(), None, Some(2)).unwrap();
    assert_eq!(page1, vec![1, 2]);

    let (page2, cursor2) = paginate(items, cursor1.as_deref(), Some(2)).unwrap();
    assert_eq!(page2, vec![3, 4]);
    assert!(cursor2.is_some());
}

#[test]
fn test_paginate_last_page() {
    let items = vec![1, 2, 3, 4, 5];
    // Skip to offset 4 (last item)
    let cursor = encode_cursor(4);
    let (page, next) = paginate(items, Some(&cursor), Some(2)).unwrap();
    assert_eq!(page, vec![5]);
    assert!(next.is_none());
}

#[test]
fn test_paginate_exact_boundary() {
    let items = vec![1, 2, 3, 4];
    let (page, cursor) = paginate(items, None, Some(4)).unwrap();
    assert_eq!(page, vec![1, 2, 3, 4]);
    assert!(cursor.is_none());
}

#[test]
fn test_paginate_invalid_cursor() {
    let items = vec![1, 2, 3];
    let result = paginate(items, Some("not-valid-base64!@#$"), Some(2));
    assert!(result.is_err());
}

#[test]
fn test_cursor_round_trip() {
    let offset = 42;
    let encoded = encode_cursor(offset);
    let decoded = decode_cursor(&encoded).unwrap();
    assert_eq!(decoded, offset);
}

#[tokio::test]
async fn test_list_tools_pagination() {
    let tool_a = ToolBuilder::new("alpha")
        .description("a")
        .handler(|_input: AddInput| async { Ok(CallToolResult::text("ok")) })
        .build();
    let tool_b = ToolBuilder::new("beta")
        .description("b")
        .handler(|_input: AddInput| async { Ok(CallToolResult::text("ok")) })
        .build();
    let tool_c = ToolBuilder::new("gamma")
        .description("c")
        .handler(|_input: AddInput| async { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new()
        .server_info("test", "1.0")
        .page_size(2)
        .tool(tool_a)
        .tool(tool_b)
        .tool(tool_c);

    init_router(&mut router).await;

    // First page
    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ListTools(ListToolsParams {
            cursor: None,
            meta: None,
        }),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    let (tools, next_cursor) = match resp.inner {
        Ok(McpResponse::ListTools(result)) => (result.tools, result.next_cursor),
        other => panic!("Expected ListTools, got {:?}", other),
    };
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "alpha");
    assert_eq!(tools[1].name, "beta");
    assert!(next_cursor.is_some());

    // Second page
    let req = RouterRequest {
        id: RequestId::Number(2),
        inner: McpRequest::ListTools(ListToolsParams {
            cursor: next_cursor,
            meta: None,
        }),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    let (tools, next_cursor) = match resp.inner {
        Ok(McpResponse::ListTools(result)) => (result.tools, result.next_cursor),
        other => panic!("Expected ListTools, got {:?}", other),
    };
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "gamma");
    assert!(next_cursor.is_none());
}

#[tokio::test]
async fn test_list_tools_no_pagination_by_default() {
    let tool_a = ToolBuilder::new("alpha")
        .description("a")
        .handler(|_input: AddInput| async { Ok(CallToolResult::text("ok")) })
        .build();
    let tool_b = ToolBuilder::new("beta")
        .description("b")
        .handler(|_input: AddInput| async { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new()
        .server_info("test", "1.0")
        .tool(tool_a)
        .tool(tool_b);

    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ListTools(ListToolsParams {
            cursor: None,
            meta: None,
        }),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    match resp.inner {
        Ok(McpResponse::ListTools(result)) => {
            assert_eq!(result.tools.len(), 2);
            assert!(result.next_cursor.is_none());
        }
        other => panic!("Expected ListTools, got {:?}", other),
    }
}

// =========================================================================
// Dynamic Tool Registry Tests
// =========================================================================

#[cfg(feature = "dynamic-tools")]
mod dynamic_tools_tests {
    use super::*;

    #[tokio::test]
    async fn test_dynamic_tools_register_and_list() {
        let (router, registry) = McpRouter::new()
            .server_info("test", "1.0")
            .with_dynamic_tools();

        let tool = ToolBuilder::new("dynamic_echo")
            .description("Dynamic echo")
            .handler(
                |input: AddInput| async move { Ok(CallToolResult::text(format!("{}", input.a))) },
            )
            .build();

        registry.register(tool);

        let mut router = router;
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(ListToolsParams::default()),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        match resp.inner {
            Ok(McpResponse::ListTools(result)) => {
                assert_eq!(result.tools.len(), 1);
                assert_eq!(result.tools[0].name, "dynamic_echo");
            }
            _ => panic!("Expected ListTools response"),
        }
    }

    #[tokio::test]
    async fn test_dynamic_tools_unregister() {
        let (router, registry) = McpRouter::new()
            .server_info("test", "1.0")
            .with_dynamic_tools();

        let tool = ToolBuilder::new("temp")
            .description("Temporary")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();

        registry.register(tool);
        assert!(registry.contains("temp"));

        let removed = registry.unregister("temp");
        assert!(removed);
        assert!(!registry.contains("temp"));

        // Unregistering again returns false
        assert!(!registry.unregister("temp"));

        let mut router = router;
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(ListToolsParams::default()),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        match resp.inner {
            Ok(McpResponse::ListTools(result)) => {
                assert_eq!(result.tools.len(), 0);
            }
            _ => panic!("Expected ListTools response"),
        }
    }

    #[tokio::test]
    async fn test_dynamic_tools_merged_with_static() {
        let static_tool = ToolBuilder::new("static_tool")
            .description("Static")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("static")) })
            .build();

        let (router, registry) = McpRouter::new()
            .server_info("test", "1.0")
            .tool(static_tool)
            .with_dynamic_tools();

        let dynamic_tool = ToolBuilder::new("dynamic_tool")
            .description("Dynamic")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("dynamic")) })
            .build();

        registry.register(dynamic_tool);

        let mut router = router;
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(ListToolsParams::default()),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        match resp.inner {
            Ok(McpResponse::ListTools(result)) => {
                assert_eq!(result.tools.len(), 2);
                let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
                assert!(names.contains(&"static_tool"));
                assert!(names.contains(&"dynamic_tool"));
            }
            _ => panic!("Expected ListTools response"),
        }
    }

    #[tokio::test]
    async fn test_static_tools_shadow_dynamic() {
        let static_tool = ToolBuilder::new("shared")
            .description("Static version")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("static")) })
            .build();

        let (router, registry) = McpRouter::new()
            .server_info("test", "1.0")
            .tool(static_tool)
            .with_dynamic_tools();

        let dynamic_tool = ToolBuilder::new("shared")
            .description("Dynamic version")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("dynamic")) })
            .build();

        registry.register(dynamic_tool);

        let mut router = router;
        init_router(&mut router).await;

        // List should only show the static version
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(ListToolsParams::default()),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        match resp.inner {
            Ok(McpResponse::ListTools(result)) => {
                assert_eq!(result.tools.len(), 1);
                assert_eq!(result.tools[0].name, "shared");
                assert_eq!(
                    result.tools[0].description.as_deref(),
                    Some("Static version")
                );
            }
            _ => panic!("Expected ListTools response"),
        }

        // Call should dispatch to the static tool
        let req = RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::CallTool(CallToolParams {
                input_responses: None,
                request_state: None,
                name: "shared".to_string(),
                arguments: serde_json::json!({"a": 1, "b": 2}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        match resp.inner {
            Ok(McpResponse::CallTool(result)) => {
                assert!(!result.is_error);
                match &result.content[0] {
                    Content::Text { text, .. } => assert_eq!(text, "static"),
                    _ => panic!("Expected text content"),
                }
            }
            _ => panic!("Expected CallTool response"),
        }
    }

    #[tokio::test]
    async fn test_dynamic_tools_call() {
        let (router, registry) = McpRouter::new()
            .server_info("test", "1.0")
            .with_dynamic_tools();

        let tool = ToolBuilder::new("add")
            .description("Add two numbers")
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build();

        registry.register(tool);

        let mut router = router;
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                input_responses: None,
                request_state: None,
                name: "add".to_string(),
                arguments: serde_json::json!({"a": 3, "b": 4}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        match resp.inner {
            Ok(McpResponse::CallTool(result)) => {
                assert!(!result.is_error);
                match &result.content[0] {
                    Content::Text { text, .. } => assert_eq!(text, "7"),
                    _ => panic!("Expected text content"),
                }
            }
            _ => panic!("Expected CallTool response"),
        }
    }

    #[tokio::test]
    async fn test_dynamic_tools_notification_on_register() {
        let (tx, mut rx) = crate::context::notification_channel(16);
        let (router, registry) = McpRouter::new()
            .server_info("test", "1.0")
            .with_dynamic_tools();
        let _router = router.with_notification_sender(tx);

        let tool = ToolBuilder::new("notified")
            .description("Test")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();

        registry.register(tool);

        let notification = rx.recv().await.unwrap();
        assert!(matches!(notification, ServerNotification::ToolsListChanged));
    }

    #[tokio::test]
    async fn test_dynamic_tools_notification_on_unregister() {
        let (tx, mut rx) = crate::context::notification_channel(16);
        let (router, registry) = McpRouter::new()
            .server_info("test", "1.0")
            .with_dynamic_tools();
        let _router = router.with_notification_sender(tx);

        let tool = ToolBuilder::new("notified")
            .description("Test")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();

        registry.register(tool);
        // Consume the register notification
        let _ = rx.recv().await.unwrap();

        registry.unregister("notified");
        let notification = rx.recv().await.unwrap();
        assert!(matches!(notification, ServerNotification::ToolsListChanged));
    }

    #[tokio::test]
    async fn test_dynamic_tools_no_notification_on_empty_unregister() {
        let (tx, mut rx) = crate::context::notification_channel(16);
        let (router, registry) = McpRouter::new()
            .server_info("test", "1.0")
            .with_dynamic_tools();
        let _router = router.with_notification_sender(tx);

        // Unregister a tool that doesn't exist — should NOT send notification
        assert!(!registry.unregister("nonexistent"));

        // Channel should be empty
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_dynamic_tools_filter_applies() {
        use crate::filter::CapabilityFilter;

        let (router, registry) = McpRouter::new()
            .server_info("test", "1.0")
            .tool_filter(CapabilityFilter::new(|_, tool: &Tool| {
                tool.name != "hidden"
            }))
            .with_dynamic_tools();

        let visible = ToolBuilder::new("visible")
            .description("Visible")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();

        let hidden = ToolBuilder::new("hidden")
            .description("Hidden")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();

        registry.register(visible);
        registry.register(hidden);

        let mut router = router;
        init_router(&mut router).await;

        // List should only show visible tool
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(ListToolsParams::default()),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        match resp.inner {
            Ok(McpResponse::ListTools(result)) => {
                assert_eq!(result.tools.len(), 1);
                assert_eq!(result.tools[0].name, "visible");
            }
            _ => panic!("Expected ListTools response"),
        }

        // Call to hidden tool should be denied
        let req = RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::CallTool(CallToolParams {
                input_responses: None,
                request_state: None,
                name: "hidden".to_string(),
                arguments: serde_json::json!({"a": 1, "b": 2}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        match resp.inner {
            Err(e) => {
                assert_eq!(e.code, -32601); // Method not found
            }
            _ => panic!("Expected JsonRpc error"),
        }
    }

    #[tokio::test]
    async fn test_dynamic_tools_capabilities_advertised() {
        // No static tools, but dynamic tools enabled — should advertise tools capability
        let (mut router, _registry) = McpRouter::new()
            .server_info("test", "1.0")
            .with_dynamic_tools();

        let init_req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::Initialize(InitializeParams {
                protocol_version: "2025-11-25".to_string(),
                capabilities: ClientCapabilities::default(),
                client_info: Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
                meta: None,
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(init_req).await.unwrap();
        match resp.inner {
            Ok(McpResponse::Initialize(result)) => {
                assert!(result.capabilities.tools.is_some());
            }
            _ => panic!("Expected Initialize response"),
        }
    }

    #[tokio::test]
    async fn test_dynamic_tools_multi_session_notification() {
        let (tx1, mut rx1) = crate::context::notification_channel(16);
        let (tx2, mut rx2) = crate::context::notification_channel(16);

        let (router, registry) = McpRouter::new()
            .server_info("test", "1.0")
            .with_dynamic_tools();

        // Simulate two sessions by calling with_notification_sender on two clones
        let _session1 = router.clone().with_notification_sender(tx1);
        let _session2 = router.clone().with_notification_sender(tx2);

        let tool = ToolBuilder::new("broadcast")
            .description("Test")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();

        registry.register(tool);

        // Both sessions should receive the notification
        let n1 = rx1.recv().await.unwrap();
        let n2 = rx2.recv().await.unwrap();
        assert!(matches!(n1, ServerNotification::ToolsListChanged));
        assert!(matches!(n2, ServerNotification::ToolsListChanged));
    }

    #[tokio::test]
    async fn test_dynamic_tools_call_not_found() {
        let (router, _registry) = McpRouter::new()
            .server_info("test", "1.0")
            .with_dynamic_tools();

        let mut router = router;
        init_router(&mut router).await;

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                input_responses: None,
                request_state: None,
                name: "nonexistent".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };

        let resp = router.ready().await.unwrap().call(req).await.unwrap();
        match resp.inner {
            Err(e) => {
                assert_eq!(e.code, -32601);
            }
            _ => panic!("Expected method not found error"),
        }
    }

    #[tokio::test]
    async fn test_dynamic_tools_registry_list() {
        let (_, registry) = McpRouter::new()
            .server_info("test", "1.0")
            .with_dynamic_tools();

        assert!(registry.list().is_empty());

        let tool = ToolBuilder::new("tool_a")
            .description("A")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();
        registry.register(tool);

        let tool = ToolBuilder::new("tool_b")
            .description("B")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build();
        registry.register(tool);

        let tools = registry.list();
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"tool_a"));
        assert!(names.contains(&"tool_b"));
    }
} // mod dynamic_tools_tests

#[tokio::test]
async fn test_tool_if_true_registers() {
    let tool = ToolBuilder::new("conditional")
        .description("Conditional tool")
        .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new().tool_if(true, tool);
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ListTools(ListToolsParams::default()),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    match resp.inner {
        Ok(McpResponse::ListTools(result)) => {
            assert_eq!(result.tools.len(), 1);
            assert_eq!(result.tools[0].name, "conditional");
        }
        _ => panic!("Expected ListTools response"),
    }
}

#[tokio::test]
async fn test_tool_if_false_skips() {
    let tool = ToolBuilder::new("conditional")
        .description("Conditional tool")
        .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
        .build();

    let mut router = McpRouter::new().tool_if(false, tool);
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ListTools(ListToolsParams::default()),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    match resp.inner {
        Ok(McpResponse::ListTools(result)) => {
            assert_eq!(result.tools.len(), 0);
        }
        _ => panic!("Expected ListTools response"),
    }
}

#[tokio::test]
async fn test_tools_if_batch_conditional() {
    let tools = vec![
        ToolBuilder::new("a")
            .description("Tool A")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build(),
        ToolBuilder::new("b")
            .description("Tool B")
            .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
            .build(),
    ];

    let mut router = McpRouter::new().tools_if(false, tools);
    init_router(&mut router).await;

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ListTools(ListToolsParams::default()),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    match resp.inner {
        Ok(McpResponse::ListTools(result)) => {
            assert_eq!(result.tools.len(), 0);
        }
        _ => panic!("Expected ListTools response"),
    }
}

#[test]
fn test_resource_if_true_registers() {
    let resource = crate::resource::ResourceBuilder::new("file:///test.txt")
        .name("test")
        .text("hello");

    let router = McpRouter::new().resource_if(true, resource);
    assert_eq!(router.inner.resources.len(), 1);
}

#[test]
fn test_resource_if_false_skips() {
    let resource = crate::resource::ResourceBuilder::new("file:///test.txt")
        .name("test")
        .text("hello");

    let router = McpRouter::new().resource_if(false, resource);
    assert_eq!(router.inner.resources.len(), 0);
}

#[test]
fn test_prompt_if_true_registers() {
    let prompt = crate::prompt::PromptBuilder::new("greet")
        .description("Greeting")
        .user_message("Hello!");

    let router = McpRouter::new().prompt_if(true, prompt);
    assert_eq!(router.inner.prompts.len(), 1);
}

#[test]
fn test_prompt_if_false_skips() {
    let prompt = crate::prompt::PromptBuilder::new("greet")
        .description("Greeting")
        .user_message("Hello!");

    let router = McpRouter::new().prompt_if(false, prompt);
    assert_eq!(router.inner.prompts.len(), 0);
}

#[tokio::test]
async fn test_disable_tool_hides_from_list() {
    let safe = ToolBuilder::new("safe")
        .description("Safe tool")
        .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
        .build();
    let dangerous = ToolBuilder::new("dangerous")
        .description("Dangerous tool")
        .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
        .build();
    let mut router = McpRouter::new().tool(safe).tool(dangerous);
    init_router(&mut router).await;

    router.disable_tool("dangerous");
    assert!(router.is_tool_enabled("safe"));
    assert!(!router.is_tool_enabled("dangerous"));

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::ListTools(ListToolsParams::default()),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    match resp.inner {
        Ok(McpResponse::ListTools(result)) => {
            let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
            assert_eq!(names, vec!["safe"]);
        }
        _ => panic!("Expected ListTools response"),
    }
}

#[tokio::test]
async fn test_disable_tool_blocks_call() {
    let dangerous = ToolBuilder::new("dangerous")
        .description("Dangerous tool")
        .handler(|_: AddInput| async { Ok(CallToolResult::text("ran")) })
        .build();
    let mut router = McpRouter::new().tool(dangerous);
    init_router(&mut router).await;

    router.disable_tool("dangerous");

    let req = RouterRequest {
        id: RequestId::Number(2),
        inner: McpRequest::CallTool(CallToolParams {
            input_responses: None,
            request_state: None,
            name: "dangerous".to_string(),
            arguments: serde_json::json!({"a": 1, "b": 2}),
            meta: None,
            task: None,
        }),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    let err = resp.inner.expect_err("disabled tool should error");
    assert_eq!(err.code, crate::error::ErrorCode::MethodNotFound as i32);
}

#[tokio::test]
async fn test_enable_tool_restores_visibility() {
    let tool = ToolBuilder::new("flippy")
        .description("Toggleable tool")
        .handler(|_: AddInput| async { Ok(CallToolResult::text("ran")) })
        .build();
    let mut router = McpRouter::new().tool(tool);
    init_router(&mut router).await;

    router.disable_tool("flippy");
    router.enable_tool("flippy");
    assert!(router.is_tool_enabled("flippy"));

    let req = RouterRequest {
        id: RequestId::Number(3),
        inner: McpRequest::CallTool(CallToolParams {
            input_responses: None,
            request_state: None,
            name: "flippy".to_string(),
            arguments: serde_json::json!({"a": 1, "b": 2}),
            meta: None,
            task: None,
        }),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    match resp.inner {
        Ok(McpResponse::CallTool(result)) => {
            assert_eq!(result.first_text(), Some("ran"));
        }
        _ => panic!("Expected CallTool response"),
    }
}

#[tokio::test]
async fn test_disable_propagates_through_fresh_session() {
    let tool = ToolBuilder::new("shared")
        .description("Shared across sessions")
        .handler(|_: AddInput| async { Ok(CallToolResult::text("ok")) })
        .build();
    let router = McpRouter::new().tool(tool);

    // Disable on the parent, observe via with_fresh_session clone.
    router.disable_tool("shared");
    let mut child = router.with_fresh_session();
    init_router(&mut child).await;
    assert!(!child.is_tool_enabled("shared"));

    let req = RouterRequest {
        id: RequestId::Number(4),
        inner: McpRequest::ListTools(ListToolsParams::default()),
        extensions: Extensions::new(),
    };
    let resp = child.ready().await.unwrap().call(req).await.unwrap();
    match resp.inner {
        Ok(McpResponse::ListTools(result)) => {
            assert!(result.tools.is_empty());
        }
        _ => panic!("Expected ListTools response"),
    }
}

#[tokio::test]
async fn test_disable_resource_and_prompt() {
    let resource = crate::resource::ResourceBuilder::new("file:///hidden.txt")
        .name("hidden")
        .text("secret");
    let prompt = crate::prompt::PromptBuilder::new("hidden_prompt")
        .description("hidden")
        .user_message("hello");

    let mut router = McpRouter::new().resource(resource).prompt(prompt);
    init_router(&mut router).await;

    router.disable_resource("file:///hidden.txt");
    router.disable_prompt("hidden_prompt");
    assert!(!router.is_resource_enabled("file:///hidden.txt"));
    assert!(!router.is_prompt_enabled("hidden_prompt"));

    // resources/list excludes
    let req = RouterRequest {
        id: RequestId::Number(5),
        inner: McpRequest::ListResources(ListResourcesParams::default()),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    match resp.inner {
        Ok(McpResponse::ListResources(result)) => {
            assert!(result.resources.is_empty());
        }
        _ => panic!("Expected ListResources response"),
    }

    // resources/read returns not found
    let req = RouterRequest {
        id: RequestId::Number(6),
        inner: McpRequest::ReadResource(ReadResourceParams {
            input_responses: None,
            request_state: None,
            uri: "file:///hidden.txt".to_string(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    let err = resp.inner.expect_err("disabled resource should error");
    assert_eq!(err.code, -32602); // SEP-2164: ResourceNotFound now uses InvalidParams

    // prompts/list excludes
    let req = RouterRequest {
        id: RequestId::Number(7),
        inner: McpRequest::ListPrompts(ListPromptsParams::default()),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    match resp.inner {
        Ok(McpResponse::ListPrompts(result)) => {
            assert!(result.prompts.is_empty());
        }
        _ => panic!("Expected ListPrompts response"),
    }

    // prompts/get returns not found
    let req = RouterRequest {
        id: RequestId::Number(8),
        inner: McpRequest::GetPrompt(GetPromptParams {
            input_responses: None,
            request_state: None,
            name: "hidden_prompt".to_string(),
            arguments: Default::default(),
            meta: None,
        }),
        extensions: Extensions::new(),
    };
    let resp = router.ready().await.unwrap().call(req).await.unwrap();
    let err = resp.inner.expect_err("disabled prompt should error");
    assert_eq!(err.code, crate::error::ErrorCode::MethodNotFound as i32);
}

#[test]
fn test_router_request_new() {
    let req = RouterRequest::new(RequestId::Number(1), McpRequest::Ping);
    assert_eq!(req.id, RequestId::Number(1));
    assert!(req.extensions.is_empty());
}

#[test]
fn test_with_inner_preserves_extensions() {
    let mut req = RouterRequest::new(RequestId::Number(1), McpRequest::Ping);
    req.extensions.insert(42u32);

    let rewritten = req.with_inner(McpRequest::ListTools(Default::default()));
    assert!(matches!(rewritten.inner, McpRequest::ListTools(_)));
    assert_eq!(rewritten.id, RequestId::Number(1));
    assert_eq!(rewritten.extensions.get::<u32>(), Some(&42));
}

#[test]
fn test_with_id_and_inner_preserves_extensions() {
    let mut req = RouterRequest::new(RequestId::Number(1), McpRequest::Ping);
    req.extensions.insert(String::from("token-abc"));

    let rewritten = req.with_id_and_inner(
        RequestId::Number(99),
        McpRequest::ListResources(Default::default()),
    );
    assert_eq!(rewritten.id, RequestId::Number(99));
    assert!(matches!(rewritten.inner, McpRequest::ListResources(_)));
    assert_eq!(
        rewritten.extensions.get::<String>(),
        Some(&String::from("token-abc"))
    );
}

#[test]
fn test_clone_with_inner_preserves_extensions() {
    let mut req = RouterRequest::new(RequestId::Number(1), McpRequest::Ping);
    req.extensions.insert(true);

    let cloned = req.clone_with_inner(McpRequest::ListTools(Default::default()));

    // Original still intact
    assert!(matches!(req.inner, McpRequest::Ping));
    assert_eq!(req.extensions.get::<bool>(), Some(&true));

    // Clone has new inner but same extensions
    assert!(matches!(cloned.inner, McpRequest::ListTools(_)));
    assert_eq!(cloned.extensions.get::<bool>(), Some(&true));
}

#[test]
fn test_router_response_is_error() {
    let ok_resp = RouterResponse {
        id: RequestId::Number(1),
        inner: Ok(McpResponse::Pong(Default::default())),
    };
    assert!(!ok_resp.is_error());

    let err_resp = RouterResponse {
        id: RequestId::Number(2),
        inner: Err(JsonRpcError::internal_error("boom")),
    };
    assert!(err_resp.is_error());
}

#[test]
fn test_extensions_len_and_is_empty() {
    let mut ext = Extensions::new();
    assert!(ext.is_empty());
    assert_eq!(ext.len(), 0);

    ext.insert(42u32);
    assert!(!ext.is_empty());
    assert_eq!(ext.len(), 1);

    ext.insert(String::from("hello"));
    assert_eq!(ext.len(), 2);
}

#[test]
fn test_router_response_serde_roundtrip() {
    // Success response
    let response = RouterResponse {
        id: RequestId::Number(1),
        inner: Ok(McpResponse::Empty(EmptyResult {})),
    };
    let json = serde_json::to_string(&response).unwrap();
    let deserialized: RouterResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.id, RequestId::Number(1));
    assert!(!deserialized.is_error());

    // Error response
    let response = RouterResponse {
        id: RequestId::String("req-2".into()),
        inner: Err(JsonRpcError::method_not_found("unknown")),
    };
    let json = serde_json::to_string(&response).unwrap();
    let deserialized: RouterResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.id, RequestId::String("req-2".into()));
    assert!(deserialized.is_error());
}

// =========================================================================
// Issue #872: McpRequest::Discover unit tests
// Unit tests that exercise the router dispatch directly via JsonRpcService,
// without going through the HTTP transport layer.
// =========================================================================

#[tokio::test]
async fn test_discover_dispatch_via_jsonrpc_service() {
    // server/discover must work without any prior initialize call.
    // The router does NOT require session initialization for this RPC.
    let router = McpRouter::new().server_info("unit-test-server", "4.2.0");
    let mut service = JsonRpcService::new(router);

    let req = JsonRpcRequest::new(1, "server/discover");
    let resp = service.call_single(req).await.unwrap();

    match resp {
        JsonRpcResponse::Result(r) => {
            // supportedVersions must be a non-empty array.
            let versions = r
                .result
                .get("supportedVersions")
                .and_then(|v| v.as_array())
                .expect("result.supportedVersions must be an array");
            assert!(!versions.is_empty(), "supportedVersions must not be empty");

            // Server identity lives in _meta, not the result body (SEP-2575 final).
            assert_eq!(
                r.result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "unit-test-server",
                "serverInfo.name must match configured value"
            );
            assert_eq!(
                r.result["_meta"]["io.modelcontextprotocol/serverInfo"]["version"], "4.2.0",
                "serverInfo.version must match configured value"
            );

            // server/discover must NOT include singular protocolVersion
            // (that field belongs to the initialize response shape).
            assert!(
                r.result.get("protocolVersion").is_none(),
                "server/discover must NOT include protocolVersion: {:?}",
                r.result
            );
        }
        JsonRpcResponse::Error(e) => panic!("Expected success, got error: {:?}", e),
        _ => panic!("unexpected response variant"),
    }
}

#[tokio::test]
async fn test_discover_does_not_require_initialization() {
    // server/discover works on a freshly created, un-initialized router.
    // No prior initialize call is made -- the session state is empty.
    let router = McpRouter::new().server_info("fresh-router", "1.0.0");
    let mut service = JsonRpcService::new(router);

    let req = JsonRpcRequest::new(2, "server/discover");
    let resp = service.call_single(req).await.unwrap();

    // Must succeed -- not return an error about missing session/initialization.
    assert!(
        !matches!(resp, JsonRpcResponse::Error(_)),
        "server/discover must not require initialization: {:?}",
        resp
    );
}

/// #1249: an expired task is distinguishable from a missing one, but only to
/// its owner. To anyone else both must answer exactly as an id that was never
/// issued does, or `tasks/get` becomes an existence oracle: probe ids, and the
/// ones that answer differently belong to somebody.
///
/// This drives the router rather than the store, which is the level the
/// authorization decision is made at. A store-level test cannot see this bug:
/// I wrote one first, installed an implementation that disclosed expiry before
/// checking ownership, and the store-level test passed anyway.
#[cfg(all(feature = "oauth", feature = "stateless"))]
#[tokio::test]
async fn expiry_is_disclosed_to_the_owner_and_to_nobody_else() {
    fn as_principal(subject: &str) -> Extensions {
        let mut extensions = tasks_client_extensions();
        extensions.insert(crate::oauth::token::TokenClaims {
            sub: Some(subject.to_string()),
            iss: None,
            aud: None,
            exp: None,
            scope: None,
            client_id: None,
            extra: HashMap::new(),
        });
        extensions
    }

    let store = std::sync::Arc::new(crate::async_task::MemoryTaskStore::new());
    let router = McpRouter::new()
        .task_store(store.clone())
        .tool(
            ToolBuilder::new("optional_task")
                .task_support(TaskSupportMode::Optional)
                .handler(|input: AddInput| async move {
                    Ok(CallToolResult::text(format!("{}", input.a + input.b)))
                })
                .build(),
        )
        .with_tasks();

    // Alice creates a task, then it expires.
    let McpResponse::FinalCreateTask(created) = router
        .handle(
            RequestId::Number(1),
            McpRequest::CallTool(CallToolParams {
                name: "optional_task".to_string(),
                arguments: serde_json::json!({"a": 1, "b": 2}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
            as_principal("alice"),
        )
        .await
        .unwrap()
    else {
        panic!("Expected a final create-task response");
    };
    let task_id = created.task.metadata.task_id.clone();
    assert!(store.set_ttl(&task_id, 1).await.unwrap());
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    let get = |id: String, who: Extensions| {
        let router = router.clone();
        async move {
            router
                .handle(
                    RequestId::Number(9),
                    McpRequest::GetTaskInfo(GetTaskInfoParams {
                        task_id: id,
                        meta: None,
                    }),
                    who,
                )
                .await
        }
    };

    // The owner is told it expired.
    let owner_saw = get(task_id.clone(), as_principal("alice")).await;
    let Err(crate::error::Error::JsonRpc(owner_error)) = owner_saw else {
        panic!("the owner must be told the task expired");
    };
    assert_eq!(
        owner_error.data,
        Some(serde_json::json!({ "reason": "task_expired" })),
        "the owner's error must carry the expiry discriminator"
    );

    // Nobody else can tell it from an id that was never issued. Comparing the
    // whole error, not just the code, because a discriminator in `data` or a
    // differing message would leak just as effectively.
    let stranger_saw = get(task_id.clone(), as_principal("bob")).await;
    let never_issued = get("never-issued".to_string(), as_principal("bob")).await;
    let (
        Err(crate::error::Error::JsonRpc(stranger_error)),
        Err(crate::error::Error::JsonRpc(missing_error)),
    ) = (stranger_saw, never_issued)
    else {
        panic!("both must be refused");
    };
    assert_eq!(
        stranger_error.code, missing_error.code,
        "an unauthorized caller must not learn the task exists"
    );
    assert_eq!(
        stranger_error.data, missing_error.data,
        "nor from the error data"
    );
    // The message embeds the id the caller supplied, which tells them nothing
    // they did not already know, so compare the shape with the id removed.
    assert_eq!(
        stranger_error.message.replace(&task_id, "<id>"),
        missing_error.message.replace("never-issued", "<id>"),
        "nor from the message, once the caller's own id is factored out"
    );
}

/// #1249: a task that expires between authorization and the operation is
/// reported to its owner as expired, not as one that never existed. The
/// operation returning nothing is not enough to conclude "missing"; resolving
/// a second time is what distinguishes the two.
#[cfg(all(feature = "oauth", feature = "stateless"))]
#[tokio::test]
async fn an_operation_that_finds_nothing_reports_expiry_to_the_owner() {
    fn as_principal(subject: &str) -> Extensions {
        let mut extensions = tasks_client_extensions();
        extensions.insert(crate::oauth::token::TokenClaims {
            sub: Some(subject.to_string()),
            iss: None,
            aud: None,
            exp: None,
            scope: None,
            client_id: None,
            extra: HashMap::new(),
        });
        extensions
    }

    let store = std::sync::Arc::new(crate::async_task::MemoryTaskStore::new());
    let router = McpRouter::new()
        .task_store(store.clone())
        .tool(
            ToolBuilder::new("optional_task")
                .task_support(TaskSupportMode::Optional)
                .handler(|input: AddInput| async move {
                    Ok(CallToolResult::text(format!("{}", input.a + input.b)))
                })
                .build(),
        )
        .with_tasks();

    let McpResponse::FinalCreateTask(created) = router
        .handle(
            RequestId::Number(1),
            McpRequest::CallTool(CallToolParams {
                name: "optional_task".to_string(),
                arguments: serde_json::json!({"a": 1, "b": 2}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
            as_principal("alice"),
        )
        .await
        .unwrap()
    else {
        panic!("Expected a final create-task response");
    };
    let task_id = created.task.metadata.task_id.clone();
    assert!(store.set_ttl(&task_id, 1).await.unwrap());
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    // Each of get, update, and cancel must reach the same conclusion.
    for (offset, request) in [
        McpRequest::GetTaskInfo(GetTaskInfoParams {
            task_id: task_id.clone(),
            meta: None,
        }),
        McpRequest::UpdateTask(UpdateTaskParams {
            task_id: task_id.clone(),
            input_responses: Default::default(),
            meta: None,
        }),
        McpRequest::CancelTask(CancelTaskParams {
            task_id: task_id.clone(),
            reason: None,
            meta: None,
        }),
    ]
    .into_iter()
    .enumerate()
    {
        let Err(crate::error::Error::JsonRpc(error)) = router
            .handle(
                RequestId::Number(10 + offset as i64),
                request,
                as_principal("alice"),
            )
            .await
        else {
            panic!("operation {offset} on an expired task must be refused");
        };
        assert_eq!(
            error.data,
            Some(serde_json::json!({ "reason": "task_expired" })),
            "operation {offset} must tell the owner the task expired"
        );
    }
}

/// #1249: a late or duplicate `tasks/update` against a task the store still
/// knows, and which has not expired, has nothing left to apply. That is an
/// acknowledgement, not a not-found, so a client retry is idempotent.
#[cfg(feature = "stateless")]
#[tokio::test]
async fn a_duplicate_update_on_a_terminal_task_is_acknowledged() {
    let store = std::sync::Arc::new(crate::async_task::MemoryTaskStore::new());
    let (task_id, _) = store
        .create_task("work", serde_json::json!({}), Some(60_000), None)
        .await
        .unwrap();
    // Terminal, so there is nothing an update could apply.
    assert!(
        store
            .complete_task(&task_id, CallToolResult::text("done"))
            .await
            .unwrap()
    );

    let router = McpRouter::new().task_store(store.clone()).with_tasks();
    let response = router
        .handle(
            RequestId::Number(1),
            McpRequest::UpdateTask(UpdateTaskParams {
                task_id: task_id.clone(),
                input_responses: Default::default(),
                meta: None,
            }),
            tasks_client_extensions(),
        )
        .await;

    // The ack's shape depends on the lifecycle; both are acknowledgements.
    assert!(
        matches!(
            response,
            Ok(McpResponse::UpdateTask(_)) | Ok(McpResponse::FinalTaskAck(_))
        ),
        "a known, unexpired task with nothing to apply is acknowledged, not \
         reported missing: {response:?}"
    );
}
