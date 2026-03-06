#[cfg(test)]
mod proxy_tests {
    use std::collections::HashMap;

    use serde_json::json;
    use tower_service::Service;

    use crate::client::{ChannelTransport, McpClient};
    use crate::protocol::{McpRequest, McpResponse, RequestId};
    use crate::proxy::McpProxy;
    use crate::router::{Extensions, RouterRequest};
    use crate::{
        CallToolResult, GetPromptResult, McpRouter, PromptBuilder, ResourceBuilder, ToolBuilder,
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
}
