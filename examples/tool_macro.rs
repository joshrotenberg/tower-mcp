//! Example: using the `#[tool_fn]`, `#[prompt_fn]`, `#[resource_fn]`, and
//! `#[resource_template_fn]` proc macros.
//!
//! The macros generate constructor functions that return `Tool` / `Prompt` /
//! `Resource` / `ResourceTemplate` built via the corresponding builders.
//! This is syntactic sugar -- the builders are the real API, and you can
//! always eject for full control.
//!
//! Run with: `cargo run --example tool_macro --features macros`

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::prompt_fn;
use tower_mcp::protocol::{GetPromptResult, ReadResourceResult};
use tower_mcp::resource_fn;
use tower_mcp::resource_template_fn;
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

// --- Define resources using the macros ---

#[resource_fn(uri = "app://config", description = "App configuration")]
async fn config() -> Result<ReadResourceResult, tower_mcp::Error> {
    Ok(ReadResourceResult::text("app://config", "debug=true"))
}

#[resource_template_fn(
    uri_template = "file:///{+path}",
    name = "file",
    description = "Read a file by path"
)]
async fn read_file(
    uri: String,
    vars: HashMap<String, String>,
) -> Result<ReadResourceResult, tower_mcp::Error> {
    let path = vars.get("path").cloned().unwrap_or_default();
    Ok(ReadResourceResult::text(uri, format!("contents of {path}")))
}

// --- Use them like any other tool/prompt/resource ---

fn main() {
    let _router = McpRouter::new()
        .server_info("macro-example", "1.0.0")
        .tool(add_tool())
        .tool(greet_tool())
        .prompt(review_prompt())
        .resource(config_resource())
        .resource_template(read_file_resource_template());

    println!("Registered via macros:");
    println!("  Tools:");
    println!("    - add (from #[tool_fn])");
    println!("    - say-hello (from #[tool_fn] with custom name)");
    println!("  Prompts:");
    println!("    - review (from #[prompt_fn])");
    println!("  Resources:");
    println!("    - app://config (from #[resource_fn])");
    println!("  Resource Templates:");
    println!("    - file:///{{+path}} (from #[resource_template_fn])");
    println!();
    println!("All identical to items built with the builder APIs.");
    println!("Use `cargo expand --example tool_macro` to see the generated code.");
}
