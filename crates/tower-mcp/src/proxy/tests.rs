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
    use crate::error::Error;
    use crate::protocol::{McpRequest, McpResponse, RequestId};
    use crate::proxy::{
        AddBackendError, BackendSetupFailures, BackendSetupOptions, McpProxy, SkippedPhase,
    };
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
                    ..Default::default()
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
            .build_strict()
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
                input_responses: None,
                request_state: None,
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
                input_responses: None,
                request_state: None,
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
                input_responses: None,
                request_state: None,
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
                input_responses: None,
                request_state: None,
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
                input_responses: None,
                request_state: None,
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
                input_responses: None,
                request_state: None,
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
                input_responses: None,
                request_state: None,
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
            .build_strict()
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
                input_responses: None,
                request_state: None,
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
    async fn test_proxy_builder_rejects_ambiguous_namespace_prefixes() {
        // With separator "_", "redis" produces prefix "redis_" and "redis_ft"
        // produces prefix "redis_ft_". Since "redis_ft_" starts with "redis_",
        // routing "redis_ft_search" is ambiguous.
        let t1 = ChannelTransport::new(math_router());
        let t2 = ChannelTransport::new(text_router());

        let result = McpProxy::builder("ambiguous", "1.0.0")
            .backend("redis", t1)
            .await
            .backend("redis_ft", t2)
            .await
            .build()
            .await;

        let err = result.err().expect("should fail with ambiguous prefixes");
        assert!(
            err.to_string().contains("Ambiguous namespace prefixes"),
            "Expected ambiguous prefix error, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_proxy_builder_allows_non_ambiguous_prefixes_with_dot_separator() {
        // With separator ".", "redis" -> "redis." and "redis_ft" -> "redis_ft."
        // Neither is a prefix of the other, so this is fine.
        let t1 = ChannelTransport::new(math_router());
        let t2 = ChannelTransport::new(text_router());

        let result = McpProxy::builder("dot-sep", "1.0.0")
            .separator(".")
            .backend("redis", t1)
            .await
            .backend("redis_ft", t2)
            .await
            .build()
            .await;

        assert!(result.is_ok());
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
            .build_strict()
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
                input_responses: None,
                request_state: None,
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
        let result = McpProxy::builder("mixed-init", "1.0.0")
            .backend("broken", BrokenTransport)
            .await
            .backend("math", good_transport)
            .await
            .build()
            .await
            .expect("proxy should build with at least one good backend");

        // The broken backend should be in the skipped list
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].namespace, "broken");
        let mut proxy = result.proxy;

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
            .build_strict()
            .await
            .expect("proxy should build");

        // Call with a delay that exceeds the timeout
        let result = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                input_responses: None,
                request_state: None,
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
            .build_strict()
            .await
            .expect("proxy should build");

        // Call with no delay -- should succeed
        let resp = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                input_responses: None,
                request_state: None,
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

    /// Records which layers a request passed through, in order.
    #[derive(Clone)]
    struct MarkerLayer {
        tag: &'static str,
        calls: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    #[derive(Clone)]
    struct MarkerService<S> {
        inner: S,
        tag: &'static str,
        calls: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    impl<S> Layer<S> for MarkerLayer {
        type Service = MarkerService<S>;

        fn layer(&self, inner: S) -> Self::Service {
            MarkerService {
                inner,
                tag: self.tag,
                calls: self.calls.clone(),
            }
        }
    }

    impl<S> Service<RouterRequest> for MarkerService<S>
    where
        S: Service<RouterRequest>,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = S::Future;

        fn poll_ready(
            &mut self,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

        fn call(&mut self, request: RouterRequest) -> Self::Future {
            self.calls.lock().unwrap().push(self.tag);
            self.inner.call(request)
        }
    }

    /// Repeated `backend_layer` calls must stack rather than each replacing
    /// the previous composition (#1173).
    #[tokio::test]
    async fn test_backend_layer_calls_stack() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let math_transport = ChannelTransport::new(math_router());

        let mut proxy = McpProxy::builder("stack-proxy", "1.0.0")
            .backend("math", math_transport)
            .await
            .backend_layer(MarkerLayer {
                tag: "first",
                calls: calls.clone(),
            })
            .backend_layer(MarkerLayer {
                tag: "second",
                calls: calls.clone(),
            })
            .build_strict()
            .await
            .expect("proxy should build");

        let resp = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                input_responses: None,
                request_state: None,
                name: "math_add".to_string(),
                arguments: json!({"a": 2, "b": 3}),
                meta: None,
                task: None,
            }),
        )
        .await
        .expect("layered call should succeed");

        match resp {
            McpResponse::CallTool(result) => assert_eq!(result.all_text(), "5"),
            other => panic!("expected CallTool response, got: {:?}", other),
        }

        // Both layers fired, and the layer added last saw the request first.
        assert_eq!(*calls.lock().unwrap(), vec!["second", "first"]);
    }

    /// Delays every call before delegating, standing in for the latency
    /// injection that exposed #1173 downstream.
    #[derive(Clone)]
    struct DelayLayer(Duration);

    #[derive(Clone)]
    struct DelayService<S> {
        inner: S,
        delay: Duration,
    }

    impl<S> Layer<S> for DelayLayer {
        type Service = DelayService<S>;

        fn layer(&self, inner: S) -> Self::Service {
            DelayService {
                inner,
                delay: self.0,
            }
        }
    }

    impl<S> Service<RouterRequest> for DelayService<S>
    where
        S: Service<RouterRequest> + Clone + Send + 'static,
        S::Future: Send,
        RouterRequest: Send,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<S::Response, S::Error>> + Send>,
        >;

        fn poll_ready(
            &mut self,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

        fn call(&mut self, request: RouterRequest) -> Self::Future {
            let delay = self.delay;
            let mut inner = self.inner.clone();
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                inner.call(request).await
            })
        }
    }

    /// A timeout added after a latency layer must observe that latency: with
    /// the pre-#1173 behavior the timeout replaced the delay and a stacked
    /// 10ms budget let a 100ms path succeed.
    #[tokio::test]
    async fn test_backend_layer_timeout_observes_earlier_layer() {
        let math_transport = ChannelTransport::new(math_router());

        let mut proxy = McpProxy::builder("delay-timeout-proxy", "1.0.0")
            .backend("math", math_transport)
            .await
            .backend_layer(DelayLayer(Duration::from_millis(100)))
            .backend_layer(TimeoutLayer::new(Duration::from_millis(10)))
            .build_strict()
            .await
            .expect("proxy should build");

        let result = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                input_responses: None,
                request_state: None,
                name: "math_add".to_string(),
                arguments: json!({"a": 2, "b": 3}),
                meta: None,
                task: None,
            }),
        )
        .await;

        let err = result.expect_err("the timeout must observe the injected delay");
        assert!(
            err.message.to_lowercase().contains("timed out")
                || err.message.to_lowercase().contains("timeout"),
            "error should mention timeout, got: {}",
            err.message
        );
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
            .build_strict()
            .await
            .expect("proxy should build");

        // math backend should work fine (no timeout layer)
        let resp = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                input_responses: None,
                request_state: None,
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
                input_responses: None,
                request_state: None,
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
            .build_strict()
            .await
            .expect("proxy should build");

        // Verify proxy built successfully with notification sender
        assert!(proxy.shared.notification_tx.is_some());
    }

    // ========================================================================
    // Instructions aggregation (#605)
    // ========================================================================

    #[tokio::test]
    async fn test_proxy_aggregates_backend_instructions() {
        // Create routers with instructions
        let math = McpRouter::new()
            .server_info("math-server", "1.0.0")
            .instructions("Provides arithmetic operations")
            .tool(
                ToolBuilder::new("add")
                    .description("Add two numbers")
                    .handler(|input: AddInput| async move {
                        Ok(CallToolResult::text(format!("{}", input.a + input.b)))
                    })
                    .build(),
            );

        let text = McpRouter::new()
            .server_info("text-server", "1.0.0")
            .instructions("Provides text manipulation tools")
            .tool(
                ToolBuilder::new("echo")
                    .description("Echo a message")
                    .handler(
                        |input: EchoInput| async move { Ok(CallToolResult::text(input.message)) },
                    )
                    .build(),
            );

        let math_transport = ChannelTransport::new(math);
        let text_transport = ChannelTransport::new(text);

        let mut proxy = McpProxy::builder("instructions-proxy", "1.0.0")
            .backend("math", math_transport)
            .await
            .backend("text", text_transport)
            .await
            .build_strict()
            .await
            .expect("proxy should build");

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::Initialize(crate::protocol::InitializeParams {
                protocol_version: "2025-11-25".to_string(),
                capabilities: Default::default(),
                client_info: crate::protocol::Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
                meta: None,
            }),
            extensions: Extensions::new(),
        };

        let resp = proxy.call(req).await.unwrap();
        let result = resp.inner.unwrap();

        if let McpResponse::Initialize(init) = result {
            let instructions = init.instructions.expect("should have instructions");
            assert!(
                instructions.contains("[math] Provides arithmetic operations"),
                "should contain math instructions: {instructions}"
            );
            assert!(
                instructions.contains("[text] Provides text manipulation tools"),
                "should contain text instructions: {instructions}"
            );
        } else {
            panic!("expected Initialize response");
        }
    }

    #[tokio::test]
    async fn test_proxy_custom_instructions_override() {
        let math_transport = ChannelTransport::new(math_router());

        let mut proxy = McpProxy::builder("custom-proxy", "1.0.0")
            .instructions("Custom proxy description")
            .backend("math", math_transport)
            .await
            .build_strict()
            .await
            .expect("proxy should build");

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::Initialize(crate::protocol::InitializeParams {
                protocol_version: "2025-11-25".to_string(),
                capabilities: Default::default(),
                client_info: crate::protocol::Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
                meta: None,
            }),
            extensions: Extensions::new(),
        };

        let resp = proxy.call(req).await.unwrap();
        let result = resp.inner.unwrap();

        if let McpResponse::Initialize(init) = result {
            assert_eq!(
                init.instructions.as_deref(),
                Some("Custom proxy description")
            );
        } else {
            panic!("expected Initialize response");
        }
    }

    #[tokio::test]
    async fn test_proxy_no_backend_instructions_gives_default() {
        // math_router doesn't set instructions
        let math_transport = ChannelTransport::new(math_router());

        let mut proxy = McpProxy::builder("default-proxy", "1.0.0")
            .backend("math", math_transport)
            .await
            .build_strict()
            .await
            .expect("proxy should build");

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::Initialize(crate::protocol::InitializeParams {
                protocol_version: "2025-11-25".to_string(),
                capabilities: Default::default(),
                client_info: crate::protocol::Implementation {
                    name: "test".to_string(),
                    version: "1.0".to_string(),
                    ..Default::default()
                },
                meta: None,
            }),
            extensions: Extensions::new(),
        };

        let resp = proxy.call(req).await.unwrap();
        let result = resp.inner.unwrap();

        if let McpResponse::Initialize(init) = result {
            let instructions = init.instructions.expect("should have instructions");
            assert_eq!(instructions, "MCP proxy aggregating 1 backend servers.");
            // Should NOT contain any [namespace] section
            assert!(!instructions.contains('['));
        } else {
            panic!("expected Initialize response");
        }
    }

    // ========================================================================
    // CoalesceLayer integration
    // ========================================================================

    // ========================================================================
    // ConcurrencyLimit integration (#604)
    // ========================================================================

    #[tokio::test]
    async fn test_backend_concurrency_limit_serializes_requests() {
        use tower::limit::ConcurrencyLimitLayer;

        let slow_transport = ChannelTransport::new(slow_router());

        let proxy = McpProxy::builder("concurrency-proxy", "1.0.0")
            .backend("slow", slow_transport)
            .await
            .backend_layer(ConcurrencyLimitLayer::new(1))
            .build_strict()
            .await
            .expect("proxy should build");

        // Fire two concurrent requests. With ConcurrencyLimitLayer(1),
        // they should be serialized (second waits for first to complete).
        // Both should succeed -- backpressure is handled internally by
        // BoxCloneService which calls poll_ready before dispatch.
        let req1 = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(crate::protocol::CallToolParams {
                input_responses: None,
                request_state: None,
                name: "slow_slow_op".to_string(),
                arguments: json!({"delay_ms": 10}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };
        let req2 = RouterRequest {
            id: RequestId::Number(2),
            inner: McpRequest::CallTool(crate::protocol::CallToolParams {
                input_responses: None,
                request_state: None,
                name: "slow_slow_op".to_string(),
                arguments: json!({"delay_ms": 10}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };

        let mut p1 = proxy.clone();
        let mut p2 = proxy.clone();
        let h1 = tokio::spawn(async move { p1.call(req1).await });
        let h2 = tokio::spawn(async move { p2.call(req2).await });

        let r1 = h1.await.unwrap().unwrap();
        let r2 = h2.await.unwrap().unwrap();

        // Both should succeed
        assert!(r1.inner.is_ok(), "first request should succeed");
        assert!(r2.inner.is_ok(), "second request should succeed");
    }

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

    // ========================================================================
    // Dynamic backend addition (#628)
    // ========================================================================

    #[tokio::test]
    async fn test_add_backend_dynamically() {
        // Start with just the math backend
        let math_transport = ChannelTransport::new(math_router());
        let mut proxy = McpProxy::builder("dynamic-proxy", "1.0.0")
            .backend("math", math_transport)
            .await
            .build_strict()
            .await
            .expect("proxy should build");

        // Initially only 1 backend with 1 tool
        assert_eq!(proxy.backend_count(), 1);
        let resp = call_proxy(&mut proxy, McpRequest::ListTools(Default::default()))
            .await
            .expect("list tools should succeed");
        match &resp {
            McpResponse::ListTools(result) => assert_eq!(result.tools.len(), 1),
            other => panic!("expected ListTools, got: {:?}", other),
        }

        // Add text backend dynamically
        let text_transport = ChannelTransport::new(text_router());
        proxy
            .add_backend("text", text_transport)
            .await
            .expect("add_backend should succeed");

        // Now 2 backends
        assert_eq!(proxy.backend_count(), 2);

        // List tools should show tools from both backends
        let resp = call_proxy(&mut proxy, McpRequest::ListTools(Default::default()))
            .await
            .expect("list tools should succeed");
        match resp {
            McpResponse::ListTools(result) => {
                assert_eq!(result.tools.len(), 2);
                let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
                assert!(names.contains(&"math_add"));
                assert!(names.contains(&"text_echo"));
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }

        // Call tool on the dynamically-added backend
        let resp = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                input_responses: None,
                request_state: None,
                name: "text_echo".to_string(),
                arguments: json!({"message": "dynamic!"}),
                meta: None,
                task: None,
            }),
        )
        .await
        .expect("call tool should succeed");

        match resp {
            McpResponse::CallTool(result) => assert_eq!(result.all_text(), "dynamic!"),
            other => panic!("expected CallTool, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_add_backend_visible_to_clones() {
        let math_transport = ChannelTransport::new(math_router());
        let proxy = McpProxy::builder("clone-proxy", "1.0.0")
            .backend("math", math_transport)
            .await
            .build_strict()
            .await
            .expect("proxy should build");

        // Clone the proxy before adding a backend
        let mut clone = proxy.clone();

        // Add text backend to the original
        let text_transport = ChannelTransport::new(text_router());
        proxy
            .add_backend("text", text_transport)
            .await
            .expect("add_backend should succeed");

        // The clone should also see the new backend (shared entries)
        assert_eq!(clone.backend_count(), 2);

        let resp = call_proxy(&mut clone, McpRequest::ListTools(Default::default()))
            .await
            .expect("list tools should succeed");
        match resp {
            McpResponse::ListTools(result) => {
                assert_eq!(result.tools.len(), 2);
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_add_backend_rejects_duplicate_namespace() {
        let math_transport = ChannelTransport::new(math_router());
        let proxy = McpProxy::builder("dup-proxy", "1.0.0")
            .backend("math", math_transport)
            .await
            .build_strict()
            .await
            .expect("proxy should build");

        let text_transport = ChannelTransport::new(text_router());
        let result = proxy.add_backend("math", text_transport).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            crate::proxy::AddBackendError::DuplicateNamespace(ns) => {
                assert_eq!(ns, "math");
            }
            other => panic!("expected DuplicateNamespace, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_add_backend_rejects_ambiguous_prefix() {
        let t1 = ChannelTransport::new(math_router());
        let proxy = McpProxy::builder("ambig-proxy", "1.0.0")
            .backend("redis", t1)
            .await
            .build_strict()
            .await
            .expect("proxy should build");

        let t2 = ChannelTransport::new(text_router());
        let result = proxy.add_backend("redis_ft", t2).await;
        assert!(result.is_err());
        assert!(
            matches!(
                result.unwrap_err(),
                crate::proxy::AddBackendError::AmbiguousPrefix { .. }
            ),
            "should be AmbiguousPrefix error"
        );
    }

    #[tokio::test]
    async fn test_add_backend_with_layer() {
        let math_transport = ChannelTransport::new(math_router());
        let mut proxy = McpProxy::builder("layered-proxy", "1.0.0")
            .backend("math", math_transport)
            .await
            .build_strict()
            .await
            .expect("proxy should build");

        // Add slow backend with a tight timeout
        let slow_transport = ChannelTransport::new(slow_router());
        proxy
            .add_backend_with_layer(
                "slow",
                slow_transport,
                TimeoutLayer::new(Duration::from_millis(50)),
            )
            .await
            .expect("add_backend_with_layer should succeed");

        // The slow backend should timeout
        let result = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                input_responses: None,
                request_state: None,
                name: "slow_slow_op".to_string(),
                arguments: json!({"delay_ms": 500}),
                meta: None,
                task: None,
            }),
        )
        .await;

        assert!(result.is_err(), "should timeout");

        // The math backend should still work
        let resp = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                input_responses: None,
                request_state: None,
                name: "math_add".to_string(),
                arguments: json!({"a": 3, "b": 4}),
                meta: None,
                task: None,
            }),
        )
        .await
        .expect("math should succeed");

        match resp {
            McpResponse::CallTool(result) => assert_eq!(result.all_text(), "7"),
            other => panic!("expected CallTool, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_add_backend_health_check_includes_new_backend() {
        let math_transport = ChannelTransport::new(math_router());
        let proxy = McpProxy::builder("health-proxy", "1.0.0")
            .backend("math", math_transport)
            .await
            .build_strict()
            .await
            .expect("proxy should build");

        let text_transport = ChannelTransport::new(text_router());
        proxy
            .add_backend("text", text_transport)
            .await
            .expect("add_backend should succeed");

        let health = proxy.health_check().await;
        assert_eq!(health.len(), 2);
        let namespaces: Vec<&str> = health.iter().map(|h| h.namespace.as_str()).collect();
        assert!(namespaces.contains(&"math"));
        assert!(namespaces.contains(&"text"));
        assert!(health.iter().all(|h| h.healthy));
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
                    input_responses: None,
                    request_state: None,
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

    // Static assertions: McpProxy must be Send + Sync so that &McpProxy is Send
    // and methods like health_check() can be called from tokio::spawn.
    #[allow(dead_code)]
    const _: () = {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        fn assert_send_sync() {
            assert_send::<McpProxy>();
            assert_sync::<McpProxy>();
        }
    };

    #[tokio::test]
    async fn test_health_check_is_spawnable() {
        // Verify that health_check future can be spawned (requires Send)
        let proxy = std::sync::Arc::new(build_test_proxy().await);
        let proxy_clone = proxy.clone();
        let handle = tokio::spawn(async move { proxy_clone.health_check().await });
        let results = handle.await.unwrap();
        assert!(!results.is_empty());
    }

    // ========================================================================
    // Remove / replace / namespaces
    // ========================================================================

    #[tokio::test]
    async fn test_remove_backend() {
        let proxy = build_test_proxy().await;
        assert_eq!(proxy.backend_count(), 2);
        assert!(proxy.backend_namespaces().contains(&"math".to_string()));

        // Remove the math backend
        assert!(proxy.remove_backend("math").await);
        assert_eq!(proxy.backend_count(), 1);
        assert!(!proxy.backend_namespaces().contains(&"math".to_string()));
        assert!(proxy.backend_namespaces().contains(&"text".to_string()));
    }

    #[tokio::test]
    async fn test_remove_backend_not_found() {
        let proxy = build_test_proxy().await;
        assert!(!proxy.remove_backend("nonexistent").await);
        assert_eq!(proxy.backend_count(), 2);
    }

    #[tokio::test]
    async fn test_remove_backend_tools_no_longer_listed() {
        let mut proxy = build_test_proxy().await;

        // Before removal: math/add should be listed
        let resp = call_proxy(&mut proxy, McpRequest::ListTools(Default::default()))
            .await
            .unwrap();
        let tools = match resp {
            McpResponse::ListTools(r) => r.tools,
            _ => panic!("expected ListTools"),
        };
        assert!(tools.iter().any(|t| t.name.contains("math")));

        // Remove math
        proxy.remove_backend("math").await;

        // After removal: math tools should be gone
        let resp = call_proxy(&mut proxy, McpRequest::ListTools(Default::default()))
            .await
            .unwrap();
        let tools = match resp {
            McpResponse::ListTools(r) => r.tools,
            _ => panic!("expected ListTools"),
        };
        assert!(!tools.iter().any(|t| t.name.contains("math")));
        // text tools should still be there
        assert!(tools.iter().any(|t| t.name.contains("text")));
    }

    #[tokio::test]
    async fn test_remove_backend_tool_calls_fail() {
        let mut proxy = build_test_proxy().await;
        proxy.remove_backend("math").await;

        // Calling a removed backend's tool should fail
        let resp = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                input_responses: None,
                request_state: None,
                name: "math_add".to_string(),
                arguments: json!({"a": 1, "b": 2}),
                meta: None,
                task: None,
            }),
        )
        .await;

        assert!(resp.is_err());
    }

    #[tokio::test]
    async fn test_replace_backend() {
        let mut proxy = build_test_proxy().await;

        // Replace math with a new math backend
        let new_math_transport = ChannelTransport::new(math_router());
        proxy
            .replace_backend("math", new_math_transport)
            .await
            .expect("replace should succeed");

        assert_eq!(proxy.backend_count(), 2);

        // The replaced backend should still work
        let resp = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                input_responses: None,
                request_state: None,
                name: "math_add".to_string(),
                arguments: json!({"a": 10, "b": 20}),
                meta: None,
                task: None,
            }),
        )
        .await
        .unwrap();

        match resp {
            McpResponse::CallTool(r) => {
                let text = r.content[0].as_text().unwrap();
                assert_eq!(text, "30");
            }
            _ => panic!("expected CallTool"),
        }
    }

    #[tokio::test]
    async fn test_backend_namespaces() {
        let proxy = build_test_proxy().await;
        let mut namespaces = proxy.backend_namespaces();
        namespaces.sort();
        assert_eq!(namespaces, vec!["math", "text"]);
    }

    #[tokio::test]
    async fn test_remove_all_backends() {
        let proxy = build_test_proxy().await;
        proxy.remove_backend("math").await;
        proxy.remove_backend("text").await;
        assert_eq!(proxy.backend_count(), 0);
        assert!(proxy.backend_namespaces().is_empty());
    }

    /// A proxy over one backend whose tool returns an explicit
    /// `structuredContent: null`. The tool opts out of the backend's own 2025
    /// shape check so the null reaches the proxy.
    async fn null_structured_proxy() -> McpProxy {
        let router = McpRouter::new().tool(
            ToolBuilder::new("nothing")
                .skip_output_validation()
                .handler(|_args: serde_json::Value| async {
                    Ok(CallToolResult::json(serde_json::Value::Null))
                })
                .build(),
        );
        McpProxy::builder("test-proxy", "1.0.0")
            .backend("b", ChannelTransport::new(router))
            .await
            .build_strict()
            .await
            .expect("proxy should build")
    }

    fn call_nothing(extensions: Extensions) -> RouterRequest {
        RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(crate::protocol::CallToolParams {
                input_responses: None,
                request_state: None,
                name: "b_nothing".to_string(),
                arguments: json!({}),
                meta: None,
                task: None,
            }),
            extensions,
        }
    }

    #[tokio::test]
    async fn test_proxy_drops_null_structured_content_for_a_2025_request() {
        let mut proxy = null_structured_proxy().await;
        let resp = proxy.call(call_nothing(Extensions::new())).await.unwrap();
        match resp.inner.expect("call tool should succeed") {
            McpResponse::CallTool(result) => assert!(result.structured_content.is_none()),
            other => panic!("expected CallTool response, got: {:?}", other),
        }
    }

    #[cfg(feature = "stateless")]
    #[tokio::test]
    async fn test_proxy_keeps_null_structured_content_for_a_2026_request() {
        let mut proxy = null_structured_proxy().await;
        let mut extensions = Extensions::new();
        extensions.insert(crate::stateless::StatelessRequestMeta {
            protocol_version: Some(crate::protocol::PROTOCOL_VERSION_2026_07_28.to_string()),
            ..Default::default()
        });
        let resp = proxy.call(call_nothing(extensions)).await.unwrap();
        match resp.inner.expect("call tool should succeed") {
            McpResponse::CallTool(result) => {
                assert_eq!(result.structured_content, Some(serde_json::Value::Null));
            }
            other => panic!("expected CallTool response, got: {:?}", other),
        }
    }

    // ========================================================================
    // Atomic backend replacement (#1616)
    // ========================================================================

    /// A transport whose server never answers, so initialization stalls
    /// until the caller gives up.
    struct StallTransport;

    #[async_trait::async_trait]
    impl crate::client::ClientTransport for StallTransport {
        async fn send(&mut self, _message: &str) -> crate::error::Result<()> {
            Ok(())
        }

        async fn recv(&mut self) -> crate::error::Result<Option<String>> {
            std::future::pending().await
        }

        fn is_connected(&self) -> bool {
            true
        }

        async fn close(&mut self) -> crate::error::Result<()> {
            Ok(())
        }
    }

    async fn call_math_add(proxy: &mut McpProxy) -> Result<String, String> {
        let resp = call_proxy(
            proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                input_responses: None,
                request_state: None,
                name: "math_add".to_string(),
                arguments: json!({"a": 2, "b": 3}),
                meta: None,
                task: None,
            }),
        )
        .await
        .map_err(|e| e.message)?;
        match resp {
            McpResponse::CallTool(r) => Ok(r.all_text()),
            other => Err(format!("expected CallTool, got {other:?}")),
        }
    }

    async fn listed_tool_names(proxy: &mut McpProxy) -> Vec<String> {
        match call_proxy(proxy, McpRequest::ListTools(Default::default()))
            .await
            .expect("list tools should succeed")
        {
            McpResponse::ListTools(result) => {
                let mut names: Vec<String> = result.tools.into_iter().map(|t| t.name).collect();
                names.sort();
                names
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_replace_backend_failure_keeps_previous_backend() {
        let mut proxy = build_test_proxy().await;

        let err = proxy
            .replace_backend("math", BrokenTransport)
            .await
            .expect_err("a broken replacement must fail");
        assert!(
            matches!(
                err,
                crate::proxy::AddBackendError::Connect(_)
                    | crate::proxy::AddBackendError::Initialize(_)
            ),
            "{err}"
        );

        assert_eq!(proxy.backend_namespaces(), vec!["math", "text"]);
        assert_eq!(call_math_add(&mut proxy).await.unwrap(), "5");
        assert!(
            listed_tool_names(&mut proxy)
                .await
                .contains(&"math_add".to_string())
        );
    }

    #[tokio::test]
    async fn test_replace_backend_retry_after_failure_succeeds() {
        let mut proxy = build_test_proxy().await;
        assert!(
            proxy
                .replace_backend("math", BrokenTransport)
                .await
                .is_err()
        );

        proxy
            .replace_backend("math", ChannelTransport::new(math_router()))
            .await
            .expect("retry should succeed");
        assert_eq!(proxy.backend_count(), 2);
        assert_eq!(call_math_add(&mut proxy).await.unwrap(), "5");
    }

    #[tokio::test]
    async fn test_replace_backend_cancelled_during_staging_keeps_previous_backend() {
        let mut proxy = build_test_proxy().await;

        let outcome = tokio::time::timeout(
            Duration::from_millis(200),
            proxy.replace_backend("math", StallTransport),
        )
        .await;
        assert!(outcome.is_err(), "staging should still be stalled");

        assert_eq!(proxy.backend_namespaces(), vec!["math", "text"]);
        assert_eq!(call_math_add(&mut proxy).await.unwrap(), "5");
    }

    #[tokio::test]
    async fn test_replace_backend_serves_old_until_commit_then_new() {
        let mut proxy = build_test_proxy().await;

        // A replacement still staging must not disturb the live backend.
        let staging = {
            let proxy = proxy.clone();
            tokio::spawn(async move { proxy.replace_backend("math", StallTransport).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(call_math_add(&mut proxy).await.unwrap(), "5");
        staging.abort();

        // Replace "math" with a server exposing different tools; the switch is
        // visible at once and the backend keeps its position.
        proxy
            .replace_backend("math", ChannelTransport::new(text_router()))
            .await
            .expect("replace should succeed");
        assert_eq!(proxy.backend_namespaces(), vec!["math", "text"]);
        let names = listed_tool_names(&mut proxy).await;
        assert!(names.contains(&"math_echo".to_string()), "{names:?}");
        assert!(!names.contains(&"math_add".to_string()), "{names:?}");
        assert!(call_math_add(&mut proxy).await.is_err());
    }

    #[tokio::test]
    async fn test_replace_backend_with_layer_applies_layer() {
        let mut proxy = build_test_proxy().await;
        proxy
            .replace_backend_with_layer(
                "math",
                ChannelTransport::new(slow_router()),
                TimeoutLayer::new(Duration::from_millis(50)),
            )
            .await
            .expect("replace should succeed");

        let resp = call_proxy(
            &mut proxy,
            McpRequest::CallTool(crate::protocol::CallToolParams {
                input_responses: None,
                request_state: None,
                name: "math_slow".to_string(),
                arguments: json!({"delay_ms": 500}),
                meta: None,
                task: None,
            }),
        )
        .await;
        let timed_out = match resp {
            Err(_) => true,
            Ok(McpResponse::CallTool(r)) => r.is_error,
            Ok(_) => false,
        };
        assert!(timed_out, "the replacement's timeout layer should apply");
    }

    #[tokio::test]
    async fn test_concurrent_replaces_leave_one_backend() {
        let proxy = build_test_proxy().await;
        let (a, b) = tokio::join!(
            proxy.replace_backend("math", ChannelTransport::new(math_router())),
            proxy.replace_backend("math", ChannelTransport::new(math_router())),
        );
        a.expect("first replace");
        b.expect("second replace");
        assert_eq!(proxy.backend_namespaces(), vec!["math", "text"]);
        assert_eq!(proxy.shared.backends.read().await.len(), 2);
    }

    #[tokio::test]
    async fn test_replace_backend_rejects_conflict_with_other_backend() {
        let proxy = McpProxy::builder("p", "1.0.0")
            .separator(".")
            .backend("db", ChannelTransport::new(math_router()))
            .await
            .build_strict()
            .await
            .expect("proxy should build");
        // "db.read" would shadow "db"'s prefix; replacing it is still a conflict.
        let err = proxy
            .replace_backend("db.read", ChannelTransport::new(text_router()))
            .await
            .expect_err("ambiguous prefix");
        assert!(
            matches!(err, crate::proxy::AddBackendError::AmbiguousPrefix { .. }),
            "{err}"
        );
        assert_eq!(proxy.backend_namespaces(), vec!["db"]);
    }

    /// A backend whose host can push notifications into its client stream.
    fn notifying_backend(
        router: McpRouter,
    ) -> (ChannelTransport, crate::context::NotificationSender) {
        let (tx, rx) = notification_channel(16);
        let router = router.with_notification_sender(tx.clone());
        (ChannelTransport::with_notifications(router, rx), tx)
    }

    fn drain(rx: &mut crate::context::NotificationReceiver) {
        while rx.try_recv().is_ok() {}
    }

    #[tokio::test]
    async fn test_watcher_follows_backend_identity_after_removal() {
        let (proxy_tx, mut proxy_rx) = notification_channel(64);
        let (c_transport, c_tx) = notifying_backend(math_router());
        let proxy = McpProxy::builder("watch", "1.0.0")
            .notification_sender(proxy_tx)
            .backend("a", ChannelTransport::new(math_router()))
            .await
            .backend("b", ChannelTransport::new(text_router()))
            .await
            .backend("c", c_transport)
            .await
            .build_strict()
            .await
            .expect("proxy should build");

        // Removing the first backend shifts every later position. A watcher
        // keyed on position would now find nothing at c's old index.
        assert!(proxy.remove_backend("a").await);
        drain(&mut proxy_rx);

        c_tx.send(crate::context::ServerNotification::ToolsListChanged)
            .await
            .unwrap();
        let forwarded = tokio::time::timeout(Duration::from_secs(5), proxy_rx.recv())
            .await
            .expect("c's watcher should refresh and forward the change");
        assert!(matches!(
            forwarded,
            Some(crate::context::ServerNotification::ToolsListChanged)
        ));
    }

    #[tokio::test]
    async fn test_watcher_runs_for_replacement_backend() {
        let (proxy_tx, mut proxy_rx) = notification_channel(64);
        let proxy = McpProxy::builder("watch", "1.0.0")
            .notification_sender(proxy_tx)
            .backend("a", ChannelTransport::new(math_router()))
            .await
            .backend("b", ChannelTransport::new(text_router()))
            .await
            .build_strict()
            .await
            .expect("proxy should build");

        let (replacement, replacement_tx) = notifying_backend(math_router());
        proxy
            .replace_backend("a", replacement)
            .await
            .expect("replace should succeed");
        drain(&mut proxy_rx);

        replacement_tx
            .send(crate::context::ServerNotification::PromptsListChanged)
            .await
            .unwrap();
        let forwarded = tokio::time::timeout(Duration::from_secs(5), proxy_rx.recv())
            .await
            .expect("the replacement's watcher should forward the change");
        assert!(matches!(
            forwarded,
            Some(crate::context::ServerNotification::PromptsListChanged)
        ));
    }

    struct CatalogTransport<F> {
        respond: F,
        replies: std::collections::VecDeque<String>,
    }

    #[async_trait::async_trait]
    impl<F> crate::client::ClientTransport for CatalogTransport<F>
    where
        F: FnMut(&serde_json::Value) -> Option<serde_json::Value> + Send + 'static,
    {
        async fn send(&mut self, message: &str) -> crate::error::Result<()> {
            let request: serde_json::Value = serde_json::from_str(message)?;
            if request.get("id").is_some()
                && let Some(mut reply) = (self.respond)(&request)
            {
                reply["jsonrpc"] = json!("2.0");
                reply["id"] = request["id"].clone();
                self.replies.push_back(reply.to_string());
            }
            Ok(())
        }

        async fn recv(&mut self) -> crate::error::Result<Option<String>> {
            match self.replies.pop_front() {
                Some(reply) => Ok(Some(reply)),
                None => std::future::pending().await,
            }
        }

        fn is_connected(&self) -> bool {
            true
        }

        async fn close(&mut self) -> crate::error::Result<()> {
            Ok(())
        }
    }

    fn catalog_transport(
        capabilities: serde_json::Value,
        mut respond: impl FnMut(&serde_json::Value) -> Option<serde_json::Value> + Send + 'static,
    ) -> impl crate::client::ClientTransport {
        CatalogTransport {
            respond: move |request: &serde_json::Value| {
                if request["method"] == "initialize" {
                    Some(json!({"result": {
                        "protocolVersion": "2025-11-25",
                        "serverInfo": {"name": "catalog", "version": "1"},
                        "capabilities": capabilities,
                    }}))
                } else {
                    respond(request)
                }
            },
            replies: Default::default(),
        }
    }

    fn method_not_found() -> serde_json::Value {
        json!({"error": {"code": -32601, "message": "unsupported method"}})
    }

    #[tokio::test(start_paused = true)]
    async fn setup_deadlines_report_connect_initialize_and_discovery() {
        let options = BackendSetupOptions {
            connect_timeout: Duration::from_secs(1),
            initialize_timeout: Duration::from_secs(2),
            discovery_timeout: Duration::from_secs(3),
        };
        let builder = McpProxy::builder("proxy", "1")
            .backend_with_options(
                "connect",
                std::future::pending::<crate::error::Result<StallTransport>>(),
                options,
            )
            .await
            // A layer on a skipped first connection must not panic.
            .backend_layer(TimeoutLayer::new(Duration::from_secs(1)))
            .backend_with_options("initialize", async { Ok(StallTransport) }, options)
            .await
            .backend_with_options(
                "discover",
                async {
                    Ok(catalog_transport(json!({"tools": {}}), |request| {
                        assert_eq!(request["method"], "tools/list");
                        None
                    }))
                },
                options,
            )
            .await;
        let error = builder
            .build()
            .await
            .err()
            .expect("all attempts failed")
            .to_string();
        assert!(error.contains("[connect] Connect failed"), "{error}");
        assert!(
            error.contains("transport connect timed out after 1s"),
            "{error}"
        );
        assert!(error.contains("[initialize] Initialize failed"), "{error}");
        assert!(
            error.contains("MCP initialize timed out after 2s"),
            "{error}"
        );
        assert!(error.contains("[discover] Initialize failed"), "{error}");
        assert!(
            error.contains("initial capability discovery timed out after 3s"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn configured_discovery_skips_unadvertised_catalogs_and_optional_templates() {
        let transport = catalog_transport(json!({"tools": {}, "resources": {}}), |request| {
            Some(match request["method"].as_str().unwrap() {
                "tools/list" => json!({"result": {"tools": []}}),
                "resources/list" => json!({"result": {"resources": []}}),
                "resources/templates/list" => method_not_found(),
                method => panic!("unexpected request {method}"),
            })
        });
        let mut proxy = McpProxy::builder("proxy", "1")
            .backend_with_options(
                "empty",
                async { Ok(transport) },
                BackendSetupOptions::default(),
            )
            .await
            .build_strict()
            .await
            .expect("successful empty and unsupported catalogs should be accepted");
        assert_eq!(proxy.backend_namespaces(), ["empty"]);
        assert!(listed_tool_names(&mut proxy).await.is_empty());
    }

    #[tokio::test]
    async fn configured_connected_client_reuses_initialize_result() {
        let mut initialized = false;
        let client = McpClient::connect(CatalogTransport {
            respond: move |request: &serde_json::Value| {
                Some(match request["method"].as_str().unwrap() {
                    "initialize" => {
                        assert!(!initialized, "initialize ran twice");
                        initialized = true;
                        json!({"result": {
                            "protocolVersion": "2025-11-25",
                            "serverInfo": {"name": "connected", "version": "1"},
                            "capabilities": {},
                        }})
                    }
                    method => panic!("unexpected request {method}"),
                })
            },
            replies: Default::default(),
        })
        .await
        .unwrap();
        client.initialize("upstream", "1").await.unwrap();

        let proxy = McpProxy::builder("proxy", "1")
            .backend_client_with_options("connected", client, BackendSetupOptions::default())
            .build_strict()
            .await
            .unwrap();
        assert_eq!(proxy.backend_namespaces(), ["connected"]);
    }

    #[tokio::test]
    async fn discovery_failure_on_a_later_page_obeys_build_policy() {
        for strict in [false, true] {
            let transport = catalog_transport(json!({"tools": {}}), |request| {
                assert_eq!(request["method"], "tools/list");
                Some(if request["params"]["cursor"] == "next-page" {
                    json!({"error": {"code": -32601, "message": "unsupported method", "data": {"cursor": "next-page"}}})
                } else {
                    json!({"result": {
                        "tools": [{"name": "partial", "inputSchema": {"type": "object"}}],
                        "nextCursor": "next-page"
                    }})
                })
            });
            let builder = McpProxy::builder("proxy", "1")
                .backend_with_options(
                    "broken",
                    async { Ok(transport) },
                    BackendSetupOptions::default(),
                )
                .await
                .backend("math", ChannelTransport::new(math_router()))
                .await;
            let skipped = if strict {
                let error = builder
                    .build_strict()
                    .await
                    .err()
                    .expect("strict build failed");
                let Error::ProxySetup { source, .. } = error else {
                    panic!("expected build context, got {error}");
                };
                source.downcast::<BackendSetupFailures>().unwrap().skipped
            } else {
                let mut result = builder.build().await.expect("degraded build succeeded");
                assert_eq!(listed_tool_names(&mut result.proxy).await, ["math_add"]);
                result.skipped
            };
            assert_eq!(skipped.len(), 1);
            assert_eq!(skipped[0].namespace, "broken");
            assert_eq!(skipped[0].phase, SkippedPhase::Initialize);
            let error = skipped[0].error.to_string();
            assert!(
                error.contains("initial capability discovery failed for tools/list"),
                "{error}"
            );
            assert!(error.contains("unsupported method"), "{error}");
            let cause = std::error::Error::source(&skipped[0].error).unwrap();
            let Some(Error::JsonRpc(rpc)) = cause.downcast_ref::<Error>() else {
                panic!("expected original JSON-RPC error, got {cause}");
            };
            assert_eq!(rpc.code, -32601);
            assert_eq!(rpc.data, Some(json!({"cursor": "next-page"})));
        }
    }

    #[tokio::test]
    async fn all_failed_build_retains_discovery_page_limit_cause() {
        let transport = catalog_transport(json!({"tools": {}}), |_| {
            Some(json!({"result": {"tools": [], "nextCursor": "more"}}))
        });
        let client = McpClient::builder()
            .max_list_pages(1)
            .connect_simple(transport)
            .await
            .unwrap();
        let error = McpProxy::builder("proxy", "1")
            .backend_client_with_options("limited", client, BackendSetupOptions::default())
            .build()
            .await
            .err()
            .expect("catalog exceeds client's page limit");
        let mut cause: &(dyn std::error::Error + 'static) = &error;
        while let Some(source) = cause.source() {
            cause = source;
        }
        assert!(
            matches!(cause.downcast_ref::<Error>(),
                Some(Error::ListPageLimitExceeded { method, limit: 1 }) if method == "tools/list"
            ),
            "{error}"
        );
    }

    #[tokio::test]
    async fn builder_layer_after_rejected_connection_leaves_previous_backend_unwrapped() {
        let calls = std::sync::Arc::default();
        let mut proxy = McpProxy::builder("proxy", "1")
            .backend("math", ChannelTransport::new(math_router()))
            .await
            .backend_with_options(
                "rejected",
                async { Err::<StallTransport, _>(crate::error::Error::internal("unavailable")) },
                BackendSetupOptions::default(),
            )
            .await
            .backend_layer(MarkerLayer {
                tag: "rejected",
                calls: std::sync::Arc::clone(&calls),
            })
            .build()
            .await
            .unwrap()
            .proxy;
        assert_eq!(call_math_add(&mut proxy).await.unwrap(), "5");
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn configured_replacement_connect_timeout_keeps_current_backend_serving() {
        let mut proxy = build_test_proxy().await;
        let options = BackendSetupOptions {
            connect_timeout: Duration::from_secs(2),
            ..Default::default()
        };
        let error = proxy
            .replace_backend_with_options(
                "math",
                std::future::pending::<crate::error::Result<StallTransport>>(),
                options,
            )
            .await
            .expect_err("transport construction should time out");
        assert!(matches!(error, AddBackendError::Connect(_)), "{error}");
        assert!(
            error
                .to_string()
                .contains("transport connect timed out after 2s")
        );
        assert_eq!(call_math_add(&mut proxy).await.unwrap(), "5");
    }

    #[tokio::test(start_paused = true)]
    async fn configured_dynamic_layered_setup_retains_previous_backend_on_failure() {
        let mut proxy = build_test_proxy().await;
        let calls = std::sync::Arc::default();
        let layer = MarkerLayer {
            tag: "dynamic",
            calls: std::sync::Arc::clone(&calls),
        };
        let options = BackendSetupOptions {
            discovery_timeout: Duration::from_millis(100),
            ..Default::default()
        };
        proxy
            .add_backend_with_options_and_layer(
                "other",
                async { Ok(ChannelTransport::new(math_router())) },
                options,
                layer.clone(),
            )
            .await
            .unwrap();
        let listed = listed_tool_names(&mut proxy).await;
        assert!(listed.contains(&"other_add".to_string()));

        for stall in [false, true] {
            let transport = catalog_transport(json!({"tools": {}}), move |_| {
                if stall {
                    None
                } else {
                    Some(json!({"error": {"code": -32603, "message": "catalog unavailable"}}))
                }
            });
            let failure = proxy
                .replace_backend_with_options_and_layer(
                    "math",
                    async { Ok(transport) },
                    options,
                    layer.clone(),
                )
                .await
                .expect_err("replacement discovery failed");
            assert!(matches!(failure, AddBackendError::Initialize(_)));
            let message = failure.to_string();
            assert!(
                message.contains(if stall {
                    "initial capability discovery timed out"
                } else {
                    "catalog unavailable"
                }),
                "{message}"
            );
            assert_eq!(call_math_add(&mut proxy).await.unwrap(), "5");
            assert_eq!(listed_tool_names(&mut proxy).await, listed);
            assert!(
                calls.lock().unwrap().is_empty(),
                "failed staging must not install its layer"
            );
        }

        proxy
            .replace_backend_with_options_and_layer(
                "math",
                async { Ok(ChannelTransport::new(math_router())) },
                options,
                layer,
            )
            .await
            .unwrap();
        assert_eq!(call_math_add(&mut proxy).await.unwrap(), "5");
        assert_eq!(*calls.lock().unwrap(), ["dynamic"]);
    }

    #[tokio::test(start_paused = true)]
    async fn health_stream_yields_fast_backend_before_stalled_snapshot_member() {
        use futures::StreamExt;

        let stalled = catalog_transport(json!({}), |request| {
            assert_eq!(request["method"], "ping");
            None
        });
        let mut proxy = McpProxy::builder("proxy", "1")
            .backend_with_options(
                "stalled",
                async { Ok(stalled) },
                BackendSetupOptions::default(),
            )
            .await
            .backend("math", ChannelTransport::new(math_router()))
            .await
            .build_strict()
            .await
            .unwrap();
        let timeout = Duration::from_millis(250);
        let probes = proxy.health_checks(timeout).await;
        futures::pin_mut!(probes);
        let first = probes.next().await.unwrap();
        assert_eq!(first.namespace, "math");
        assert!(first.healthy);
        assert!(proxy.remove_backend("stalled").await);
        let last = probes.next().await.unwrap();
        assert_eq!(last.namespace, "stalled");
        assert!(!last.healthy);
        assert!(probes.next().await.is_none());
        assert_eq!(proxy.health_check().await[0].namespace, "math");
        assert_eq!(call_math_add(&mut proxy).await.unwrap(), "5");
    }
}
