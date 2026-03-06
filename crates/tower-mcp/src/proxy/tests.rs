#[cfg(test)]
mod proxy_tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use serde_json::json;
    use tower::Layer;
    use tower::timeout::TimeoutLayer;
    use tower_service::Service;

    use crate::client::{ChannelTransport, McpClient};
    use crate::context::notification_channel;
    use crate::protocol::{McpRequest, McpResponse, RequestId};
    use crate::proxy::McpProxy;
    use crate::router::{Extensions, RouterRequest};
    use crate::{
        CallToolResult, GetPromptResult, McpRouter, PromptBuilder, ReadResourceResult,
        ResourceBuilder, ResourceContent, ResourceTemplateBuilder, ToolBuilder,
    };

    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema)]
    struct AddInput {
        a: i64,
        b: i64,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    struct EchoInput {
        message: String,
    }

    /// Create a "math" backend router with an `add` tool.
    fn math_router() -> McpRouter {
        let add = ToolBuilder::new("add")
            .description("Add two numbers")
            .handler(|input: AddInput| async move {
                Ok(CallToolResult::text(format!("{}", input.a + input.b)))
            })
            .build();

        McpRouter::new()
            .server_info("math-server", "1.0.0")
            .tool(add)
    }

    /// Create a "text" backend router with an `echo` tool and a `greet` prompt.
    fn text_router() -> McpRouter {
        let echo = ToolBuilder::new("echo")
            .description("Echo a message")
            .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.message)) })
            .build();

        let readme = ResourceBuilder::new("file:///README.md")
            .name("README")
            .description("Project readme")
            .text("# Hello");

        let file_template = ResourceTemplateBuilder::new("file:///{path}")
            .name("File Template")
            .description("Read a file by path")
            .handler(|uri: String, vars: HashMap<String, String>| async move {
                let path = vars.get("path").cloned().unwrap_or_default();
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri,
                        mime_type: Some("text/plain".to_string()),
                        text: Some(format!("contents of {}", path)),
                        blob: None,
                        meta: None,
                    }],
                    meta: None,
                })
            });

        let greet = PromptBuilder::new("greet")
            .description("Greet someone")
            .required_arg("name", "Name to greet")
            .handler(|args: HashMap<String, String>| async move {
                let name = args.get("name").map(|s| s.as_str()).unwrap_or("World");
                Ok(GetPromptResult::user_message(format!("Hello, {}!", name)))
            })
            .build();

        McpRouter::new()
            .server_info("text-server", "1.0.0")
            .tool(echo)
            .resource(readme)
            .resource_template(file_template)
            .prompt(greet)
    }

    /// Build a proxy with two in-process backends.
    async fn build_test_proxy() -> McpProxy {
        let math_transport = ChannelTransport::new(math_router());
        let text_transport = ChannelTransport::new(text_router());

        McpProxy::builder("test-proxy", "1.0.0")
            .backend("math", math_transport)
            .await
            .backend("text", text_transport)
            .await
            .build()
            .await
            .expect("proxy should build")
    }

    /// Helper: send an McpRequest through the proxy and get the McpResponse.
    async fn call_proxy(
        proxy: &mut McpProxy,
        request: McpRequest,
    ) -> Result<McpResponse, tower_mcp_types::JsonRpcError> {
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: request,
            extensions: Extensions::new(),
        };
        let resp = proxy.call(req).await.expect("infallible");
        resp.inner
    }

    // ========================================================================
    // Initialize
    // ========================================================================

    #[tokio::test]
    async fn test_proxy_initialize() {
        let mut proxy = build_test_proxy().await;

        let resp = call_proxy(
            &mut proxy,
            McpRequest::Initialize(crate::protocol::InitializeParams {
                protocol_version: "2025-11-25".to_string(),
                capabilities: Default::default(),
                client_info: crate::protocol::Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    title: None,
                    description: None,
                    icons: None,
                    website_url: None,
                    meta: None,
                },
                meta: None,
            }),
        )
        .await
        .expect("initialize should succeed");

        match resp {
            McpResponse::Initialize(init) => {
                assert_eq!(init.server_info.name, "test-proxy");
                assert_eq!(init.server_info.version, "1.0.0");
                assert!(init.capabilities.tools.is_some());
            }
            other => panic!("expected Initialize response, got: {:?}", other),
        }
    }

    // ========================================================================
    // List tools
    // ========================================================================

    #[tokio::test]
    async fn test_proxy_list_tools() {
        let mut proxy = build_test_proxy().await;

        let resp = call_proxy(&mut proxy, McpRequest::ListTools(Default::default()))
            .await
            .expect("list tools should succeed");

        match resp {
            McpResponse::ListTools(result) => {
                let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
                assert!(
                    names.contains(&"math_add"),
                    "expected math_add, got: {:?}",
                    names
                );
                assert!(
                    names.contains(&"text_echo"),
                    "expected text_echo, got: {:?}",
                    names
                );
                assert_eq!(result.tools.len(), 2);
            }
            other => panic!("expected ListTools response, got: {:?}", other),
        }
    }

    // ========================================================================
    // Call tool - routing
    // ========================================================================

    #[tokio::test]
    async fn test_proxy_call_tool_routes_to_correct_backend() {
        let mut proxy = build_test_proxy().await;

        // Call math_add
        let resp = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                name: "math_add".to_string(),
                arguments: json!({"a": 10, "b": 32}),
                meta: None,
                task: None,
            }),
        )
        .await
        .expect("call tool should succeed");

        match resp {
            McpResponse::CallTool(result) => {
                assert_eq!(result.all_text(), "42");
            }
            other => panic!("expected CallTool response, got: {:?}", other),
        }

        // Call text_echo
        let resp = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                name: "text_echo".to_string(),
                arguments: json!({"message": "hello"}),
                meta: None,
                task: None,
            }),
        )
        .await
        .expect("call tool should succeed");

        match resp {
            McpResponse::CallTool(result) => {
                assert_eq!(result.all_text(), "hello");
            }
            other => panic!("expected CallTool response, got: {:?}", other),
        }
    }

    // ========================================================================
    // Call tool - unknown tool
    // ========================================================================

    #[tokio::test]
    async fn test_proxy_call_unknown_tool_returns_error() {
        let mut proxy = build_test_proxy().await;

        let result = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                name: "nonexistent_tool".to_string(),
                arguments: json!({}),
                meta: None,
                task: None,
            }),
        )
        .await;

        assert!(result.is_err(), "should return error for unknown tool");
        let err = result.unwrap_err();
        assert!(err.message.contains("Unknown tool"));
    }

    // ========================================================================
    // List resources
    // ========================================================================

    #[tokio::test]
    async fn test_proxy_call_unknown_resource_returns_error() {
        let mut proxy = build_test_proxy().await;

        let result = call_proxy(
            &mut proxy,
            McpRequest::ReadResource(crate::protocol::ReadResourceParams {
                uri: "unknown://resource".to_string(),
                meta: None,
            }),
        )
        .await;

        assert!(result.is_err(), "should return error for unknown resource");
        let err = result.unwrap_err();
        assert!(
            err.message.contains("Unknown resource"),
            "error should mention unknown resource, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn test_proxy_call_unknown_prompt_returns_error() {
        let mut proxy = build_test_proxy().await;

        let result = call_proxy(
            &mut proxy,
            McpRequest::GetPrompt(crate::protocol::GetPromptParams {
                name: "nonexistent_prompt".to_string(),
                arguments: Default::default(),
                meta: None,
            }),
        )
        .await;

        assert!(result.is_err(), "should return error for unknown prompt");
        let err = result.unwrap_err();
        assert!(
            err.message.contains("Unknown prompt"),
            "error should mention unknown prompt, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn test_proxy_list_resources() {
        let mut proxy = build_test_proxy().await;

        let resp = call_proxy(&mut proxy, McpRequest::ListResources(Default::default()))
            .await
            .expect("list resources should succeed");

        match resp {
            McpResponse::ListResources(result) => {
                assert_eq!(result.resources.len(), 1);
                // URI should be namespaced
                assert!(
                    result.resources[0].uri.starts_with("text_"),
                    "expected text_ prefix on URI, got: {}",
                    result.resources[0].uri
                );
            }
            other => panic!("expected ListResources response, got: {:?}", other),
        }
    }

    // ========================================================================
    // Read resource - routing
    // ========================================================================

    #[tokio::test]
    async fn test_proxy_read_resource_routes_correctly() {
        let mut proxy = build_test_proxy().await;

        let resp = call_proxy(
            &mut proxy,
            McpRequest::ReadResource(crate::protocol::ReadResourceParams {
                uri: "text_file:///README.md".to_string(),
                meta: None,
            }),
        )
        .await
        .expect("read resource should succeed");

        match resp {
            McpResponse::ReadResource(result) => {
                assert_eq!(result.first_text(), Some("# Hello"));
            }
            other => panic!("expected ReadResource response, got: {:?}", other),
        }
    }

    // ========================================================================
    // List resource templates
    // ========================================================================

    #[tokio::test]
    async fn test_proxy_list_resource_templates() {
        let mut proxy = build_test_proxy().await;

        let resp = call_proxy(
            &mut proxy,
            McpRequest::ListResourceTemplates(Default::default()),
        )
        .await
        .expect("list resource templates should succeed");

        match resp {
            McpResponse::ListResourceTemplates(result) => {
                // text backend has one resource template
                assert_eq!(result.resource_templates.len(), 1);
                let template = &result.resource_templates[0];
                // Should be namespaced
                assert!(
                    template.name.starts_with("text_"),
                    "template name should be namespaced, got: {}",
                    template.name
                );
                assert!(
                    template.uri_template.starts_with("text_"),
                    "template URI should be namespaced, got: {}",
                    template.uri_template
                );
            }
            other => panic!("expected ListResourceTemplates, got: {:?}", other),
        }
    }

    // ========================================================================
    // List prompts
    // ========================================================================

    #[tokio::test]
    async fn test_proxy_list_prompts() {
        let mut proxy = build_test_proxy().await;

        let resp = call_proxy(&mut proxy, McpRequest::ListPrompts(Default::default()))
            .await
            .expect("list prompts should succeed");

        match resp {
            McpResponse::ListPrompts(result) => {
                assert_eq!(result.prompts.len(), 1);
                assert_eq!(result.prompts[0].name, "text_greet");
            }
            other => panic!("expected ListPrompts response, got: {:?}", other),
        }
    }

    // ========================================================================
    // Get prompt - routing
    // ========================================================================

    #[tokio::test]
    async fn test_proxy_get_prompt_routes_correctly() {
        let mut proxy = build_test_proxy().await;

        let resp = call_proxy(
            &mut proxy,
            McpRequest::GetPrompt(crate::protocol::GetPromptParams {
                name: "text_greet".to_string(),
                arguments: HashMap::from([("name".to_string(), "Alice".to_string())]),
                meta: None,
            }),
        )
        .await
        .expect("get prompt should succeed");

        match resp {
            McpResponse::GetPrompt(result) => {
                let text = result.first_message_text().unwrap();
                assert!(
                    text.contains("Alice"),
                    "expected Alice in prompt, got: {}",
                    text
                );
            }
            other => panic!("expected GetPrompt response, got: {:?}", other),
        }
    }

    // ========================================================================
    // Ping
    // ========================================================================

    #[tokio::test]
    async fn test_proxy_ping() {
        let mut proxy = build_test_proxy().await;

        let resp = call_proxy(&mut proxy, McpRequest::Ping)
            .await
            .expect("ping should succeed");

        assert!(matches!(resp, McpResponse::Pong(_)));
    }

    // ========================================================================
    // Custom separator
    // ========================================================================

    #[tokio::test]
    async fn test_proxy_custom_separator() {
        let math_transport = ChannelTransport::new(math_router());

        let mut proxy = McpProxy::builder("sep-proxy", "1.0.0")
            .separator(".")
            .backend("math", math_transport)
            .await
            .build()
            .await
            .expect("proxy should build");

        // List tools with dot separator
        let resp = call_proxy(&mut proxy, McpRequest::ListTools(Default::default()))
            .await
            .expect("list tools should succeed");

        match resp {
            McpResponse::ListTools(result) => {
                assert_eq!(result.tools[0].name, "math.add");
            }
            other => panic!("expected ListTools response, got: {:?}", other),
        }

        // Call tool with dot separator
        let resp = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                name: "math.add".to_string(),
                arguments: json!({"a": 1, "b": 2}),
                meta: None,
                task: None,
            }),
        )
        .await
        .expect("call tool should succeed");

        match resp {
            McpResponse::CallTool(result) => {
                assert_eq!(result.all_text(), "3");
            }
            other => panic!("expected CallTool response, got: {:?}", other),
        }
    }

    // ========================================================================
    // Builder validation
    // ========================================================================

    #[tokio::test]
    async fn test_proxy_builder_rejects_empty_backends() {
        let result = McpProxy::builder("empty", "1.0.0").build().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_proxy_builder_rejects_duplicate_namespaces() {
        let t1 = ChannelTransport::new(math_router());
        let t2 = ChannelTransport::new(text_router());

        let result = McpProxy::builder("dup", "1.0.0")
            .backend("same", t1)
            .await
            .backend("same", t2)
            .await
            .build()
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_proxy_unsupported_method_returns_error() {
        let mut proxy = build_test_proxy().await;

        let result = call_proxy(
            &mut proxy,
            McpRequest::SetLoggingLevel(crate::protocol::SetLogLevelParams {
                level: crate::protocol::LogLevel::Info,
                meta: None,
            }),
        )
        .await;

        assert!(result.is_err(), "unsupported method should return error");
        let err = result.unwrap_err();
        assert!(
            err.message.contains("not supported"),
            "error should mention not supported, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn test_proxy_backend_client_path() {
        let transport = ChannelTransport::new(math_router());
        let client = McpClient::connect(transport).await.expect("should connect");

        // Use backend_client (pre-connected McpClient) instead of backend (transport)
        let mut proxy = McpProxy::builder("client-proxy", "1.0.0")
            .backend_client("math", client)
            .build()
            .await
            .expect("proxy should build");

        // Should still be able to list and call tools
        let resp = call_proxy(&mut proxy, McpRequest::ListTools(Default::default()))
            .await
            .expect("list tools should succeed");

        match resp {
            McpResponse::ListTools(result) => {
                assert_eq!(result.tools.len(), 1);
                assert_eq!(result.tools[0].name, "math_add");
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }

        let resp = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                name: "math_add".to_string(),
                arguments: json!({"a": 5, "b": 7}),
                meta: None,
                task: None,
            }),
        )
        .await
        .expect("call tool should succeed");

        match resp {
            McpResponse::CallTool(result) => assert_eq!(result.all_text(), "12"),
            other => panic!("expected CallTool, got: {:?}", other),
        }
    }

    // ========================================================================
    // Clone
    // ========================================================================

    #[tokio::test]
    async fn test_proxy_is_clone() {
        let proxy = build_test_proxy().await;
        let mut proxy2 = proxy.clone();

        // Both should work independently
        let resp = call_proxy(&mut proxy2, McpRequest::Ping)
            .await
            .expect("ping should succeed");
        assert!(matches!(resp, McpResponse::Pong(_)));
    }

    // ========================================================================
    // ChannelTransport basics
    // ========================================================================

    #[tokio::test]
    async fn test_channel_transport_basic_roundtrip() {
        let router = math_router();
        let transport = ChannelTransport::new(router);

        let client = McpClient::connect(transport).await.expect("should connect");
        client
            .initialize("test-client", "1.0.0")
            .await
            .expect("should initialize");

        let tools = client.list_all_tools().await.expect("should list tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "add");

        let result = client
            .call_tool("add", json!({"a": 100, "b": 200}))
            .await
            .expect("should call tool");
        assert_eq!(result.all_text(), "300");
    }

    #[tokio::test]
    async fn test_channel_transport_with_resources_and_prompts() {
        let router = text_router();
        let transport = ChannelTransport::new(router);

        let client = McpClient::connect(transport).await.expect("should connect");
        client
            .initialize("test-client", "1.0.0")
            .await
            .expect("should initialize");

        // Tools
        let tools = client.list_all_tools().await.expect("should list tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");

        // Resources
        let resources = client
            .list_all_resources()
            .await
            .expect("should list resources");
        assert_eq!(resources.len(), 1);

        let content = client
            .read_resource("file:///README.md")
            .await
            .expect("should read resource");
        assert_eq!(content.first_text(), Some("# Hello"));

        // Prompts
        let prompts = client
            .list_all_prompts()
            .await
            .expect("should list prompts");
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].name, "greet");

        let prompt = client
            .get_prompt(
                "greet",
                Some(HashMap::from([("name".to_string(), "Bob".to_string())])),
            )
            .await
            .expect("should get prompt");
        assert!(prompt.first_message_text().unwrap().contains("Bob"));
    }

    /// A transport that immediately returns EOF, causing initialization to fail.
    struct BrokenTransport;

    #[async_trait::async_trait]
    impl crate::client::ClientTransport for BrokenTransport {
        async fn send(&mut self, _message: &str) -> crate::error::Result<()> {
            Err(crate::error::Error::internal("broken transport"))
        }

        async fn recv(&mut self) -> crate::error::Result<Option<String>> {
            Ok(None) // EOF
        }

        fn is_connected(&self) -> bool {
            false
        }

        async fn close(&mut self) -> crate::error::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_proxy_skips_failed_backend_initialization() {
        let good_transport = ChannelTransport::new(math_router());

        // Build with one broken and one good backend
        let mut proxy = McpProxy::builder("mixed-init", "1.0.0")
            .backend("broken", BrokenTransport)
            .await
            .backend("math", good_transport)
            .await
            .build()
            .await
            .expect("proxy should build with at least one good backend");

        // Only the math backend should be available
        let resp = call_proxy(&mut proxy, McpRequest::ListTools(Default::default()))
            .await
            .expect("list tools should succeed");

        match resp {
            McpResponse::ListTools(result) => {
                assert_eq!(result.tools.len(), 1);
                assert_eq!(result.tools[0].name, "math_add");
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }
    }

    // ========================================================================
    // Per-backend middleware
    // ========================================================================

    #[derive(Debug, Deserialize, JsonSchema)]
    struct SlowInput {
        delay_ms: u64,
    }

    /// Create a "slow" backend router with a tool that sleeps.
    fn slow_router() -> McpRouter {
        let slow = ToolBuilder::new("slow_op")
            .description("A slow operation")
            .handler(|input: SlowInput| async move {
                tokio::time::sleep(Duration::from_millis(input.delay_ms)).await;
                Ok(CallToolResult::text("done"))
            })
            .build();

        McpRouter::new()
            .server_info("slow-server", "1.0.0")
            .tool(slow)
    }

    #[tokio::test]
    async fn test_backend_layer_timeout_triggers() {
        let slow_transport = ChannelTransport::new(slow_router());

        let mut proxy = McpProxy::builder("timeout-proxy", "1.0.0")
            .backend("slow", slow_transport)
            .await
            .backend_layer(TimeoutLayer::new(Duration::from_millis(50)))
            .build()
            .await
            .expect("proxy should build");

        // Call with a delay that exceeds the timeout
        let result = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                name: "slow_slow_op".to_string(),
                arguments: json!({"delay_ms": 500}),
                meta: None,
                task: None,
            }),
        )
        .await;

        // Should get a JSON-RPC error from the CatchError wrapper
        assert!(result.is_err(), "should timeout");
        let err = result.unwrap_err();
        assert!(
            err.message.to_lowercase().contains("timed out")
                || err.message.to_lowercase().contains("timeout"),
            "error should mention timeout, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn test_backend_layer_timeout_allows_fast_requests() {
        let slow_transport = ChannelTransport::new(slow_router());

        let mut proxy = McpProxy::builder("timeout-proxy", "1.0.0")
            .backend("slow", slow_transport)
            .await
            .backend_layer(TimeoutLayer::new(Duration::from_secs(5)))
            .build()
            .await
            .expect("proxy should build");

        // Call with no delay -- should succeed
        let resp = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                name: "slow_slow_op".to_string(),
                arguments: json!({"delay_ms": 0}),
                meta: None,
                task: None,
            }),
        )
        .await
        .expect("fast call should succeed");

        match resp {
            McpResponse::CallTool(result) => {
                assert_eq!(result.all_text(), "done");
            }
            other => panic!("expected CallTool response, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_backend_layer_only_affects_target_backend() {
        let math_transport = ChannelTransport::new(math_router());
        let slow_transport = ChannelTransport::new(slow_router());

        let mut proxy = McpProxy::builder("mixed-proxy", "1.0.0")
            // math backend: no middleware
            .backend("math", math_transport)
            .await
            // slow backend: tight timeout
            .backend("slow", slow_transport)
            .await
            .backend_layer(TimeoutLayer::new(Duration::from_millis(50)))
            .build()
            .await
            .expect("proxy should build");

        // math backend should work fine (no timeout layer)
        let resp = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                name: "math_add".to_string(),
                arguments: json!({"a": 1, "b": 2}),
                meta: None,
                task: None,
            }),
        )
        .await
        .expect("math should succeed");

        match resp {
            McpResponse::CallTool(result) => assert_eq!(result.all_text(), "3"),
            other => panic!("expected CallTool, got: {:?}", other),
        }

        // slow backend should timeout
        let result = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                name: "slow_slow_op".to_string(),
                arguments: json!({"delay_ms": 500}),
                meta: None,
                task: None,
            }),
        )
        .await;

        assert!(result.is_err(), "slow backend should timeout");
    }

    // ========================================================================
    // Health check
    // ========================================================================

    #[tokio::test]
    async fn test_proxy_health_check_all_healthy() {
        let proxy = build_test_proxy().await;

        let health = proxy.health_check().await;
        assert_eq!(health.len(), 2);
        assert!(
            health.iter().all(|h| h.healthy),
            "all backends should be healthy"
        );

        let namespaces: Vec<&str> = health.iter().map(|h| h.namespace.as_str()).collect();
        assert!(namespaces.contains(&"math"));
        assert!(namespaces.contains(&"text"));
    }

    // ========================================================================
    // Notification forwarding
    // ========================================================================

    #[tokio::test]
    async fn test_proxy_notification_sender_configured() {
        let (notif_tx, _notif_rx) = notification_channel(32);
        let math_transport = ChannelTransport::new(math_router());

        let proxy = McpProxy::builder("notif-proxy", "1.0.0")
            .notification_sender(notif_tx)
            .backend("math", math_transport)
            .await
            .build()
            .await
            .expect("proxy should build");

        // Verify proxy built successfully with notification sender
        assert!(proxy.shared.notification_tx.is_some());
    }

    // ========================================================================
    // CoalesceLayer integration
    // ========================================================================

    #[tokio::test]
    async fn test_coalesce_layer_deduplicates_concurrent_list_tools() {
        use std::mem::discriminant;
        use tower::ServiceExt;
        use tower_resilience::coalesce::{CoalesceError, CoalesceLayer};

        type CoalesceResult =
            Result<crate::router::RouterResponse, CoalesceError<std::convert::Infallible>>;

        let proxy = build_test_proxy().await;

        // Wrap the proxy in CoalesceLayer keyed by McpRequest discriminant.
        // This means concurrent list_tools calls share a single execution.
        let coalesced =
            CoalesceLayer::new(|req: &RouterRequest| discriminant(&req.inner)).layer(proxy);

        // Fire 5 concurrent list_tools requests
        let mut handles = vec![];
        for _ in 0..5 {
            let mut svc = coalesced.clone();
            handles.push(tokio::spawn(async move {
                let req = RouterRequest {
                    id: RequestId::Number(1),
                    inner: McpRequest::ListTools(Default::default()),
                    extensions: Extensions::new(),
                };
                let result: CoalesceResult = svc.ready().await.unwrap().call(req).await;
                result
            }));
        }

        // All should succeed with identical tool lists
        let mut tool_lists: Vec<Vec<String>> = Vec::new();
        for handle in handles {
            let resp = handle.await.unwrap().unwrap();
            let mcp_resp = resp.inner.expect("should be Ok");
            match mcp_resp {
                McpResponse::ListTools(list) => {
                    let names: Vec<String> = list.tools.iter().map(|t| t.name.clone()).collect();
                    tool_lists.push(names);
                }
                other => panic!("expected ListTools, got: {:?}", other),
            }
        }

        // All responses should be identical (coalesced from the same execution)
        assert_eq!(tool_lists.len(), 5);
        for list in &tool_lists {
            assert_eq!(list, &tool_lists[0], "all responses should match");
        }
        // Should contain tools from both backends
        assert!(tool_lists[0].contains(&"math_add".to_string()));
        assert!(tool_lists[0].contains(&"text_echo".to_string()));
    }

    #[tokio::test]
    async fn test_coalesce_layer_does_not_affect_different_methods() {
        use std::mem::discriminant;
        use tower::ServiceExt;
        use tower_resilience::coalesce::{CoalesceError, CoalesceLayer};

        type CoalesceResult =
            Result<crate::router::RouterResponse, CoalesceError<std::convert::Infallible>>;

        let proxy = build_test_proxy().await;

        let coalesced =
            CoalesceLayer::new(|req: &RouterRequest| discriminant(&req.inner)).layer(proxy);

        // Fire list_tools and call_tool concurrently -- they have different
        // discriminants so they should NOT be coalesced.
        let mut list_svc = coalesced.clone();
        let mut call_svc = coalesced.clone();

        let list_handle: tokio::task::JoinHandle<CoalesceResult> = tokio::spawn(async move {
            let req = RouterRequest {
                id: RequestId::Number(1),
                inner: McpRequest::ListTools(Default::default()),
                extensions: Extensions::new(),
            };
            list_svc.ready().await.unwrap().call(req).await
        });

        let call_handle: tokio::task::JoinHandle<CoalesceResult> = tokio::spawn(async move {
            let req = RouterRequest {
                id: RequestId::Number(2),
                inner: McpRequest::CallTool(crate::protocol::CallToolParams {
                    name: "math_add".to_string(),
                    arguments: json!({"a": 10, "b": 20}),
                    meta: None,
                    task: None,
                }),
                extensions: Extensions::new(),
            };
            call_svc.ready().await.unwrap().call(req).await
        });

        let list_resp = list_handle.await.unwrap().unwrap();
        let call_resp = call_handle.await.unwrap().unwrap();

        // list_tools should return tool definitions
        match list_resp.inner.unwrap() {
            McpResponse::ListTools(list) => {
                assert!(!list.tools.is_empty());
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }

        // call_tool should return the computation result
        match call_resp.inner.unwrap() {
            McpResponse::CallTool(result) => {
                assert_eq!(result.all_text(), "30");
            }
            other => panic!("expected CallTool, got: {:?}", other),
        }
    }
}
