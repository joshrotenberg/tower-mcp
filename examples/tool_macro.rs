//! Example: using the `#[tool_fn]` and `#[prompt_fn]` proc macros.
//!
//! The macros generate constructor functions that return `Tool` / `Prompt`
//! built via `ToolBuilder` / `PromptBuilder`. This is syntactic sugar -- the
//! builders are the real API, and you can always eject for full control.
//!
//! Run with: `cargo run --example tool_macro --features macros`

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::prompt_fn;
use tower_mcp::protocol::GetPromptResult;
use tower_mcp::tool_fn;
use tower_mcp::{CallToolResult, McpRouter};

// --- Define tools using the macro ---

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    /// First number
    a: i64,
    /// Second number
    b: i64,
}

#[tool_fn(description = "Add two numbers")]
async fn add(input: AddInput) -> Result<CallToolResult, tower_mcp::Error> {
    Ok(CallToolResult::text(format!("{}", input.a + input.b)))
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GreetInput {
    /// Name to greet
    name: String,
}

#[tool_fn(name = "say-hello", description = "Greet someone by name")]
async fn greet(input: GreetInput) -> Result<CallToolResult, tower_mcp::Error> {
    Ok(CallToolResult::text(format!("Hello, {}!", input.name)))
}

// --- Define prompts using the macro ---

#[prompt_fn(description = "Review code in a given language", args(code = "Code to review", ?language = "Programming language"))]
async fn review(args: HashMap<String, String>) -> Result<GetPromptResult, tower_mcp::Error> {
    let code = args.get("code").cloned().unwrap_or_default();
    let language = args
        .get("language")
        .cloned()
        .unwrap_or_else(|| "unknown".into());
    Ok(GetPromptResult::user_message(format!(
        "Please review this {language} code:\n\n```\n{code}\n```"
    )))
}

// --- Use them like any other tool/prompt ---

fn main() {
    let _router = McpRouter::new()
        .server_info("macro-example", "1.0.0")
        .tool(add_tool())
        .tool(greet_tool())
        .prompt(review_prompt());

    println!("Registered via macros:");
    println!("  Tools:");
    println!("    - add (from #[tool_fn])");
    println!("    - say-hello (from #[tool_fn] with custom name)");
    println!("  Prompts:");
    println!("    - review (from #[prompt_fn])");
    println!();
    println!("These are identical to items built with ToolBuilder/PromptBuilder.");
    println!("Use `cargo expand --example tool_macro` to see the generated code.");
}
