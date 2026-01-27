//! HTTP server example for tower-mcp
//!
//! Run with: cargo run --example http_server --features http
//!
//! Test with curl:
//! ```bash
//! # Initialize session
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -H "Accept: application/json, text/event-stream" \
//!   -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"curl","version":"1.0"}}}'
//!
//! # List tools (use session ID from initialize response)
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -H "MCP-Session-Id: <session-id>" \
//!   -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
//!
//! # Call a tool
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -H "MCP-Session-Id: <session-id>" \
//!   -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"add","arguments":{"a":10,"b":32}}}'
//! ```

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, HttpTransport, McpRouter, PromptBuilder, ResourceBuilder, ToolBuilder,
};

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EchoInput {
    message: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tower_mcp=debug".parse()?),
        )
        .init();

    // Create tools
    let add = ToolBuilder::new("add")
        .description("Add two numbers together")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build()?;

    let echo = ToolBuilder::new("echo")
        .description("Echo a message back")
        .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.message)) })
        .build()?;

    // Create resources
    let config = ResourceBuilder::new("file:///config.json")
        .name("Configuration")
        .description("Server configuration")
        .json(serde_json::json!({
            "version": "1.0.0",
            "debug": true,
            "features": ["add", "echo"]
        }));

    // Create prompts
    let greet = PromptBuilder::new("greet")
        .description("Generate a greeting")
        .required_arg("name", "Name to greet")
        .handler(|args| async move {
            let name = args.get("name").map(|s| s.as_str()).unwrap_or("World");
            Ok(tower_mcp::GetPromptResult {
                description: Some("A friendly greeting".to_string()),
                messages: vec![tower_mcp::PromptMessage {
                    role: tower_mcp::PromptRole::User,
                    content: tower_mcp::protocol::Content::Text {
                        text: format!("Please greet {} warmly.", name),
                        annotations: None,
                    },
                }],
            })
        });

    // Create the MCP router
    let router = McpRouter::new()
        .server_info("http-example", "1.0.0")
        .instructions("A simple HTTP MCP server example with add and echo tools.")
        .tool(add)
        .tool(echo)
        .resource(config)
        .prompt(greet);

    // Create and configure the HTTP transport
    let transport = HttpTransport::new(router)
        // For local development, disable origin validation
        // In production, use .allowed_origins(vec!["https://your-domain.com".to_string()])
        .disable_origin_validation();

    // Serve on localhost:3000
    tracing::info!("Starting HTTP MCP server on http://127.0.0.1:3000");
    transport.serve("127.0.0.1:3000").await?;

    Ok(())
}
