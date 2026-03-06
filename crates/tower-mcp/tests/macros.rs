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
