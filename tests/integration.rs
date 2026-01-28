//! Integration tests for tower-mcp
//!
//! Tests the full protocol flow including session lifecycle,
//! tool execution, resources, prompts, and error handling.

use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::HashMap;
use tower_mcp::{
    CallToolResult, Extensions, GetPromptResult, JsonRpcRequest, JsonRpcResponse, JsonRpcService,
    McpRouter, PromptBuilder, PromptMessage, PromptRole, ReadResourceResult, ResourceBuilder,
    ResourceContent, ToolBuilder,
};

// =============================================================================
// Test fixtures
// =============================================================================

#[derive(Debug, Deserialize, JsonSchema)]
struct EchoInput {
    message: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

fn create_test_router() -> McpRouter {
    let echo = ToolBuilder::new("echo")
        .description("Echo a message")
        .read_only()
        .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.message)) })
        .build()
        .expect("valid tool");

    let add = ToolBuilder::new("add")
        .description("Add two numbers")
        .read_only()
        .idempotent()
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build()
        .expect("valid tool");

    let failing = ToolBuilder::new("failing")
        .description("A tool that always fails")
        .handler(
            |_input: EchoInput| async move { Err(tower_mcp::Error::tool("Intentional failure")) },
        )
        .build()
        .expect("valid tool");

    McpRouter::new()
        .server_info("test-server", "1.0.0")
        .instructions("Test server for integration tests")
        .tool(echo)
        .tool(add)
        .tool(failing)
}

fn create_router_with_resources_and_prompts() -> McpRouter {
    // Tools
    let echo = ToolBuilder::new("echo")
        .description("Echo a message")
        .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.message)) })
        .build()
        .expect("valid tool");

    // Resources
    let config_resource = ResourceBuilder::new("file:///config.json")
        .name("Configuration")
        .description("Application configuration")
        .json(serde_json::json!({
            "version": "1.0",
            "debug": true
        }));

    let readme_resource = ResourceBuilder::new("file:///README.md")
        .name("README")
        .description("Project documentation")
        .mime_type("text/markdown")
        .text("# Test Project\n\nThis is a test project.");

    let dynamic_resource = ResourceBuilder::new("memory://counter")
        .name("Counter")
        .description("A dynamic counter")
        .handler(|| async {
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri: "memory://counter".to_string(),
                    mime_type: Some("text/plain".to_string()),
                    text: Some("42".to_string()),
                    blob: None,
                }],
            })
        });

    // Prompts
    let greet_prompt = PromptBuilder::new("greet")
        .description("Generate a greeting")
        .required_arg("name", "The name to greet")
        .handler(|args: HashMap<String, String>| async move {
            let name = args.get("name").map(|s| s.as_str()).unwrap_or("World");
            Ok(GetPromptResult {
                description: Some("A friendly greeting".to_string()),
                messages: vec![PromptMessage {
                    role: PromptRole::User,
                    content: tower_mcp::protocol::Content::Text {
                        text: format!("Please greet {} warmly.", name),
                        annotations: None,
                    },
                }],
            })
        });

    let code_review_prompt = PromptBuilder::new("code_review")
        .description("Review code for issues")
        .required_arg("code", "The code to review")
        .optional_arg("language", "Programming language")
        .handler(|args: HashMap<String, String>| async move {
            let code = args.get("code").map(|s| s.as_str()).unwrap_or("");
            let lang = args
                .get("language")
                .map(|s| s.as_str())
                .unwrap_or("unknown");
            Ok(GetPromptResult {
                description: Some("Code review prompt".to_string()),
                messages: vec![PromptMessage {
                    role: PromptRole::User,
                    content: tower_mcp::protocol::Content::Text {
                        text: format!(
                            "Please review this {} code for issues:\n\n```{}\n{}\n```",
                            lang, lang, code
                        ),
                        annotations: None,
                    },
                }],
            })
        });

    let static_prompt = PromptBuilder::new("help")
        .description("Get help")
        .user_message("How can I help you today?");

    McpRouter::new()
        .server_info("test-server", "1.0.0")
        .tool(echo)
        .resource(config_resource)
        .resource(readme_resource)
        .resource(dynamic_resource)
        .prompt(greet_prompt)
        .prompt(code_review_prompt)
        .prompt(static_prompt)
}

// =============================================================================
// Session lifecycle tests
// =============================================================================

#[tokio::test]
async fn test_session_lifecycle_happy_path() {
    let router = create_test_router();
    let mut service = JsonRpcService::new(router.clone());

    // 1. Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": {
            "name": "test-client",
            "version": "1.0.0"
        }
    }));

    let resp = service.call_single(init_req).await.unwrap();
    match resp {
        JsonRpcResponse::Result(r) => {
            assert_eq!(r.id, tower_mcp::protocol::RequestId::Number(1));
            assert!(r.result.get("protocolVersion").is_some());
            assert!(r.result.get("serverInfo").is_some());
            assert!(r.result.get("capabilities").is_some());
        }
        JsonRpcResponse::Error(e) => panic!("Expected success, got error: {:?}", e),
    }

    // 2. Send initialized notification (handled by router directly)
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // 3. List tools
    let list_req = JsonRpcRequest::new(2, "tools/list");
    let resp = service.call_single(list_req).await.unwrap();
    match resp {
        JsonRpcResponse::Result(r) => {
            let tools = r.result.get("tools").unwrap().as_array().unwrap();
            assert_eq!(tools.len(), 3);
        }
        JsonRpcResponse::Error(e) => panic!("Expected success, got error: {:?}", e),
    }

    // 4. Call a tool
    let call_req = JsonRpcRequest::new(3, "tools/call").with_params(serde_json::json!({
        "name": "echo",
        "arguments": {
            "message": "Hello, MCP!"
        }
    }));
    let resp = service.call_single(call_req).await.unwrap();
    match resp {
        JsonRpcResponse::Result(r) => {
            let content = r.result.get("content").unwrap().as_array().unwrap();
            let text = content[0].get("text").unwrap().as_str().unwrap();
            assert_eq!(text, "Hello, MCP!");
        }
        JsonRpcResponse::Error(e) => panic!("Expected success, got error: {:?}", e),
    }
}

#[tokio::test]
async fn test_session_rejects_requests_before_init() {
    let router = create_test_router();
    let mut service = JsonRpcService::new(router);

    // Try to list tools before initializing
    let list_req = JsonRpcRequest::new(1, "tools/list");
    let resp = service.call_single(list_req).await.unwrap();

    match resp {
        JsonRpcResponse::Error(e) => {
            assert!(e.error.message.contains("not initialized"));
        }
        JsonRpcResponse::Result(_) => panic!("Expected error for pre-init request"),
    }
}

#[tokio::test]
async fn test_ping_always_allowed() {
    let router = create_test_router();
    let mut service = JsonRpcService::new(router);

    // Ping should work even before initialization
    let ping_req = JsonRpcRequest::new(1, "ping");
    let resp = service.call_single(ping_req).await.unwrap();

    match resp {
        JsonRpcResponse::Result(_) => {} // Success
        JsonRpcResponse::Error(e) => panic!("Ping should always work: {:?}", e),
    }
}

// =============================================================================
// Tool execution tests
// =============================================================================

#[tokio::test]
async fn test_tool_call_add() {
    let router = create_test_router();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize first
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    service.call_single(init_req).await.unwrap();
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Call add tool
    let call_req = JsonRpcRequest::new(2, "tools/call").with_params(serde_json::json!({
        "name": "add",
        "arguments": { "a": 17, "b": 25 }
    }));
    let resp = service.call_single(call_req).await.unwrap();

    match resp {
        JsonRpcResponse::Result(r) => {
            let content = r.result.get("content").unwrap().as_array().unwrap();
            let text = content[0].get("text").unwrap().as_str().unwrap();
            assert_eq!(text, "42");
        }
        JsonRpcResponse::Error(e) => panic!("Expected success, got error: {:?}", e),
    }
}

#[tokio::test]
async fn test_tool_not_found() {
    let router = create_test_router();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    service.call_single(init_req).await.unwrap();
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Call non-existent tool
    let call_req = JsonRpcRequest::new(2, "tools/call").with_params(serde_json::json!({
        "name": "nonexistent",
        "arguments": {}
    }));
    let resp = service.call_single(call_req).await.unwrap();

    match resp {
        JsonRpcResponse::Error(e) => {
            assert_eq!(e.error.code, -32601); // Method not found
        }
        JsonRpcResponse::Result(_) => panic!("Expected error for nonexistent tool"),
    }
}

#[tokio::test]
async fn test_tool_invalid_arguments() {
    let router = create_test_router();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    service.call_single(init_req).await.unwrap();
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Call add with invalid arguments (missing required field)
    let call_req = JsonRpcRequest::new(2, "tools/call").with_params(serde_json::json!({
        "name": "add",
        "arguments": { "a": 10 }  // Missing "b"
    }));
    let resp = service.call_single(call_req).await.unwrap();

    match resp {
        JsonRpcResponse::Error(e) => {
            assert!(e.error.message.contains("Invalid input") || e.error.code == -32603);
        }
        JsonRpcResponse::Result(_) => panic!("Expected error for invalid arguments"),
    }
}

#[tokio::test]
async fn test_tool_execution_error() {
    let router = create_test_router();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    service.call_single(init_req).await.unwrap();
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Call failing tool
    let call_req = JsonRpcRequest::new(2, "tools/call").with_params(serde_json::json!({
        "name": "failing",
        "arguments": { "message": "test" }
    }));
    let resp = service.call_single(call_req).await.unwrap();

    match resp {
        JsonRpcResponse::Error(e) => {
            assert!(e.error.message.contains("Intentional failure"));
        }
        JsonRpcResponse::Result(_) => panic!("Expected error from failing tool"),
    }
}

// =============================================================================
// Batch request tests
// =============================================================================

#[tokio::test]
async fn test_batch_requests() {
    let router = create_test_router();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize first
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    service.call_single(init_req).await.unwrap();
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Send batch of requests
    let requests = vec![
        JsonRpcRequest::new(2, "tools/list"),
        JsonRpcRequest::new(3, "tools/call").with_params(serde_json::json!({
            "name": "echo",
            "arguments": { "message": "batch test" }
        })),
        JsonRpcRequest::new(4, "ping"),
    ];

    let responses = service.call_batch(requests).await.unwrap();

    assert_eq!(responses.len(), 3);

    // All should be successful
    for resp in &responses {
        match resp {
            JsonRpcResponse::Result(_) => {}
            JsonRpcResponse::Error(e) => panic!("Unexpected error in batch: {:?}", e),
        }
    }
}

// =============================================================================
// Protocol version negotiation tests
// =============================================================================

#[tokio::test]
async fn test_protocol_version_negotiation_supported() {
    let router = create_test_router();
    let mut service = JsonRpcService::new(router);

    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));

    let resp = service.call_single(init_req).await.unwrap();

    match resp {
        JsonRpcResponse::Result(r) => {
            let version = r.result.get("protocolVersion").unwrap().as_str().unwrap();
            assert_eq!(version, "2025-03-26");
        }
        JsonRpcResponse::Error(e) => panic!("Unexpected error: {:?}", e),
    }
}

#[tokio::test]
async fn test_protocol_version_negotiation_unsupported() {
    let router = create_test_router();
    let mut service = JsonRpcService::new(router);

    // Request unsupported version
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2020-01-01",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));

    let resp = service.call_single(init_req).await.unwrap();

    match resp {
        JsonRpcResponse::Result(r) => {
            // Server responds with its latest supported version
            let version = r.result.get("protocolVersion").unwrap().as_str().unwrap();
            assert_eq!(version, "2025-03-26");
        }
        JsonRpcResponse::Error(e) => panic!("Unexpected error: {:?}", e),
    }
}

// =============================================================================
// JSON-RPC validation tests
// =============================================================================

#[tokio::test]
async fn test_invalid_jsonrpc_version() {
    let router = create_test_router();
    let mut service = JsonRpcService::new(router);

    // Create request with wrong JSON-RPC version
    let req = JsonRpcRequest {
        jsonrpc: "1.0".to_string(), // Wrong version
        id: tower_mcp::protocol::RequestId::Number(1),
        method: "ping".to_string(),
        params: None,
    };

    let resp = service.call_single(req).await.unwrap();

    match resp {
        JsonRpcResponse::Error(e) => {
            assert_eq!(e.error.code, -32600); // Invalid request
        }
        JsonRpcResponse::Result(_) => panic!("Expected error for invalid JSON-RPC version"),
    }
}

// =============================================================================
// Error path tests (#13)
// =============================================================================

#[tokio::test]
async fn test_error_unknown_method() {
    let router = create_test_router();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    service.call_single(init_req).await.unwrap();
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Try unknown method
    let req = JsonRpcRequest::new(2, "unknown/method");
    let resp = service.call_single(req).await.unwrap();

    match resp {
        JsonRpcResponse::Error(e) => {
            assert_eq!(e.error.code, -32601); // Method not found
            assert!(e.error.message.contains("unknown/method"));
        }
        JsonRpcResponse::Result(_) => panic!("Expected error for unknown method"),
    }
}

#[tokio::test]
async fn test_error_malformed_tool_arguments() {
    let router = create_test_router();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    service.call_single(init_req).await.unwrap();
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Call tool with wrong argument types
    let call_req = JsonRpcRequest::new(2, "tools/call").with_params(serde_json::json!({
        "name": "add",
        "arguments": { "a": "not a number", "b": 5 }
    }));
    let resp = service.call_single(call_req).await.unwrap();

    match resp {
        JsonRpcResponse::Error(e) => {
            assert_eq!(e.error.code, -32603); // Internal error (tool error)
            assert!(e.error.message.contains("Invalid input"));
        }
        JsonRpcResponse::Result(_) => panic!("Expected error for malformed arguments"),
    }
}

#[tokio::test]
async fn test_error_double_initialization() {
    let router = create_test_router();
    let mut service = JsonRpcService::new(router.clone());

    // First initialization
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    let resp1 = service.call_single(init_req).await.unwrap();
    assert!(matches!(resp1, JsonRpcResponse::Result(_)));

    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Second initialization should still work (per MCP spec, server can re-initialize)
    let init_req2 = JsonRpcRequest::new(2, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    let resp2 = service.call_single(init_req2).await.unwrap();
    // Note: Whether this succeeds or fails depends on implementation
    // Our implementation allows it currently
    assert!(matches!(resp2, JsonRpcResponse::Result(_)));
}

#[tokio::test]
async fn test_error_missing_required_params() {
    let router = create_test_router();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    service.call_single(init_req).await.unwrap();
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Call tools/call without arguments
    let call_req = JsonRpcRequest::new(2, "tools/call").with_params(serde_json::json!({
        "name": "echo"
        // Missing "arguments"
    }));
    let resp = service.call_single(call_req).await.unwrap();

    match resp {
        JsonRpcResponse::Error(e) => {
            // Tool expects message field in arguments
            assert!(e.error.code == -32603 || e.error.code == -32602);
        }
        JsonRpcResponse::Result(_) => panic!("Expected error for missing arguments"),
    }
}

#[tokio::test]
async fn test_error_resources_not_found() {
    let router = create_test_router();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    service.call_single(init_req).await.unwrap();
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Try to read a resource (none registered)
    let read_req = JsonRpcRequest::new(2, "resources/read").with_params(serde_json::json!({
        "uri": "file:///nonexistent"
    }));
    let resp = service.call_single(read_req).await.unwrap();

    match resp {
        JsonRpcResponse::Error(e) => {
            assert_eq!(e.error.code, -32002); // MCP ResourceNotFound
        }
        JsonRpcResponse::Result(_) => panic!("Expected error for resource not found"),
    }
}

#[tokio::test]
async fn test_error_prompts_not_found() {
    let router = create_test_router();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    service.call_single(init_req).await.unwrap();
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Try to get a prompt (none registered)
    let get_req = JsonRpcRequest::new(2, "prompts/get").with_params(serde_json::json!({
        "name": "nonexistent"
    }));
    let resp = service.call_single(get_req).await.unwrap();

    match resp {
        JsonRpcResponse::Error(e) => {
            assert_eq!(e.error.code, -32601); // Method not found (prompt not found)
        }
        JsonRpcResponse::Result(_) => panic!("Expected error for prompt not found"),
    }
}

// =============================================================================
// Resources tests (#7)
// =============================================================================

#[tokio::test]
async fn test_resources_list() {
    let router = create_router_with_resources_and_prompts();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    let resp = service.call_single(init_req).await.unwrap();

    // Check that resources capability is declared
    match resp {
        JsonRpcResponse::Result(r) => {
            let caps = r.result.get("capabilities").unwrap();
            assert!(
                caps.get("resources").is_some(),
                "Resources capability should be declared"
            );
        }
        JsonRpcResponse::Error(e) => panic!("Unexpected error: {:?}", e),
    }

    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // List resources
    let list_req = JsonRpcRequest::new(2, "resources/list");
    let resp = service.call_single(list_req).await.unwrap();

    match resp {
        JsonRpcResponse::Result(r) => {
            let resources = r.result.get("resources").unwrap().as_array().unwrap();
            assert_eq!(resources.len(), 3);

            // Check that all resources have required fields
            for resource in resources {
                assert!(resource.get("uri").is_some());
                assert!(resource.get("name").is_some());
            }
        }
        JsonRpcResponse::Error(e) => panic!("Unexpected error: {:?}", e),
    }
}

#[tokio::test]
async fn test_resources_read_json() {
    let router = create_router_with_resources_and_prompts();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    service.call_single(init_req).await.unwrap();
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Read JSON resource
    let read_req = JsonRpcRequest::new(2, "resources/read").with_params(serde_json::json!({
        "uri": "file:///config.json"
    }));
    let resp = service.call_single(read_req).await.unwrap();

    match resp {
        JsonRpcResponse::Result(r) => {
            let contents = r.result.get("contents").unwrap().as_array().unwrap();
            assert_eq!(contents.len(), 1);

            let content = &contents[0];
            assert_eq!(
                content.get("uri").unwrap().as_str().unwrap(),
                "file:///config.json"
            );
            assert_eq!(
                content.get("mimeType").unwrap().as_str().unwrap(),
                "application/json"
            );

            let text = content.get("text").unwrap().as_str().unwrap();
            let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(parsed.get("version").unwrap().as_str().unwrap(), "1.0");
            assert!(parsed.get("debug").unwrap().as_bool().unwrap());
        }
        JsonRpcResponse::Error(e) => panic!("Unexpected error: {:?}", e),
    }
}

#[tokio::test]
async fn test_resources_read_text() {
    let router = create_router_with_resources_and_prompts();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    service.call_single(init_req).await.unwrap();
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Read markdown resource
    let read_req = JsonRpcRequest::new(2, "resources/read").with_params(serde_json::json!({
        "uri": "file:///README.md"
    }));
    let resp = service.call_single(read_req).await.unwrap();

    match resp {
        JsonRpcResponse::Result(r) => {
            let contents = r.result.get("contents").unwrap().as_array().unwrap();
            let content = &contents[0];
            let text = content.get("text").unwrap().as_str().unwrap();
            assert!(text.contains("# Test Project"));
        }
        JsonRpcResponse::Error(e) => panic!("Unexpected error: {:?}", e),
    }
}

#[tokio::test]
async fn test_resources_read_dynamic() {
    let router = create_router_with_resources_and_prompts();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    service.call_single(init_req).await.unwrap();
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Read dynamic resource
    let read_req = JsonRpcRequest::new(2, "resources/read").with_params(serde_json::json!({
        "uri": "memory://counter"
    }));
    let resp = service.call_single(read_req).await.unwrap();

    match resp {
        JsonRpcResponse::Result(r) => {
            let contents = r.result.get("contents").unwrap().as_array().unwrap();
            let content = &contents[0];
            let text = content.get("text").unwrap().as_str().unwrap();
            assert_eq!(text, "42");
        }
        JsonRpcResponse::Error(e) => panic!("Unexpected error: {:?}", e),
    }
}

#[tokio::test]
async fn test_resources_read_not_found() {
    let router = create_router_with_resources_and_prompts();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    service.call_single(init_req).await.unwrap();
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Try to read non-existent resource
    let read_req = JsonRpcRequest::new(2, "resources/read").with_params(serde_json::json!({
        "uri": "file:///nonexistent.txt"
    }));
    let resp = service.call_single(read_req).await.unwrap();

    match resp {
        JsonRpcResponse::Error(e) => {
            assert_eq!(e.error.code, -32002); // MCP ResourceNotFound
            assert!(e.error.message.contains("not found"));
        }
        JsonRpcResponse::Result(_) => panic!("Expected error for non-existent resource"),
    }
}

// =============================================================================
// Prompts tests (#8)
// =============================================================================

#[tokio::test]
async fn test_prompts_list() {
    let router = create_router_with_resources_and_prompts();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    let resp = service.call_single(init_req).await.unwrap();

    // Check that prompts capability is declared
    match resp {
        JsonRpcResponse::Result(r) => {
            let caps = r.result.get("capabilities").unwrap();
            assert!(
                caps.get("prompts").is_some(),
                "Prompts capability should be declared"
            );
        }
        JsonRpcResponse::Error(e) => panic!("Unexpected error: {:?}", e),
    }

    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // List prompts
    let list_req = JsonRpcRequest::new(2, "prompts/list");
    let resp = service.call_single(list_req).await.unwrap();

    match resp {
        JsonRpcResponse::Result(r) => {
            let prompts = r.result.get("prompts").unwrap().as_array().unwrap();
            assert_eq!(prompts.len(), 3);

            // Check that all prompts have required fields
            for prompt in prompts {
                assert!(prompt.get("name").is_some());
            }
        }
        JsonRpcResponse::Error(e) => panic!("Unexpected error: {:?}", e),
    }
}

#[tokio::test]
async fn test_prompts_get_with_args() {
    let router = create_router_with_resources_and_prompts();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    service.call_single(init_req).await.unwrap();
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Get prompt with arguments
    let get_req = JsonRpcRequest::new(2, "prompts/get").with_params(serde_json::json!({
        "name": "greet",
        "arguments": {
            "name": "Alice"
        }
    }));
    let resp = service.call_single(get_req).await.unwrap();

    match resp {
        JsonRpcResponse::Result(r) => {
            let messages = r.result.get("messages").unwrap().as_array().unwrap();
            assert_eq!(messages.len(), 1);

            let message = &messages[0];
            assert_eq!(message.get("role").unwrap().as_str().unwrap(), "user");

            let content = message.get("content").unwrap();
            let text = content.get("text").unwrap().as_str().unwrap();
            assert!(text.contains("Alice"));
        }
        JsonRpcResponse::Error(e) => panic!("Unexpected error: {:?}", e),
    }
}

#[tokio::test]
async fn test_prompts_get_code_review() {
    let router = create_router_with_resources_and_prompts();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    service.call_single(init_req).await.unwrap();
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Get code review prompt
    let get_req = JsonRpcRequest::new(2, "prompts/get").with_params(serde_json::json!({
        "name": "code_review",
        "arguments": {
            "code": "fn main() { println!(\"Hello\"); }",
            "language": "rust"
        }
    }));
    let resp = service.call_single(get_req).await.unwrap();

    match resp {
        JsonRpcResponse::Result(r) => {
            let messages = r.result.get("messages").unwrap().as_array().unwrap();
            let content = messages[0].get("content").unwrap();
            let text = content.get("text").unwrap().as_str().unwrap();
            assert!(text.contains("rust"));
            assert!(text.contains("println"));
        }
        JsonRpcResponse::Error(e) => panic!("Unexpected error: {:?}", e),
    }
}

#[tokio::test]
async fn test_prompts_get_static() {
    let router = create_router_with_resources_and_prompts();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    service.call_single(init_req).await.unwrap();
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Get static prompt (no args needed)
    let get_req = JsonRpcRequest::new(2, "prompts/get").with_params(serde_json::json!({
        "name": "help"
    }));
    let resp = service.call_single(get_req).await.unwrap();

    match resp {
        JsonRpcResponse::Result(r) => {
            let messages = r.result.get("messages").unwrap().as_array().unwrap();
            let content = messages[0].get("content").unwrap();
            let text = content.get("text").unwrap().as_str().unwrap();
            assert_eq!(text, "How can I help you today?");
        }
        JsonRpcResponse::Error(e) => panic!("Unexpected error: {:?}", e),
    }
}

#[tokio::test]
async fn test_prompts_get_not_found() {
    let router = create_router_with_resources_and_prompts();
    let mut service = JsonRpcService::new(router.clone());

    // Initialize
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    service.call_single(init_req).await.unwrap();
    router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);

    // Try to get non-existent prompt
    let get_req = JsonRpcRequest::new(2, "prompts/get").with_params(serde_json::json!({
        "name": "nonexistent"
    }));
    let resp = service.call_single(get_req).await.unwrap();

    match resp {
        JsonRpcResponse::Error(e) => {
            assert_eq!(e.error.code, -32601);
            assert!(e.error.message.contains("not found"));
        }
        JsonRpcResponse::Result(_) => panic!("Expected error for non-existent prompt"),
    }
}

// =============================================================================
// Capabilities tests
// =============================================================================

#[tokio::test]
async fn test_capabilities_tools_only() {
    let router = create_test_router(); // Has only tools
    let mut service = JsonRpcService::new(router);

    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    let resp = service.call_single(init_req).await.unwrap();

    match resp {
        JsonRpcResponse::Result(r) => {
            let caps = r.result.get("capabilities").unwrap();
            assert!(
                caps.get("tools").is_some(),
                "Tools capability should be present"
            );
            assert!(
                caps.get("resources").is_none(),
                "Resources capability should NOT be present"
            );
            assert!(
                caps.get("prompts").is_none(),
                "Prompts capability should NOT be present"
            );
        }
        JsonRpcResponse::Error(e) => panic!("Unexpected error: {:?}", e),
    }
}

#[tokio::test]
async fn test_capabilities_all() {
    let router = create_router_with_resources_and_prompts(); // Has tools, resources, and prompts
    let mut service = JsonRpcService::new(router);

    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "1.0" }
    }));
    let resp = service.call_single(init_req).await.unwrap();

    match resp {
        JsonRpcResponse::Result(r) => {
            let caps = r.result.get("capabilities").unwrap();
            assert!(
                caps.get("tools").is_some(),
                "Tools capability should be present"
            );
            assert!(
                caps.get("resources").is_some(),
                "Resources capability should be present"
            );
            assert!(
                caps.get("prompts").is_some(),
                "Prompts capability should be present"
            );
        }
        JsonRpcResponse::Error(e) => panic!("Unexpected error: {:?}", e),
    }
}

// =============================================================================
// TestClient tests (#124)
// =============================================================================

#[cfg(feature = "testing")]
mod test_client_tests {
    use super::*;
    use tower_mcp::TestClient;

    #[tokio::test]
    async fn test_client_initialize_and_list_tools() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);

        let init = client.initialize().await;
        assert!(init.get("protocolVersion").is_some());
        assert!(init.get("serverInfo").is_some());

        let tools = client.list_tools().await;
        assert_eq!(tools.len(), 3);
    }

    #[tokio::test]
    async fn test_client_call_tool_text() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let result = client
            .call_tool("echo", serde_json::json!({"message": "hello"}))
            .await;
        assert_eq!(result.all_text(), "hello");
        assert_eq!(result.first_text(), Some("hello"));
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn test_client_call_tool_add() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let result = client
            .call_tool("add", serde_json::json!({"a": 17, "b": 25}))
            .await;
        assert_eq!(result.all_text(), "42");
    }

    #[tokio::test]
    async fn test_client_call_tool_expect_error() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        // Non-existent tool returns JSON-RPC error
        let error = client
            .call_tool_expect_error("nonexistent", serde_json::json!({}))
            .await;
        assert!(error.get("code").is_some() || error.get("message").is_some());
    }

    #[tokio::test]
    async fn test_client_resources() {
        let router = create_router_with_resources_and_prompts();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let resources = client.list_resources().await;
        assert_eq!(resources.len(), 3);

        let result = client.read_resource("file:///README.md").await;
        assert_eq!(
            result.first_text(),
            Some("# Test Project\n\nThis is a test project.")
        );
        assert_eq!(result.first_uri(), Some("file:///README.md"));
    }

    #[tokio::test]
    async fn test_client_prompts() {
        let router = create_router_with_resources_and_prompts();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let prompts = client.list_prompts().await;
        assert_eq!(prompts.len(), 3);

        let mut args = HashMap::new();
        args.insert("name".to_string(), "Alice".to_string());
        let result = client.get_prompt("greet", args).await;
        let text = result.first_message_text().unwrap();
        assert!(text.contains("Alice"));
    }

    #[tokio::test]
    async fn test_client_send_request_expect_error() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let error = client
            .send_request_expect_error("unknown/method", None)
            .await;
        let code = error.get("code").and_then(|v| v.as_i64()).unwrap();
        assert_eq!(code, -32601); // Method not found
    }

    #[tokio::test]
    async fn test_client_raw_request() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);
        client.initialize().await;

        let raw = client
            .call_tool_raw("echo", serde_json::json!({"message": "raw"}))
            .await;
        let content = raw.get("content").unwrap().as_array().unwrap();
        assert_eq!(content[0].get("text").unwrap().as_str().unwrap(), "raw");
    }

    #[tokio::test]
    async fn test_client_ping() {
        let router = create_test_router();
        let mut client = TestClient::from_router(router);

        // Ping works even before initialization
        let _result = client.send_request("ping", None).await;
    }

    #[tokio::test]
    async fn test_content_as_text() {
        use tower_mcp::Content;

        let text_content = Content::Text {
            text: "hello".to_string(),
            annotations: None,
        };
        assert_eq!(text_content.as_text(), Some("hello"));

        let image_content = Content::Image {
            data: "abc".to_string(),
            mime_type: "image/png".to_string(),
            annotations: None,
        };
        assert_eq!(image_content.as_text(), None);
    }

    #[tokio::test]
    async fn test_read_resource_result_accessors() {
        let result = ReadResourceResult::text("file://test.txt", "content here");
        assert_eq!(result.first_text(), Some("content here"));
        assert_eq!(result.first_uri(), Some("file://test.txt"));
    }

    #[tokio::test]
    async fn test_get_prompt_result_accessor() {
        let result = GetPromptResult::user_message("Analyze this.");
        assert_eq!(result.first_message_text(), Some("Analyze this."));
    }
}

// =============================================================================
// Scope enforcement tests (#127)
// =============================================================================

#[cfg(feature = "oauth")]
mod scope_enforcement_tests {
    use super::*;
    use tower_mcp::oauth::{ScopeEnforcementLayer, ScopePolicy, TokenClaims};
    use tower_mcp::transport::service::CatchError;

    /// Helper: create a JsonRpcService with a scope enforcement middleware,
    /// optionally injecting TokenClaims into extensions.
    fn make_service_with_scope(
        router: McpRouter,
        policy: ScopePolicy,
        claims: Option<TokenClaims>,
    ) -> JsonRpcService<
        CatchError<
            tower::util::BoxCloneService<
                tower_mcp::RouterRequest,
                tower_mcp::RouterResponse,
                std::convert::Infallible,
            >,
        >,
    > {
        use tower::ServiceBuilder;

        let svc = ServiceBuilder::new()
            .layer_fn(CatchError::new)
            .layer(ScopeEnforcementLayer::new(policy))
            .service(router);

        let boxed = tower::util::BoxCloneService::new(svc);
        let wrapped = CatchError::new(boxed);

        let mut ext = Extensions::new();
        if let Some(c) = claims {
            ext.insert(c);
        }

        JsonRpcService::new(wrapped).with_extensions(ext)
    }

    fn test_claims(scopes: &str) -> TokenClaims {
        TokenClaims {
            sub: Some("test-user".to_string()),
            iss: None,
            aud: None,
            exp: None,
            scope: Some(scopes.to_string()),
            client_id: None,
            extra: HashMap::new(),
        }
    }

    async fn initialize(
        router: &McpRouter,
        service: &mut JsonRpcService<
            CatchError<
                tower::util::BoxCloneService<
                    tower_mcp::RouterRequest,
                    tower_mcp::RouterResponse,
                    std::convert::Infallible,
                >,
            >,
        >,
    ) {
        let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1.0" }
        }));
        service.call_single(init_req).await.unwrap();
        router.handle_notification(tower_mcp::protocol::McpNotification::Initialized);
    }

    #[tokio::test]
    async fn test_scope_enforcement_blocks_tool_without_required_scope() {
        let router = create_test_router();
        let policy = ScopePolicy::new().tool_scope("echo", "mcp:admin");
        let claims = test_claims("mcp:read");
        let mut service = make_service_with_scope(router.clone(), policy, Some(claims));

        initialize(&router, &mut service).await;

        let call_req = JsonRpcRequest::new(2, "tools/call").with_params(serde_json::json!({
            "name": "echo",
            "arguments": { "message": "hello" }
        }));
        let resp = service.call_single(call_req).await.unwrap();

        match resp {
            JsonRpcResponse::Error(e) => {
                assert_eq!(e.error.code, -32007); // Forbidden
                assert!(
                    e.error.message.contains("insufficient"),
                    "Expected 'insufficient' in error message, got: {}",
                    e.error.message
                );
            }
            JsonRpcResponse::Result(_) => panic!("Expected forbidden error"),
        }
    }

    #[tokio::test]
    async fn test_scope_enforcement_allows_tool_with_required_scope() {
        let router = create_test_router();
        let policy = ScopePolicy::new().tool_scope("echo", "mcp:read");
        let claims = test_claims("mcp:read mcp:write");
        let mut service = make_service_with_scope(router.clone(), policy, Some(claims));

        initialize(&router, &mut service).await;

        let call_req = JsonRpcRequest::new(2, "tools/call").with_params(serde_json::json!({
            "name": "echo",
            "arguments": { "message": "hello" }
        }));
        let resp = service.call_single(call_req).await.unwrap();

        match resp {
            JsonRpcResponse::Result(r) => {
                let content = r.result.get("content").unwrap().as_array().unwrap();
                let text = content[0].get("text").unwrap().as_str().unwrap();
                assert_eq!(text, "hello");
            }
            JsonRpcResponse::Error(e) => panic!("Expected success, got error: {:?}", e),
        }
    }

    #[tokio::test]
    async fn test_scope_enforcement_pass_through_without_claims() {
        let router = create_test_router();
        let policy = ScopePolicy::new().tool_scope("echo", "mcp:admin");
        // No claims -- should pass through
        let mut service = make_service_with_scope(router.clone(), policy, None);

        initialize(&router, &mut service).await;

        let call_req = JsonRpcRequest::new(2, "tools/call").with_params(serde_json::json!({
            "name": "echo",
            "arguments": { "message": "no auth" }
        }));
        let resp = service.call_single(call_req).await.unwrap();

        match resp {
            JsonRpcResponse::Result(r) => {
                let content = r.result.get("content").unwrap().as_array().unwrap();
                let text = content[0].get("text").unwrap().as_str().unwrap();
                assert_eq!(text, "no auth");
            }
            JsonRpcResponse::Error(e) => panic!("Expected pass-through, got error: {:?}", e),
        }
    }

    #[tokio::test]
    async fn test_scope_enforcement_resource_scope() {
        let router = create_router_with_resources_and_prompts();
        let policy = ScopePolicy::new().resource_scope("file:///config.json", "mcp:config");
        let claims = test_claims("mcp:read");
        let mut service = make_service_with_scope(router.clone(), policy, Some(claims));

        initialize(&router, &mut service).await;

        let read_req = JsonRpcRequest::new(2, "resources/read").with_params(serde_json::json!({
            "uri": "file:///config.json"
        }));
        let resp = service.call_single(read_req).await.unwrap();

        match resp {
            JsonRpcResponse::Error(e) => {
                assert_eq!(e.error.code, -32007);
            }
            JsonRpcResponse::Result(_) => panic!("Expected forbidden error for resource"),
        }
    }

    #[tokio::test]
    async fn test_scope_enforcement_prompt_scope() {
        let router = create_router_with_resources_and_prompts();
        let policy = ScopePolicy::new().prompt_scope("greet", "mcp:prompt");
        let claims = test_claims("mcp:read");
        let mut service = make_service_with_scope(router.clone(), policy, Some(claims));

        initialize(&router, &mut service).await;

        let get_req = JsonRpcRequest::new(2, "prompts/get").with_params(serde_json::json!({
            "name": "greet",
            "arguments": { "name": "Alice" }
        }));
        let resp = service.call_single(get_req).await.unwrap();

        match resp {
            JsonRpcResponse::Error(e) => {
                assert_eq!(e.error.code, -32007);
            }
            JsonRpcResponse::Result(_) => panic!("Expected forbidden error for prompt"),
        }
    }

    #[tokio::test]
    async fn test_scope_enforcement_default_scope() {
        let router = create_test_router();
        let policy = ScopePolicy::new().default_scope("mcp:base");
        let claims = test_claims("mcp:other");
        let mut service = make_service_with_scope(router.clone(), policy, Some(claims));

        // Even initialization should require the default scope since
        // it goes through the middleware. But initialize uses McpRequest::Initialize
        // which falls into the default check.
        let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1.0" }
        }));
        let resp = service.call_single(init_req).await.unwrap();

        match resp {
            JsonRpcResponse::Error(e) => {
                assert_eq!(e.error.code, -32007);
            }
            JsonRpcResponse::Result(_) => panic!("Expected forbidden error for default scope"),
        }
    }
}
