//! Example: using the `#[tool_fn]` proc macro.
//!
//! The macro generates a `{fn_name}_tool()` constructor that returns a `Tool`
//! built via `ToolBuilder`. This is syntactic sugar -- the builder is the
//! real API, and you can always eject to it for full control.
//!
//! Run with: `cargo run --example tool_macro --features macros`

use schemars::JsonSchema;
use serde::Deserialize;
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

// --- Use them like any other tool ---

fn main() {
    // The macro generates add_tool() and greet_tool() functions
    let _router = McpRouter::new()
        .server_info("macro-example", "1.0.0")
        .tool(add_tool())
        .tool(greet_tool());

    println!("Tools registered:");
    println!("  - add (from #[tool_fn])");
    println!("  - say-hello (from #[tool_fn] with custom name)");
    println!();
    println!("These are identical to tools built with ToolBuilder.");
    println!("Use `cargo expand --example tool_macro` to see the generated code.");
}
