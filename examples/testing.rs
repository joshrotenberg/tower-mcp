//! In-process testing example
//!
//! Demonstrates using `TestClient` to test MCP servers without spawning a
//! subprocess or opening a network connection. TestClient handles JSON-RPC
//! framing, request IDs, and protocol initialization -- you just call methods
//! and assert on results.
//!
//! Run with: cargo run --example testing --features testing

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use tower_mcp::{
    CallToolResult, GetPromptResult, McpRouter, PromptBuilder, ResourceBuilder, TestClient,
    ToolBuilder,
};

#[derive(Debug, Deserialize, JsonSchema)]
struct GreetInput {
    /// Name to greet
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

/// A typed result for demonstrating call_tool_typed()
#[derive(Debug, Deserialize, PartialEq)]
struct AddResult {
    sum: i64,
}

fn build_router() -> McpRouter {
    let greet = ToolBuilder::new("greet")
        .description("Greet someone by name")
        .handler(|input: GreetInput| async move {
            Ok(CallToolResult::text(format!("Hello, {}!", input.name)))
        })
        .build();

    let add_json = ToolBuilder::new("add_json")
        .description("Add two numbers and return structured JSON")
        .read_only()
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::from_serialize(&serde_json::json!({
                "sum": input.a + input.b
            }))
            .unwrap())
        })
        .build();

    let readme = ResourceBuilder::new("file:///README.md")
        .name("README")
        .description("Project readme")
        .text("# My Project\n\nA sample project.");

    let welcome = PromptBuilder::new("welcome")
        .description("Generate a welcome message")
        .required_arg("name", "User's name")
        .handler(|args: HashMap<String, String>| async move {
            let name = args.get("name").map(|s| s.as_str()).unwrap_or("User");
            Ok(GetPromptResult::user_message(format!(
                "Welcome, {name}! How can I help you today?"
            )))
        })
        .build();

    McpRouter::new()
        .server_info("test-server", "1.0.0")
        .tool(greet)
        .tool(add_json)
        .resource(readme)
        .prompt(welcome)
}

#[tokio::main]
async fn main() {
    let router = build_router();
    let mut client = TestClient::from_router(router);

    // --- Initialize ---
    // Must be called first; sends initialize + initialized notification
    let init = client.initialize().await;
    println!(
        "Initialized: {}",
        serde_json::to_string_pretty(&init).unwrap()
    );

    // --- Tools ---
    let tools = client.list_tools().await;
    assert_eq!(tools.len(), 2);
    println!("\nTools: {}", serde_json::to_string_pretty(&tools).unwrap());

    // call_tool() returns CallToolResult
    let result = client.call_tool("greet", json!({"name": "World"})).await;
    assert_eq!(result.all_text(), "Hello, World!");
    println!("\ncall_tool: {}", result.all_text());

    // call_tool_typed() deserializes into your type
    let result: AddResult = client
        .call_tool_typed("add_json", json!({"a": 17, "b": 25}))
        .await;
    assert_eq!(result, AddResult { sum: 42 });
    println!("call_tool_typed: sum = {}", result.sum);

    // call_tool_expect_error() asserts the call fails
    let err = client
        .call_tool_expect_error("nonexistent_tool", json!({}))
        .await;
    println!("Expected error: {}", err);

    // --- Resources ---
    let resources = client.list_resources().await;
    assert_eq!(resources.len(), 1);

    let readme = client.read_resource("file:///README.md").await;
    assert_eq!(
        readme.first_text(),
        Some("# My Project\n\nA sample project.")
    );
    println!("\nResource text: {}", readme.first_text().unwrap());

    // --- Prompts ---
    let prompts = client.list_prompts().await;
    assert_eq!(prompts.len(), 1);

    let mut args = HashMap::new();
    args.insert("name".to_string(), "Alice".to_string());
    let prompt = client.get_prompt("welcome", args).await;
    assert!(
        prompt
            .first_message_text()
            .unwrap()
            .contains("Welcome, Alice!")
    );
    println!("Prompt message: {}", prompt.first_message_text().unwrap());

    println!("\nAll assertions passed.");
}
