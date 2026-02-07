//! Basic example showing how to create an MCP server with tools.
//!
//! This example demonstrates:
//! - Creating tools with the builder pattern
//! - Setting up an McpRouter
//! - Using the JsonRpcService for protocol framing
//!
//! Note: This example doesn't include a transport layer yet.
//! It shows how to manually process requests for testing.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tower_mcp::{CallToolResult, JsonRpcRequest, JsonRpcService, McpRouter, ToolBuilder};

// Input types for our tools - schemars generates JSON Schema automatically
#[derive(Debug, Deserialize, JsonSchema)]
struct GreetInput {
    /// The name to greet
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    /// First number
    a: i64,
    /// Second number
    b: i64,
}

/// Output type for the add tool - demonstrates structured JSON output
#[derive(Debug, Serialize)]
struct AddOutput {
    result: i64,
    expression: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EchoInput {
    /// Message to echo back
    message: String,
    /// Number of times to repeat (optional)
    #[serde(default = "default_repeat")]
    repeat: u32,
}

fn default_repeat() -> u32 {
    1
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    // Initialize tracing for debug output
    tracing_subscriber::fmt()
        .with_env_filter("tower_mcp=debug")
        .init();

    // Build our tools
    let greet = ToolBuilder::new("greet")
        .description("Greet someone by name")
        .read_only() // This tool doesn't modify any state
        .handler(|input: GreetInput| async move {
            Ok(CallToolResult::text(format!("Hello, {}!", input.name)))
        })
        .build();

    // Demonstrates returning structured JSON using from_serialize
    let add = ToolBuilder::new("add")
        .description("Add two numbers together")
        .read_only()
        .idempotent() // Same inputs always produce same output
        .handler(|input: AddInput| async move {
            let output = AddOutput {
                result: input.a + input.b,
                expression: format!("{} + {} = {}", input.a, input.b, input.a + input.b),
            };
            // Use from_serialize for structured JSON output from any Serialize type
            CallToolResult::from_serialize(&output)
        })
        .build();

    let echo = ToolBuilder::new("echo")
        .description("Echo a message back, optionally repeated")
        .read_only()
        .handler(|input: EchoInput| async move {
            let repeated: Vec<_> = std::iter::repeat_n(&input.message, input.repeat as usize)
                .cloned()
                .collect();
            Ok(CallToolResult::text(repeated.join(" ")))
        })
        .build();

    // Create the router with our tools.
    // auto_instructions() generates instructions from tool descriptions at init time.
    let router = McpRouter::new()
        .server_info("basic-example", "0.1.0")
        .auto_instructions()
        .tool(greet)
        .tool(add)
        .tool(echo);

    // Wrap with JsonRpcService for protocol framing
    let mut service = JsonRpcService::new(router);

    // Simulate some requests (in a real app, these would come from a transport)
    println!("=== Simulating MCP requests ===\n");

    // 1. Initialize
    println!("1. Initialize request:");
    let init_req = JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
        "protocolVersion": "2025-11-25",
        "capabilities": {},
        "clientInfo": {
            "name": "example-client",
            "version": "1.0.0"
        }
    }));
    let resp = service.call_single(init_req).await?;
    println!("   Response: {}\n", serde_json::to_string_pretty(&resp)?);

    // 2. List tools
    println!("2. List tools request:");
    let list_req = JsonRpcRequest::new(2, "tools/list");
    let resp = service.call_single(list_req).await?;
    println!("   Response: {}\n", serde_json::to_string_pretty(&resp)?);

    // 3. Call the greet tool
    println!("3. Call greet tool:");
    let call_req = JsonRpcRequest::new(3, "tools/call").with_params(serde_json::json!({
        "name": "greet",
        "arguments": {
            "name": "World"
        }
    }));
    let resp = service.call_single(call_req).await?;
    println!("   Response: {}\n", serde_json::to_string_pretty(&resp)?);

    // 4. Call the add tool
    println!("4. Call add tool:");
    let call_req = JsonRpcRequest::new(4, "tools/call").with_params(serde_json::json!({
        "name": "add",
        "arguments": {
            "a": 17,
            "b": 25
        }
    }));
    let resp = service.call_single(call_req).await?;
    println!("   Response: {}\n", serde_json::to_string_pretty(&resp)?);

    // 5. Call the echo tool with repeat
    println!("5. Call echo tool:");
    let call_req = JsonRpcRequest::new(5, "tools/call").with_params(serde_json::json!({
        "name": "echo",
        "arguments": {
            "message": "tower-mcp!",
            "repeat": 3
        }
    }));
    let resp = service.call_single(call_req).await?;
    println!("   Response: {}\n", serde_json::to_string_pretty(&resp)?);

    // 6. Ping
    println!("6. Ping request:");
    let ping_req = JsonRpcRequest::new(6, "ping");
    let resp = service.call_single(ping_req).await?;
    println!("   Response: {}\n", serde_json::to_string_pretty(&resp)?);

    // 7. Try calling a non-existent tool
    println!("7. Call non-existent tool (error case):");
    let call_req = JsonRpcRequest::new(7, "tools/call").with_params(serde_json::json!({
        "name": "does_not_exist",
        "arguments": {}
    }));
    let resp = service.call_single(call_req).await?;
    println!("   Response: {}\n", serde_json::to_string_pretty(&resp)?);

    Ok(())
}
