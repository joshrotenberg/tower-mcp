//! Stdio MCP server example
//!
//! This example creates a simple MCP server that can be used with
//! Claude Desktop, Claude Code, or any other MCP client.
//!
//! Run with: cargo run --example stdio_server
//!
//! Test with: echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}' | cargo run --example stdio_server

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{CallToolResult, McpRouter, ResourceBuilder, StdioTransport, ToolBuilder};

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
    // Initialize tracing for debug output (to stderr so it doesn't interfere with JSON-RPC)
    tracing_subscriber::fmt()
        .with_env_filter("tower_mcp=debug")
        .with_writer(std::io::stderr)
        .init();

    // Build tools
    let echo = ToolBuilder::new("echo")
        .description("Echo a message back")
        .read_only()
        .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.message)) })
        .build()?;

    let add = ToolBuilder::new("add")
        .description("Add two numbers together")
        .read_only()
        .idempotent()
        .handler(|input: AddInput| async move {
            let result = input.a + input.b;
            Ok(CallToolResult::text(format!("{}", result)))
        })
        .build()?;

    let reverse = ToolBuilder::new("reverse")
        .description("Reverse a string")
        .read_only()
        .idempotent()
        .handler(|input: ReverseInput| async move {
            let reversed: String = input.text.chars().rev().collect();
            Ok(CallToolResult::text(reversed))
        })
        .build()?;

    // Create a resource that serves this file's own source code
    let source = ResourceBuilder::new("source://stdio_server.rs")
        .name("Server Source Code")
        .description("The complete source code for this example server")
        .mime_type("text/x-rust")
        .text(include_str!("stdio_server.rs"));

    // Create router
    let router = McpRouter::new()
        .server_info("tower-mcp-example", env!("CARGO_PKG_VERSION"))
        .instructions(
            "A simple example MCP server with echo, add, and reverse tools. \
             Read the source://stdio_server.rs resource to see how it's built.",
        )
        .tool(echo)
        .tool(add)
        .tool(reverse)
        .resource(source);

    // Create and run stdio transport
    let mut transport = StdioTransport::new(router);

    tracing::info!("Starting stdio server");
    transport.run().await?;

    Ok(())
}
