//! Getting started with tower-mcp
//!
//! This example covers the fundamentals of building an MCP server:
//!
//! 1. **Tools** -- define input types, handlers, and annotations
//! 2. **Resources** -- serve static content
//! 3. **Prompts** -- reusable prompt templates
//! 4. **Transport** -- serve over stdio for use with Claude Desktop, Claude Code, etc.
//!
//! Run with: cargo run --example getting_started
//!
//! Test with:
//! ```bash
//! echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}' \
//!   | cargo run --example getting_started
//! ```

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, McpRouter, PromptBuilder, ResourceBuilder, StdioTransport, ToolBuilder,
};

// =============================================================================
// Input types -- schemars generates JSON Schema automatically for the client
// =============================================================================

#[derive(Debug, Deserialize, JsonSchema)]
struct EchoInput {
    /// The message to echo back
    message: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    /// First number
    a: i64,
    /// Second number
    b: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReverseInput {
    /// The text to reverse
    text: String,
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    // Tracing goes to stderr so it doesn't interfere with stdio JSON-RPC
    tracing_subscriber::fmt()
        .with_env_filter("tower_mcp=debug")
        .with_writer(std::io::stderr)
        .init();

    // -- Tools --
    // Each tool has a typed input (derives JsonSchema for automatic schema generation),
    // an async handler, and optional annotations describing its behavior.

    let echo = ToolBuilder::new("echo")
        .description("Echo a message back")
        .read_only()
        .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.message)) })
        .build();

    let add = ToolBuilder::new("add")
        .description("Add two numbers together")
        .read_only()
        .idempotent()
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    let reverse = ToolBuilder::new("reverse")
        .description("Reverse a string")
        .read_only()
        .idempotent()
        .handler(|input: ReverseInput| async move {
            let reversed: String = input.text.chars().rev().collect();
            Ok(CallToolResult::text(reversed))
        })
        .build();

    // -- Resources --
    // Static content served to clients. This one includes the server's own source code.

    let source = ResourceBuilder::new("source://getting_started.rs")
        .name("Server Source Code")
        .description("The complete source code for this example server")
        .mime_type("text/x-rust")
        .text(include_str!("getting_started.rs"));

    // -- Prompts --
    // Reusable prompt templates that clients can request and fill with arguments.

    let greet_prompt = PromptBuilder::new("greet")
        .description("Generate a greeting for someone")
        .required_arg("name", "The person to greet")
        .handler(|args| async move {
            let name = args.get("name").map(|s| s.as_str()).unwrap_or("World");
            Ok(tower_mcp::GetPromptResult::user_message(format!(
                "Please greet {name} warmly."
            )))
        })
        .build();

    // -- Router --
    // Combines all capabilities. auto_instructions_with() generates a capabilities
    // summary that clients see during initialization.

    let router = McpRouter::new()
        .server_info("getting-started", env!("CARGO_PKG_VERSION"))
        .auto_instructions_with(
            Some("A simple example MCP server demonstrating tools, resources, and prompts."),
            None::<String>,
        )
        .tool(echo)
        .tool(add)
        .tool(reverse)
        .resource(source)
        .prompt(greet_prompt);

    // -- Transport --
    // StdioTransport reads JSON-RPC from stdin and writes to stdout.
    // This is the standard transport for Claude Desktop and Claude Code.

    tracing::info!("Starting stdio server");
    StdioTransport::new(router).run().await?;

    Ok(())
}
