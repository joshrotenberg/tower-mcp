//! Integration tests for tower-mcp
//!
//! Tests the full protocol flow including session lifecycle,
//! tool execution, and error handling.

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, JsonRpcRequest, JsonRpcResponse, JsonRpcService, McpRouter, ToolBuilder,
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
            assert_eq!(e.error.code, -32601); // Method not found (resource not found)
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
