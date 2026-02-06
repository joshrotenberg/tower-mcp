//! HTTP server example for tower-mcp
//!
//! Run with: cargo run --example http_server --features http
//!
//! Test with curl:
//! ```bash
//! # Initialize session (client sends protocolVersion)
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -H "Accept: application/json, text/event-stream" \
//!   -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"1.0"}}}'
//!
//! # Server responds with negotiated protocolVersion and its capabilities:
//! # {
//! #   "jsonrpc": "2.0",
//! #   "id": 1,
//! #   "result": {
//! #     "protocolVersion": "2025-11-25",
//! #     "serverInfo": { "name": "http-example", "version": "1.0.0" },
//! #     "capabilities": { "tools": {}, "resources": {}, "prompts": {} }
//! #   }
//! # }
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
//!
//! # Subscribe to SSE stream for notifications (progress, logs, etc.)
//! # Each event includes an ID for stream resumption (SEP-1699):
//! #   id: 0
//! #   event: message
//! #   data: {"jsonrpc":"2.0","method":"notifications/progress",...}
//! curl -N http://localhost:3000/ \
//!   -H "Accept: text/event-stream" \
//!   -H "MCP-Session-Id: <session-id>"
//!
//! # Reconnect with Last-Event-ID to replay missed events:
//! curl -N http://localhost:3000/ \
//!   -H "Accept: text/event-stream" \
//!   -H "MCP-Session-Id: <session-id>" \
//!   -H "Last-Event-ID: 5"
//! ```

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tower::timeout::TimeoutLayer;
use tower_mcp::{
    CallToolResult, HttpTransport, McpRouter, PromptBuilder, ResourceBuilder, ToolBuilder,
    extract::{Context, Json},
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

/// Input for the slow_task tool
#[derive(Debug, Deserialize, JsonSchema)]
struct SlowTaskInput {
    /// Number of steps to simulate (default: 5)
    #[serde(default = "default_steps")]
    steps: u32,
    /// Delay between steps in milliseconds (default: 500)
    #[serde(default = "default_delay_ms")]
    delay_ms: u64,
}

fn default_steps() -> u32 {
    5
}

fn default_delay_ms() -> u64 {
    500
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
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

    // A tool that simulates work and sends progress notifications
    // Useful for demonstrating SSE event streaming and Last-Event-ID resumption
    let slow_task = ToolBuilder::new("slow_task")
        .description("Simulate a slow task that sends progress notifications (for SSE demo)")
        .extractor_handler(
            (),
            |ctx: Context, Json(input): Json<SlowTaskInput>| async move {
                let steps = input.steps.min(20); // Cap at 20 steps
                let delay = Duration::from_millis(input.delay_ms.min(2000)); // Cap delay at 2s

                for i in 0..steps {
                    // Send progress notification (appears on SSE stream with event ID)
                    ctx.report_progress(
                        i as f64,
                        Some(steps as f64),
                        Some(&format!("Step {}/{}", i + 1, steps)),
                    )
                    .await;

                    tokio::time::sleep(delay).await;
                }

                // Final progress
                ctx.report_progress(steps as f64, Some(steps as f64), Some("Complete!"))
                    .await;

                Ok(CallToolResult::text(format!(
                    "Completed {} steps (each SSE event has a unique ID for resumption)",
                    steps
                )))
            },
        )
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
        })
        .build();

    // Create the MCP router
    let router = McpRouter::new()
        .server_info("http-example", "1.0.0")
        .instructions("A simple HTTP MCP server example with add, echo, and slow_task tools.")
        .tool(add)
        .tool(echo)
        .tool(slow_task)
        .resource(config)
        .prompt(greet);

    // Create and configure the HTTP transport
    let transport = HttpTransport::new(router)
        // For local development, disable origin validation
        // In production, use .allowed_origins(vec!["https://your-domain.com".to_string()])
        .disable_origin_validation()
        // Apply a 30-second timeout to MCP request processing.
        // This is separate from any axum-level middleware on into_router().
        .layer(TimeoutLayer::new(Duration::from_secs(30)));

    // Serve on localhost:3000
    tracing::info!("Starting HTTP MCP server on http://127.0.0.1:3000");
    transport.serve("127.0.0.1:3000").await?;

    Ok(())
}
