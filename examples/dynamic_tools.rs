//! Dynamic tools example showing meta-tools that create/remove tools at runtime.
//!
//! This example demonstrates:
//! - Using `DynamicToolRegistry` for runtime tool registration
//! - Building meta-tools (`create_tool`, `remove_tool`) that manage other tools
//! - `ToolsListChanged` notifications when the tool set changes
//!
//! Run with: `cargo run --example dynamic_tools --features dynamic-tools`

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::context::notification_channel;
use tower_mcp::extract::{Json, State};
use tower_mcp::{
    CallToolResult, DynamicToolRegistry, JsonRpcRequest, JsonRpcService, McpRouter, ToolBuilder,
};

/// Input for the `create_tool` meta-tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct CreateToolInput {
    /// Name for the new tool
    name: String,
    /// Description for the new tool
    description: String,
    /// Prefix string prepended to messages
    prefix: String,
}

/// Input for the `remove_tool` meta-tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct RemoveToolInput {
    /// Name of the tool to remove
    name: String,
}

/// Input for dynamically created tools.
#[derive(Debug, Deserialize, JsonSchema)]
struct DynamicInput {
    /// The message to process
    message: String,
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter("tower_mcp=debug")
        .init();

    // Set up router with dynamic tool support
    let (router, registry) = McpRouter::new()
        .server_info("dynamic-tools-example", "0.1.0")
        .with_dynamic_tools();

    // Meta-tool: create_tool — registers a new dynamic tool
    let create_tool = ToolBuilder::new("create_tool")
        .description("Create a new tool that prefixes messages")
        .extractor_handler(
            registry.clone(),
            |State(reg): State<DynamicToolRegistry>, Json(input): Json<CreateToolInput>| async move {
                let prefix = input.prefix.clone();
                let tool = ToolBuilder::new(&input.name)
                    .description(&input.description)
                    .handler(move |dinput: DynamicInput| {
                        let prefix = prefix.clone();
                        async move { Ok(CallToolResult::text(format!("{}, {}", prefix, dinput.message))) }
                    })
                    .build();

                reg.register(tool);
                Ok(CallToolResult::text(format!("Created tool '{}'", input.name)))
            },
        )
        .build();

    // Meta-tool: remove_tool — unregisters a dynamic tool
    let remove_tool = ToolBuilder::new("remove_tool")
        .description("Remove a previously created tool")
        .extractor_handler(
            registry.clone(),
            |State(reg): State<DynamicToolRegistry>, Json(input): Json<RemoveToolInput>| async move {
                if reg.unregister(&input.name) {
                    Ok(CallToolResult::text(format!("Removed tool '{}'", input.name)))
                } else {
                    Ok(CallToolResult::text(format!("Tool '{}' not found", input.name)))
                }
            },
        )
        .build();

    // Wire up the notification channel and finalize the router
    let (tx, mut rx) = notification_channel(256);
    let router = router
        .with_notification_sender(tx)
        .tool(create_tool)
        .tool(remove_tool);

    let mut service = JsonRpcService::new(router);

    println!("=== Dynamic Tools Example ===\n");

    // 1. Initialize
    println!("1. Initialize:");
    let resp = service
        .call_single(
            JsonRpcRequest::new(1, "initialize").with_params(serde_json::json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "example-client", "version": "1.0.0" }
            })),
        )
        .await?;
    println!("   {}\n", serde_json::to_string_pretty(&resp)?);

    // 2. List tools — only meta-tools exist
    println!("2. List tools (before creating any):");
    let resp = service
        .call_single(JsonRpcRequest::new(2, "tools/list"))
        .await?;
    println!("   {}\n", serde_json::to_string_pretty(&resp)?);

    // 3. Create a dynamic "greet" tool
    println!("3. Call create_tool to make 'greet':");
    let resp = service
        .call_single(
            JsonRpcRequest::new(3, "tools/call").with_params(serde_json::json!({
                "name": "create_tool",
                "arguments": {
                    "name": "greet",
                    "description": "Greet someone",
                    "prefix": "Hello"
                }
            })),
        )
        .await?;
    println!("   {}\n", serde_json::to_string_pretty(&resp)?);

    // 4. List tools — now includes "greet"
    println!("4. List tools (after creating 'greet'):");
    let resp = service
        .call_single(JsonRpcRequest::new(4, "tools/list"))
        .await?;
    println!("   {}\n", serde_json::to_string_pretty(&resp)?);

    // 5. Call the dynamic "greet" tool
    println!("5. Call dynamic 'greet' tool:");
    let resp = service
        .call_single(
            JsonRpcRequest::new(5, "tools/call").with_params(serde_json::json!({
                "name": "greet",
                "arguments": { "message": "World" }
            })),
        )
        .await?;
    println!("   {}\n", serde_json::to_string_pretty(&resp)?);

    // 6. Remove the "greet" tool
    println!("6. Call remove_tool to remove 'greet':");
    let resp = service
        .call_single(
            JsonRpcRequest::new(6, "tools/call").with_params(serde_json::json!({
                "name": "remove_tool",
                "arguments": { "name": "greet" }
            })),
        )
        .await?;
    println!("   {}\n", serde_json::to_string_pretty(&resp)?);

    // 7. List tools — back to meta-tools only
    println!("7. List tools (after removing 'greet'):");
    let resp = service
        .call_single(JsonRpcRequest::new(7, "tools/list"))
        .await?;
    println!("   {}\n", serde_json::to_string_pretty(&resp)?);

    // 8. Drain and display notifications
    println!("=== Notifications received ===");
    let mut count = 0;
    while let Ok(notification) = rx.try_recv() {
        count += 1;
        println!("   [{count}] {notification:?}");
    }
    if count == 0 {
        println!("   (none)");
    }

    Ok(())
}
