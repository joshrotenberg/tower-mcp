//! Unix Domain Socket server example for tower-mcp
//!
//! Run with: cargo run --example unix_socket_server --features unix
//!
//! Test with curl (requires curl 7.40+ for --unix-socket):
//! ```bash
//! # Initialize session
//! curl --unix-socket /tmp/tower-mcp.sock \
//!   -X POST http://localhost/ \
//!   -H "Content-Type: application/json" \
//!   -H "Accept: application/json, text/event-stream" \
//!   -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"1.0"}}}'
//!
//! # List tools (use session ID from initialize response)
//! curl --unix-socket /tmp/tower-mcp.sock \
//!   -X POST http://localhost/ \
//!   -H "Content-Type: application/json" \
//!   -H "MCP-Session-Id: <session-id>" \
//!   -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
//!
//! # Call a tool
//! curl --unix-socket /tmp/tower-mcp.sock \
//!   -X POST http://localhost/ \
//!   -H "Content-Type: application/json" \
//!   -H "MCP-Session-Id: <session-id>" \
//!   -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"add","arguments":{"a":10,"b":32}}}'
//! ```

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tower::timeout::TimeoutLayer;
use tower_mcp::{
    CallToolResult, McpRouter, PromptBuilder, ResourceBuilder, ToolBuilder, UnixSocketTransport,
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
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tower_mcp=debug".parse()?),
        )
        .init();

    let add = ToolBuilder::new("add")
        .description("Add two numbers together")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    let echo = ToolBuilder::new("echo")
        .description("Echo a message back")
        .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.message)) })
        .build();

    let config = ResourceBuilder::new("file:///config.json")
        .name("Configuration")
        .description("Server configuration")
        .json(serde_json::json!({
            "version": "1.0.0",
            "transport": "unix-socket"
        }));

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
                        meta: None,
                    },
                    meta: None,
                }],
                meta: None,
            })
        })
        .build();

    let router = McpRouter::new()
        .server_info("unix-socket-example", "1.0.0")
        .auto_instructions()
        .tool(add)
        .tool(echo)
        .resource(config)
        .prompt(greet);

    let socket_path = "/tmp/tower-mcp.sock";

    let transport = UnixSocketTransport::new(router)
        .disable_origin_validation()
        .layer(TimeoutLayer::new(Duration::from_secs(30)));

    tracing::info!("Starting Unix socket MCP server on {}", socket_path);
    transport.serve(socket_path).await?;

    Ok(())
}
