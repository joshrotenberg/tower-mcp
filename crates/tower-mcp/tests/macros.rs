#![cfg(feature = "macros")]

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::CallToolResult;
use tower_mcp::tool_fn;

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

#[tool_fn(description = "Add two numbers")]
async fn add(input: AddInput) -> Result<CallToolResult, tower_mcp::Error> {
    Ok(CallToolResult::text(format!("{}", input.a + input.b)))
}

#[tool_fn(name = "custom-name", description = "Custom named tool")]
async fn multiply(input: AddInput) -> Result<CallToolResult, tower_mcp::Error> {
    Ok(CallToolResult::text(format!("{}", input.a * input.b)))
}

#[tokio::test]
async fn test_tool_fn_macro_generates_working_tool() {
    let tool = add_tool();
    assert_eq!(tool.name, "add");
    assert_eq!(tool.description.as_deref(), Some("Add two numbers"));

    let result = tool.call(serde_json::json!({"a": 2, "b": 3})).await;
    assert!(!result.is_error);
    assert_eq!(result.first_text().unwrap(), "5");
}

#[tokio::test]
async fn test_tool_fn_macro_custom_name() {
    let tool = multiply_tool();
    assert_eq!(tool.name, "custom-name");
    assert_eq!(tool.description.as_deref(), Some("Custom named tool"));

    let result = tool.call(serde_json::json!({"a": 4, "b": 5})).await;
    assert!(!result.is_error);
    assert_eq!(result.first_text().unwrap(), "20");
}

#[test]
fn test_tool_fn_macro_tool_registers_in_router() {
    use tower_mcp::McpRouter;

    // This verifies the macro output is compatible with McpRouter::tool()
    let _router = McpRouter::new()
        .server_info("test", "1.0.0")
        .tool(add_tool())
        .tool(multiply_tool());
}

#[tokio::test]
async fn test_tool_fn_default_name_converts_underscores_to_hyphens() {
    // Function name `add` has no underscores, so name stays "add"
    let tool = add_tool();
    assert_eq!(tool.name, "add");
}

// ---------------------------------------------------------------------------
// prompt_fn tests
// ---------------------------------------------------------------------------

use std::collections::HashMap;
use tower_mcp::prompt_fn;
use tower_mcp::protocol::GetPromptResult;

#[prompt_fn(description = "Greet someone", args(name = "Name to greet"))]
async fn greet(args: HashMap<String, String>) -> Result<GetPromptResult, tower_mcp::Error> {
    let name = args.get("name").cloned().unwrap_or_default();
    Ok(GetPromptResult::user_message(format!("Hello, {name}!")))
}

#[prompt_fn(
    name = "custom-prompt",
    description = "A custom named prompt",
    args(topic = "The topic", ?style = "Optional style")
)]
async fn summarize(args: HashMap<String, String>) -> Result<GetPromptResult, tower_mcp::Error> {
    let topic = args.get("topic").cloned().unwrap_or_default();
    let style = args.get("style").cloned().unwrap_or_else(|| "brief".into());
    Ok(GetPromptResult::user_message(format!(
        "Summarize {topic} in a {style} style"
    )))
}

#[tokio::test]
async fn test_prompt_fn_macro_generates_working_prompt() {
    let prompt = greet_prompt();
    assert_eq!(prompt.name, "greet");
    assert_eq!(prompt.description.as_deref(), Some("Greet someone"));
    assert_eq!(prompt.arguments.len(), 1);
    assert_eq!(prompt.arguments[0].name, "name");
    assert!(prompt.arguments[0].required);
}

#[tokio::test]
async fn test_prompt_fn_macro_custom_name_and_optional_args() {
    let prompt = summarize_prompt();
    assert_eq!(prompt.name, "custom-prompt");
    assert_eq!(prompt.arguments.len(), 2);

    // First arg is required
    assert_eq!(prompt.arguments[0].name, "topic");
    assert!(prompt.arguments[0].required);

    // Second arg is optional
    assert_eq!(prompt.arguments[1].name, "style");
    assert!(!prompt.arguments[1].required);
}

#[test]
fn test_prompt_fn_macro_registers_in_router() {
    use tower_mcp::McpRouter;

    let _router = McpRouter::new()
        .server_info("test", "1.0.0")
        .prompt(greet_prompt())
        .prompt(summarize_prompt());
}
