//! Capability filtering example - name-based access control
//!
//! This example demonstrates how to use capability filtering to control which
//! tools, resources, and prompts are visible based on naming conventions.
//!
//! In this example:
//! - Tools/resources/prompts prefixed with `admin_` are hidden by default
//! - Tools/resources/prompts prefixed with `internal_` are also hidden
//! - All other capabilities are visible
//!
//! This pattern is useful for:
//! - Hiding administrative functions from regular users
//! - Hiding internal/debugging tools from production clients
//! - Creating tiered access based on capability naming conventions
//!
//! Run with: cargo run --example capability_filtering --features http
//!
//! Test with curl:
//! ```bash
//! # Initialize a session
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"1.0"}}}'
//!
//! # List tools (admin_ tools are hidden)
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -H "Mcp-Session-Id: <session-id-from-init>" \
//!   -d '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
//!
//! # Try to call hidden admin tool (returns "method not found")
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -H "Mcp-Session-Id: <session-id-from-init>" \
//!   -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"admin_delete_user","arguments":{"user_id":"123"}}}'
//!
//! # List resources (admin config is hidden)
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -H "Mcp-Session-Id: <session-id-from-init>" \
//!   -d '{"jsonrpc":"2.0","id":4,"method":"resources/list","params":{}}'
//!
//! # List prompts (admin/internal prompts are hidden)
//! curl -X POST http://localhost:3000/ \
//!   -H "Content-Type: application/json" \
//!   -H "Mcp-Session-Id: <session-id-from-init>" \
//!   -d '{"jsonrpc":"2.0","id":5,"method":"prompts/list","params":{}}'
//! ```

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, CapabilityFilter, DenialBehavior, Filterable, HttpTransport, McpRouter, Prompt,
    PromptBuilder, Resource, ResourceBuilder, Tool, ToolBuilder,
};

#[derive(Debug, Deserialize, JsonSchema)]
struct EchoInput {
    message: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UserIdInput {
    user_id: String,
}

/// Filter that hides capabilities with admin_ or internal_ prefix.
/// The session parameter is available for future integration with
/// session-based auth (e.g., checking JWT claims stored in session extensions).
fn is_public_tool(_session: &tower_mcp::SessionState, tool: &Tool) -> bool {
    let name = tool.name();
    !name.starts_with("admin_") && !name.starts_with("internal_")
}

fn is_public_resource(_session: &tower_mcp::SessionState, resource: &Resource) -> bool {
    let name = resource.name();
    !name.starts_with("admin_") && !name.starts_with("internal_")
}

fn is_public_prompt(_session: &tower_mcp::SessionState, prompt: &Prompt) -> bool {
    let name = prompt.name();
    !name.starts_with("admin_") && !name.starts_with("internal_")
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("tower_mcp=debug".parse()?)
                .add_directive("capability_filtering=info".parse()?),
        )
        .init();

    // === PUBLIC TOOLS ===

    let echo = ToolBuilder::new("echo")
        .description("Echo a message back")
        .handler(|input: EchoInput| async move {
            Ok(CallToolResult::text(format!("Echo: {}", input.message)))
        })
        .build()?;

    let get_time = ToolBuilder::new("get_time")
        .description("Get the current server time")
        .handler(|_: tower_mcp::NoParams| async move {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            Ok(CallToolResult::text(format!("Current timestamp: {}", now)))
        })
        .build()?;

    // === ADMIN TOOLS (will be filtered out) ===

    let admin_stats = ToolBuilder::new("admin_get_stats")
        .description("Get system statistics (admin only)")
        .handler(|_: tower_mcp::NoParams| async move {
            Ok(CallToolResult::text(
                "System Stats: CPU: 45%, Memory: 2.1GB, Active Users: 1,234",
            ))
        })
        .build()?;

    let admin_delete = ToolBuilder::new("admin_delete_user")
        .description("Delete a user account (admin only)")
        .handler(|input: UserIdInput| async move {
            Ok(CallToolResult::text(format!(
                "User {} has been deleted",
                input.user_id
            )))
        })
        .build()?;

    // === INTERNAL TOOLS (will be filtered out) ===

    let internal_debug = ToolBuilder::new("internal_debug_info")
        .description("Get internal debug information")
        .handler(|_: tower_mcp::NoParams| async move {
            Ok(CallToolResult::text("Debug: memory_pools=12, gc_runs=847"))
        })
        .build()?;

    // === RESOURCES ===

    let public_docs = ResourceBuilder::new("docs://readme")
        .name("readme")
        .description("Public documentation")
        .text("Welcome to our API! This documentation is publicly available.");

    let api_reference = ResourceBuilder::new("docs://api")
        .name("api_reference")
        .description("API reference documentation")
        .text("# API Reference\n\n## Endpoints\n\n- GET /users\n- POST /users");

    let admin_config = ResourceBuilder::new("config://system")
        .name("admin_system_config")
        .description("System configuration (admin only)")
        .text("database_url: postgres://localhost/prod\napi_secret: sk-xxx");

    let internal_metrics = ResourceBuilder::new("metrics://internal")
        .name("internal_metrics")
        .description("Internal metrics data")
        .text("requests_per_second: 1247\nerror_rate: 0.02%");

    // === PROMPTS ===

    let greeting = PromptBuilder::new("greeting")
        .description("A friendly greeting prompt")
        .user_message("Please greet the user warmly and ask how you can help them today.");

    let code_review = PromptBuilder::new("code_review")
        .description("Code review assistant prompt")
        .required_arg("language", "The programming language")
        .handler(|args| async move {
            let lang = args.get("language").cloned().unwrap_or_default();
            Ok(tower_mcp::GetPromptResult::user_message(format!(
                "Review the following {} code for bugs, security issues, and style.",
                lang
            )))
        });

    let admin_debug = PromptBuilder::new("admin_debug_assistant")
        .description("System debugging prompt (admin only)")
        .user_message(
            "You are a system administrator assistant. Help diagnose issues and analyze logs.",
        );

    let internal_test = PromptBuilder::new("internal_test_prompt")
        .description("Internal testing prompt")
        .user_message("This is an internal test prompt for development use only.");

    // === BUILD ROUTER WITH FILTERS ===

    let mcp_router = McpRouter::new()
        .server_info("capability-filtering-example", "1.0.0")
        .instructions(
            "This server demonstrates capability filtering. Tools, resources, and prompts \
             prefixed with 'admin_' or 'internal_' are hidden from clients.",
        )
        // Public tools
        .tool(echo)
        .tool(get_time)
        // Admin tools (filtered)
        .tool(admin_stats)
        .tool(admin_delete)
        // Internal tools (filtered)
        .tool(internal_debug)
        // Resources
        .resource(public_docs)
        .resource(api_reference)
        .resource(admin_config)
        .resource(internal_metrics)
        // Prompts
        .prompt(greeting)
        .prompt(code_review)
        .prompt(admin_debug)
        .prompt(internal_test)
        // Apply filters
        .tool_filter(
            CapabilityFilter::new(is_public_tool)
                // NotFound (default) - don't reveal that hidden tools exist
                .denial_behavior(DenialBehavior::NotFound),
        )
        .resource_filter(
            CapabilityFilter::new(is_public_resource)
                // Unauthorized - hint that auth might help (useful for debugging)
                .denial_behavior(DenialBehavior::Unauthorized),
        )
        .prompt_filter(CapabilityFilter::new(is_public_prompt));

    // Create HTTP transport
    let transport = HttpTransport::new(mcp_router).disable_origin_validation();

    // Serve
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    tracing::info!("Starting capability filtering example on http://127.0.0.1:3000");
    tracing::info!("");
    tracing::info!("Visible capabilities:");
    tracing::info!("  Tools: echo, get_time");
    tracing::info!("  Resources: readme, api_reference");
    tracing::info!("  Prompts: greeting, code_review");
    tracing::info!("");
    tracing::info!("Hidden capabilities (admin_* and internal_*):");
    tracing::info!("  Tools: admin_get_stats, admin_delete_user, internal_debug_info");
    tracing::info!("  Resources: admin_system_config, internal_metrics");
    tracing::info!("  Prompts: admin_debug_assistant, internal_test_prompt");

    axum::serve(listener, transport.into_router()).await?;

    Ok(())
}
