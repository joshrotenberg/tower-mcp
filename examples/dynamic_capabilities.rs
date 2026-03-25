//! Dynamic capabilities -- register tools, prompts, and resources at runtime.
//!
//! This example demonstrates:
//! - `DynamicToolRegistry` for runtime tool registration/removal
//! - `DynamicPromptRegistry` for runtime prompt registration/removal
//! - `DynamicResourceRegistry` for runtime resource registration/removal
//! - Meta-tools (`create_tool`, `remove_tool`) that manage other tools
//! - List-changed notifications when capabilities change
//!
//! Run with: `cargo run --example dynamic_capabilities --features dynamic-tools`

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::context::notification_channel;
use tower_mcp::extract::{Json, State};
use tower_mcp::{
    CallToolResult, DynamicToolRegistry, JsonRpcRequest, JsonRpcService, McpRouter, PromptBuilder,
    ResourceBuilder, ToolBuilder,
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

    // Set up router with dynamic tools, prompts, and resources
    let (router, tool_registry) = McpRouter::new()
        .server_info("dynamic-example", "0.1.0")
        .with_dynamic_tools();
    let (router, prompt_registry) = router.with_dynamic_prompts();
    let (router, resource_registry) = router.with_dynamic_resources();

    // Meta-tool: create_tool -- registers a new dynamic tool
    let create_tool = ToolBuilder::new("create_tool")
        .description("Create a new tool that prefixes messages")
        .extractor_handler(
            tool_registry.clone(),
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

    // Meta-tool: remove_tool -- unregisters a dynamic tool
    let remove_tool = ToolBuilder::new("remove_tool")
        .description("Remove a previously created tool")
        .extractor_handler(
            tool_registry.clone(),
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

    println!("=== Dynamic Registration Example ===\n");

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

    // 2. Register a dynamic prompt
    println!("2. Register dynamic prompt 'code-review':");
    let prompt = PromptBuilder::new("code-review")
        .description("Structured code review guidance")
        .user_message("Review the code for security, performance, and maintainability.");
    prompt_registry.register(prompt);
    println!("   Done.\n");

    // 3. List prompts
    println!("3. List prompts:");
    let resp = service
        .call_single(JsonRpcRequest::new(3, "prompts/list"))
        .await?;
    println!("   {}\n", serde_json::to_string_pretty(&resp)?);

    // 4. Register a dynamic resource
    println!("4. Register dynamic resource 'config://app':");
    let resource = ResourceBuilder::new("config://app")
        .name("App Config")
        .description("Current application configuration")
        .text(r#"{"debug": true, "log_level": "info"}"#);
    resource_registry.register(resource);
    println!("   Done.\n");

    // 5. List resources
    println!("5. List resources:");
    let resp = service
        .call_single(JsonRpcRequest::new(5, "resources/list"))
        .await?;
    println!("   {}\n", serde_json::to_string_pretty(&resp)?);

    // 6. Read the dynamic resource
    println!("6. Read 'config://app':");
    let resp = service
        .call_single(
            JsonRpcRequest::new(6, "resources/read").with_params(serde_json::json!({
                "uri": "config://app"
            })),
        )
        .await?;
    println!("   {}\n", serde_json::to_string_pretty(&resp)?);

    // 7. Create a dynamic tool via meta-tool
    println!("7. Create dynamic tool 'greet':");
    let resp = service
        .call_single(
            JsonRpcRequest::new(7, "tools/call").with_params(serde_json::json!({
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

    // 8. Call the dynamic tool
    println!("8. Call dynamic 'greet' tool:");
    let resp = service
        .call_single(
            JsonRpcRequest::new(8, "tools/call").with_params(serde_json::json!({
                "name": "greet",
                "arguments": { "message": "World" }
            })),
        )
        .await?;
    println!("   {}\n", serde_json::to_string_pretty(&resp)?);

    // 9. Unregister everything
    println!("9. Unregister dynamic capabilities:");
    tool_registry.unregister("greet");
    println!("   Removed tool 'greet'");
    prompt_registry.unregister("code-review");
    println!("   Removed prompt 'code-review'");
    resource_registry.unregister("config://app");
    println!("   Removed resource 'config://app'\n");

    // 10. Drain and display notifications
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
